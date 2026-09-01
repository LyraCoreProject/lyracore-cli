//! Git and Official Package Sources: `lyracore packages add <git-url>` and
//! `lyracore packages update [NAME]`.
//!
//! A Git Package Source is a repository whose ROOT is one Package — the folder holding `src/`
//! and/or `client/` is the repository itself, not a directory inside it.
//!
//! AN INSTALLED PACKAGE IS NOT A WORKING COPY. The clone lands in this checkout's scratch space,
//! and what gets installed is a copy of its tree without the `.git` — the exclusion `tree::collect`
//! already applies to a local folder that happens to be a repository, so the clone needs no special
//! case. `preflight`, `publish` and `client sync` must read a fixed tree; a working copy would give
//! them whatever the last git operation in it left behind, and `packages list` could no longer tell
//! an operator's own edits from a checked-out branch. That is also why `update` re-clones instead
//! of pulling in place: there is nothing in the inventory to pull.
//!
//! `update` therefore advances a RECORDED commit rather than a working copy: the stamp says which
//! repository and which commit the installed tree came from, and the update replaces the whole
//! folder with the repository's current one. The old folder is kept until the new revision is
//! installed and preflight is green, so every failure is recoverable.
//!
//! Neither verb publishes anything or synchronizes a client.

use super::stamp::{
    self, ProvenanceStamp, SOURCE_GIT, SOURCE_LOCAL, SOURCE_OFFICIAL, SOURCE_SCAFFOLD,
};
use super::{
    confirm, find, install_tree, inventory, review::TrustReview, shell_quote, InstalledPackage,
    Origin, PackageName, PackageState,
};
use crate::cmd::import::Prompt;
use crate::cmd::preflight;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Where clones are made, and where the previous revision waits during an update. Both are scratch
/// space under `.lyracore/`, never an inventory: nothing the server build can see.
const CLONE_DIR: &str = "package-clones";
const PRESERVED_DIR: &str = "package-updates";

/// The URL of a repository whose root is one Package.
pub(crate) struct GitSource {
    url: String,
}

impl GitSource {
    /// The argument spellings `packages add` reads as a repository rather than a path: the URL
    /// schemes git speaks, and the scp-style `user@host:path` form. Everything else is a path on
    /// this machine, or a bare Package name for `add` to resolve against the Official Package
    /// Collection — see [`official`](super::official).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let url = raw.trim();
        let is_url = ["https://", "http://", "ssh://", "git://"]
            .iter()
            .any(|scheme| url.starts_with(scheme))
            || is_scp_style(url);
        is_url.then(|| Self {
            url: url.to_string(),
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The Package name the repository carries: its last path segment without the `.git` suffix.
    ///
    /// A repository's own name is the only name available — there is no argument to override it
    /// with — so a repository named something the server build would refuse is refused here, with
    /// the URL in the message rather than just the name taken out of it.
    fn name(&self) -> Result<PackageName> {
        let last = self
            .url
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or_default();
        let stem = last.strip_suffix(".git").unwrap_or(last);
        PackageName::parse(stem).map_err(|error| {
            Error::Usage(format!(
                "cannot install {}: a Package takes the name of the repository it comes from, and \
                 '{stem}' is not a usable one. {error}",
                self.url
            ))
        })
    }
}

/// `user@host:path`, git's scp-style remote. The colon is what makes it a remote rather than a
/// relative path, and the `@` before it is what makes it a remote rather than a Windows drive or a
/// folder with a colon in its name.
fn is_scp_style(url: &str) -> bool {
    if url.contains("://") || url.starts_with('/') || url.starts_with('.') || url.starts_with('~') {
        return false;
    }
    let Some((location, path)) = url.split_once(':') else {
        return false;
    };
    let Some((user, host)) = location.split_once('@') else {
        return false;
    };
    !user.is_empty() && !host.is_empty() && !host.contains('/') && !path.is_empty()
}

/// A repository cloned into this checkout's scratch space, at the commit its default branch points
/// at right now. Dropping it removes the clone.
///
/// `pub(crate)`: [`official`](super::official) clones the Official Package Collection through the
/// same machinery, rather than a second copy of it, so a clone behaves identically (depth-1,
/// `GIT_TERMINAL_PROMPT` disabled, scratch space cleaned up) whichever the two commands runs it.
pub(crate) struct RepositoryClone {
    dir: PathBuf,
    revision: String,
}

impl RepositoryClone {
    pub(crate) fn fetch(
        project: &ProjectLayout,
        runner: &dyn ProcessRunner,
        source: &GitSource,
    ) -> Result<Self> {
        let root = project.state_dir.join(CLONE_DIR);
        std::fs::create_dir_all(&root)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        // Held from here on, so a clone that fails halfway leaves no scratch directory behind.
        let mut clone = Self {
            dir: root.join(format!("{}-{nonce}", std::process::id())),
            revision: String::new(),
        };

        println!("· cloning {}", source.url());
        runner.run_and_wait(
            &CommandSpec::new("git")
                .arg("clone")
                // One commit is all an install records and all it copies. History would be fetched
                // only to be deleted with `.git` a moment later.
                .arg("--depth")
                .arg("1")
                // A repository that needs credentials must FAIL here rather than sit on a hidden
                // prompt inside a command the operator may have scripted with --yes.
                .env("GIT_TERMINAL_PROMPT", "0")
                .arg("--")
                .arg(source.url())
                .arg(clone.dir.to_string_lossy().to_string()),
        )?;
        if !clone.dir.is_dir() {
            return Err(Error::Process(format!(
                "`git clone` reported success but left no working copy at {}. Nothing was \
                 installed.",
                clone.dir.display()
            )));
        }

        let revision = runner
            .run_and_wait(
                &CommandSpec::new("git")
                    .cwd(clone.dir.clone())
                    .arg("rev-parse")
                    .arg("HEAD"),
            )?
            .trim()
            .to_string();
        if revision.is_empty() {
            return Err(Error::Process(format!(
                "cloned {} but could not resolve the commit it is at (`git rev-parse HEAD` said \
                 nothing). Nothing was installed.",
                source.url()
            )));
        }
        clone.revision = revision;
        Ok(clone)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn revision(&self) -> &str {
        &self.revision
    }
}

impl Drop for RepositoryClone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Install a Package from a Git Package Source: clone it, then hand the clone to the same
/// [`install`](super::install) a local folder goes through, recording the exact commit.
pub(crate) fn add(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    source: &GitSource,
    yes: bool,
) -> Result<()> {
    let name = source.name()?;
    // Before the network: an install the inventory would refuse anyway is not worth a clone.
    super::check_collision(project, &name)?;

    let clone = RepositoryClone::fetch(project, runner, source)?;
    super::install(
        project,
        runner,
        prompt,
        clone.path(),
        &name,
        &Origin::Git {
            url: source.url(),
            revision: clone.revision(),
        },
        yes,
    )
}

/// Advance one named Package with a Git Package Source or Official Package Source, or every one of
/// them, to its Package Source's current commit.
///
/// With no name this walks BOTH inventories: a disabled Package is still installed and still came
/// from somewhere, and leaving it behind would mean re-enabling it later brought back a revision
/// the operator thought they had updated.
pub fn update(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    name: Option<&str>,
    yes: bool,
) -> Result<()> {
    let targets = match name {
        Some(raw) => vec![find(project, &PackageName::parse(raw)?)?],
        None => inventory(project)?
            .into_iter()
            .filter(|package| {
                package.stamp.as_ref().is_some_and(|recorded| {
                    matches!(recorded.source_kind.as_str(), SOURCE_GIT | SOURCE_OFFICIAL)
                })
            })
            .collect(),
    };
    if targets.is_empty() {
        println!("no Packages from a Git Package Source or Official Package Source are installed, so there is nothing to update.");
        println!(
            "`lyracore packages add <git-url>` installs one from a repository whose root is a \
             Package, and `lyracore packages add <name>` installs one from the Official Package \
             Collection. `lyracore packages list` shows where each installed Package came from."
        );
        return Ok(());
    }

    let mut advanced = 0;
    for package in &targets {
        if targets.len() > 1 {
            println!();
            println!("== {} ==", package.name.as_str());
        }
        match update_one(project, runner, prompt, package, yes) {
            Ok(true) => advanced += 1,
            Ok(false) => {}
            // Stop at the first failure. After a refused or failed update the checkout needs
            // attention, and updating more Packages on top of it would only make the state harder
            // to read.
            Err(error) => {
                if advanced > 0 {
                    println!();
                    println!(
                        "stopping at '{}'. The {advanced} Package(s) updated before it are \
                         installed and preflighted; they are not rolled back.",
                        package.name.as_str()
                    );
                }
                return Err(error);
            }
        }
    }

    if targets.len() > 1 {
        println!();
        println!("{advanced} of {} Package(s) advanced.", targets.len());
    }
    Ok(())
}

/// Update one Package in place, in whichever inventory it sits. `true` if it advanced.
///
/// This walks the same steps as an install, in the same order, and shares the tree install, the
/// stamp and the consent gate with it. It does NOT share the whole of [`install`](super::install):
/// the destination is occupied rather than free, the question names two commits rather than one,
/// and a failure restores instead of leaving the copy to be fixed in place. Folding the two into
/// one function would mean a policy argument for each of those differences.
fn update_one(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    package: &InstalledPackage,
    yes: bool,
) -> Result<bool> {
    let name = package.name.as_str();
    let (recorded, source) = update_source(package)?;
    refuse_dirty(package, recorded)?;

    let clone = source.fetch(project, runner)?;
    if clone.revision() == recorded.revision {
        println!(
            "'{name}' is already at {} — the repository has nothing newer.",
            short(clone.revision())
        );
        return Ok(false);
    }

    // Whatever refuses the candidate has to name the two commits itself. The clone is scratch
    // space this function deletes on its way out, so an error naming only a path inside it would
    // name a directory that no longer exists by the time the operator reads it.
    let candidate = |error: Error| {
        Error::Usage(format!(
            "cannot update '{name}' from {} to {}: the revision at {} is not installable. '{name}' \
             is untouched at {}, still at {}.\n  ({error})",
            short(&recorded.revision),
            short(clone.revision()),
            source.url(),
            package.dir.display(),
            short(&recorded.revision)
        ))
    };
    let candidate_path = source
        .candidate_path(&clone, &package.name)
        .map_err(&candidate)?;
    super::validate_shape(&candidate_path).map_err(&candidate)?;
    let review = TrustReview::scan(&candidate_path).map_err(&candidate)?;
    // Computed before the question, so the tree the operator was shown is the tree that is
    // installed if they say yes.
    let reviewed_identity = stamp::content_identity(&candidate_path).map_err(&candidate)?;
    println!();
    print!("{}", review.render(&candidate_path));
    println!();
    println!("  source    {} {}", source.kind(), source.url());
    println!("  from      {}", recorded.revision);
    println!("  to        {}", clone.revision());
    println!();

    confirm(
        prompt,
        &format!(
            "Replace '{name}' at {} with {} of {}?",
            package.dir.display(),
            short(clone.revision()),
            source.url()
        ),
        &format!(
            "Nothing was changed; '{name}' is still at {}.",
            short(&recorded.revision)
        ),
        yes,
    )?;
    // Re-taken after the network and after the question: an edit made while the clone ran or while
    // the operator read the prompt is work this replace would destroy, and the refusal above
    // promises it never does.
    refuse_dirty(package, recorded)?;

    let origin = source.origin(clone.revision());
    let compiled_in = package.state == PackageState::Enabled;
    let preserved = PreservedPackage::move_aside(project, package)?;
    let installed = install_tree(
        project,
        &package.name,
        &candidate_path,
        &reviewed_identity,
        &origin,
        &package.dir,
    );
    // Whether the candidate reached the destination decides what `restore` may delete: a
    // destination it did not put there belongs to another process.
    let landed = installed.is_ok();
    let outcome = installed.and_then(|identity| {
        println!();
        println!("updated {name} -> {}", package.dir.display());
        print!("{}", origin.report());
        println!("  identity  {identity}");
        println!();
        if compiled_in {
            println!("running preflight with the new revision compiled in");
        } else {
            println!(
                "running preflight ('{name}' is disabled, so the module does not compile it in)"
            );
        }
        preflight::run(project, runner)
    });
    if let Err(cause) = outcome {
        return Err(preserved.restore(
            package,
            &recorded.revision,
            clone.revision(),
            cause,
            landed,
        ));
    }
    preserved.discard();

    println!();
    if compiled_in {
        println!(
            "'{name}' is at {} and preflight is green. Two steps remain, and this command runs \
             neither:",
            short(clone.revision())
        );
        println!("  lyracore publish       compile the new revision into the module and publish it to every database");
    } else {
        println!(
            "'{name}' is at {} and preflight is green. It is disabled, so the module still does \
             not compile it. Two steps remain, and this command runs neither:",
            short(clone.revision())
        );
        println!("  lyracore packages enable {name}");
        println!("  lyracore publish       compile the new revision into the module and publish it to every database");
    }
    if review.addons.is_empty() && review.client_overrides == 0 {
        println!("  lyracore client sync   not needed: this Package ships no client content");
    } else {
        println!(
            "  lyracore client sync   install its {} addon(s) and {} client override(s) into your client",
            review.addons.len(),
            review.client_overrides
        );
    }
    Ok(true)
}

/// The candidate source an installed Package records.
///
/// A Git Package Source uses the clone root. An Official Package Source resolves the installed
/// Package name under the cloned collection. Both then enter the same update contract below.
enum UpdateSource {
    Git(GitSource),
    Official,
}

impl UpdateSource {
    fn fetch(
        &self,
        project: &ProjectLayout,
        runner: &dyn ProcessRunner,
    ) -> Result<RepositoryClone> {
        match self {
            Self::Git(source) => RepositoryClone::fetch(project, runner, source),
            Self::Official => {
                let collection = super::official::collection_source();
                RepositoryClone::fetch(project, runner, &collection)
            }
        }
    }

    fn candidate_path(&self, clone: &RepositoryClone, name: &PackageName) -> Result<PathBuf> {
        match self {
            Self::Git(_) => Ok(clone.path().to_path_buf()),
            Self::Official => super::official::resolve(clone.path(), name),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Git(_) => SOURCE_GIT,
            Self::Official => SOURCE_OFFICIAL,
        }
    }

    fn url(&self) -> &str {
        match self {
            Self::Git(source) => source.url(),
            Self::Official => super::official::COLLECTION_URL,
        }
    }

    fn origin<'a>(&'a self, revision: &'a str) -> Origin<'a> {
        match self {
            Self::Git(source) => Origin::Git {
                url: source.url(),
                revision,
            },
            Self::Official => Origin::Official { revision },
        }
    }
}

/// The recorded update source, or a refusal naming what the Package actually is.
///
/// A local folder and a scaffold have no remote tree to advance from. The Official Package
/// Collection is one fixed collection, so an edited collection URL does not turn an official stamp
/// into a general registry entry.
fn update_source(package: &InstalledPackage) -> Result<(&ProvenanceStamp, UpdateSource)> {
    let name = package.name.as_str();
    let Some(recorded) = package.stamp.as_ref() else {
        return Err(Error::Usage(format!(
            "cannot update '{name}': it has no readable provenance stamp, so nothing records a \
             repository to update it from. It was created by hand, predates `lyracore packages \
             add`, or its stamp was edited. Nothing was changed."
        )));
    };
    match recorded.source_kind.as_str() {
        SOURCE_GIT if recorded.source.is_empty() || recorded.revision.is_empty() => {
            Err(Error::Usage(format!(
                "cannot update '{name}': its stamp says it came from a repository but records no \
                 {}. Nothing was changed.",
                if recorded.source.is_empty() {
                    "URL"
                } else {
                    "commit to advance from"
                }
            )))
        }
        SOURCE_GIT => {
            let source = GitSource::parse(&recorded.source).ok_or_else(|| {
                Error::Usage(format!(
                    "cannot update '{name}': its stamp records the Package Source '{}', which is \
                     not a repository URL this command can clone. Nothing was changed.",
                    recorded.source
                ))
            })?;
            Ok((recorded, UpdateSource::Git(source)))
        }
        SOURCE_LOCAL => Err(Error::Usage(format!(
            "cannot update '{name}': it was installed from a folder on this machine ({}), not from \
             a repository, so there is no newer revision to fetch. Copy a newer version in by hand, \
             or disable, remove and add the folder again. Nothing was changed.",
            recorded.source
        ))),
        SOURCE_SCAFFOLD => Err(Error::Usage(format!(
            "cannot update '{name}': it was scaffolded from this checkout's reference Package and \
             has no Package Source. It is yours to edit in place. Nothing was changed."
        ))),
        SOURCE_OFFICIAL if recorded.source != super::official::COLLECTION_URL => {
            Err(Error::Usage(format!(
                "cannot update '{name}': its Official Package Source is '{}', not the Official \
                 Package Collection ({}). Nothing was changed.",
                recorded.source,
                super::official::COLLECTION_URL
            )))
        }
        SOURCE_OFFICIAL if recorded.revision.is_empty() => Err(Error::Usage(format!(
            "cannot update '{name}': its stamp says it came from the Official Package Collection \
             but records no commit to advance from. Nothing was changed."
        ))),
        SOURCE_OFFICIAL => Ok((recorded, UpdateSource::Official)),
        "" => Err(Error::Usage(format!(
            "cannot update '{name}': its stamp records no Package Source kind, so nothing says \
             where it came from. Nothing was changed."
        ))),
        other => Err(Error::Usage(format!(
            "cannot update '{name}': its Package Source kind is '{other}', and this command only \
             advances Git or Official Package Sources ('{SOURCE_GIT}' or '{SOURCE_OFFICIAL}'). \
             Nothing was changed."
        ))),
    }
}

/// Refuse a Package whose tree is not the one its stamp recorded.
///
/// An update REPLACES the whole folder, so it may only run when every byte in it is recorded
/// somewhere else. This is `remove`'s rule for the same reason: local edits exist nowhere but here,
/// and neither command can get them back.
fn refuse_dirty(package: &InstalledPackage, recorded: &ProvenanceStamp) -> Result<()> {
    let name = package.name.as_str();
    if recorded.content_identity.is_empty() {
        return Err(Error::Usage(format!(
            "cannot update '{name}': its stamp records no content identity, so this command cannot \
             tell installed content from local work. Nothing was changed."
        )));
    }
    let current = stamp::content_identity(&package.dir)?;
    if current != recorded.content_identity {
        return Err(Error::Usage(format!(
            "cannot update '{name}': the folder no longer matches what was installed (stamp {}, on \
             disk {current}). An update replaces the whole folder, and those changes exist only \
             here. Copy them somewhere outside the checkout first, then update. Nothing was \
             changed, and nothing was discarded.",
            recorded.content_identity
        )));
    }
    Ok(())
}

/// The Package as it was before an update, moved out of its inventory but kept on disk until the
/// new revision is installed AND preflight is green.
///
/// The failure that matters during an update is the one that happens with the old folder already
/// gone. Keeping it makes every one of them recoverable: the old revision goes back where it was,
/// and the operator is where they started.
struct PreservedPackage {
    aside: PathBuf,
    original: PathBuf,
    settled: bool,
}

impl PreservedPackage {
    fn move_aside(project: &ProjectLayout, package: &InstalledPackage) -> Result<Self> {
        let root = project.state_dir.join(PRESERVED_DIR);
        std::fs::create_dir_all(&root)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let aside = root.join(format!(
            "{}-{}-{nonce}",
            package.name.as_str(),
            std::process::id()
        ));
        // A rename, not a copy: the inventory must never hold two versions of one Package, not even
        // for the length of a copy the build could start reading halfway through.
        std::fs::rename(&package.dir, &aside).map_err(|error| {
            Error::Process(format!(
                "could not move '{}' aside before updating it: {error}. Nothing was changed.",
                package.name.as_str()
            ))
        })?;
        Ok(Self {
            aside,
            original: package.dir.clone(),
            settled: false,
        })
    }

    /// The update landed: the previous revision is no longer needed.
    fn discard(mut self) {
        self.settled = true;
        let _ = std::fs::remove_dir_all(&self.aside);
    }

    /// Put the previous revision back and explain what failed, naming both commits.
    ///
    /// `landed` says whether the candidate reached the destination. Only then is the destination
    /// OURS to delete. This command leaves the Package's name free for as long as the swap takes,
    /// so a directory that appeared while it was gone belongs to another process, and removing it
    /// would be the merge-or-overwrite every other path here refuses.
    ///
    /// A candidate that did land is deleted rather than left half-installed. It is one clone away
    /// from being back, and a folder the operator did not choose to keep must not be what the next
    /// `preflight` or `publish` compiles.
    fn restore(
        mut self,
        package: &InstalledPackage,
        from: &str,
        to: &str,
        cause: Error,
        landed: bool,
    ) -> Error {
        self.settled = true;
        let name = package.name.as_str();
        let (from, to) = (short(from), short(to));
        if landed {
            if let Err(error) = std::fs::remove_dir_all(&self.original) {
                return Error::Process(format!(
                    "updating '{name}' from {from} to {to} failed ({cause}), and the candidate at \
                     {} could not be removed either ({error}). The previous revision is preserved \
                     at {}. Put it back by hand once {} is gone.",
                    self.original.display(),
                    self.aside.display(),
                    shell_quote(&self.original)
                ));
            }
        } else if self.original.exists() {
            return Error::Process(format!(
                "updating '{name}' from {from} to {to} failed ({cause}), and {} now holds a \
                 Package this command did not put there. Nothing was merged or overwritten, and \
                 the previous revision is preserved untouched at {}. Decide which one to keep, \
                 then move it back by hand.",
                self.original.display(),
                self.aside.display()
            ));
        }
        if let Err(error) = std::fs::rename(&self.aside, &self.original) {
            return Error::Process(format!(
                "updating '{name}' from {from} to {to} failed ({cause}), and the previous revision \
                 could not be moved back to {} ({error}). It is intact at {}. Move it back by \
                 hand.",
                self.original.display(),
                self.aside.display()
            ));
        }
        Error::Process(format!(
            "updating '{name}' from {from} to {to} failed, so nothing was published and the module \
             on the node is unchanged.\n  The previous revision is back at {}, byte for byte, and \
             the candidate was discarded.\n  The failure below may be in the new revision or in \
             another preflight prerequisite; read it before re-running `lyracore packages update \
             {name}`.\n  ({cause})",
            self.original.display()
        ))
    }
}

impl Drop for PreservedPackage {
    fn drop(&mut self) {
        if !self.settled {
            // Only reachable if the update panicked between the two moves. Say where the folder is
            // rather than deleting it: it is the only copy of the previous revision.
            eprintln!(
                "the previous revision of this Package is preserved at {}; move it back to {}",
                self.aside.display(),
                self.original.display()
            );
        }
    }
}

/// A commit, shortened for prose. The conventional 7 characters, or the whole thing if it is
/// shorter.
///
/// Counted in CHARACTERS, not bytes. A recorded revision is read back out of a stamp file this
/// CLI's own documentation calls operator-editable, so it is untrusted text: slicing it by byte
/// offset would turn a hand-edited stamp into a panic instead of the refusal it should get.
fn short(revision: &str) -> &str {
    match revision.char_indices().nth(7) {
        Some((boundary, _)) => &revision[..boundary],
        None => revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::packages::tests::{candidate, checkout, Answer};
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const URL: &str = "https://example.invalid/greeter.git";
    const FIRST: &str = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b";
    const SECOND: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c";

    /// A machine whose `git clone` produces `tree` and whose `git rev-parse HEAD` answers
    /// `revision`: one repository, at one commit.
    fn repository(tree: &Path, revision: &str) -> FakeStack {
        FakeStack::new()
            .with_git_clone(tree)
            .with_stdout("rev-parse HEAD", &format!("{revision}\n"))
    }

    /// Install `URL` at `revision` from `tree`, the way an operator would.
    fn install(project: &ProjectLayout, stack: &FakeStack) {
        super::super::add(project, &stack.runner(), &Answer("yes"), URL, true).unwrap();
    }

    /// A Package in the enabled inventory carrying a hand-written stamp, for the Package Source
    /// kinds no install in this checkout can produce.
    fn stamped(project: &ProjectLayout, name: &str, kind: &str, source: &str) {
        let dir = project.packages_dir().join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/mod.rs"), "pub fn helper() {}\n").unwrap();
        ProvenanceStamp {
            source_kind: kind.to_string(),
            source: source.to_string(),
            revision: String::new(),
            content_identity: stamp::content_identity(&dir).unwrap(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
        }
        .write(&dir)
        .unwrap();
    }

    // ---- what counts as a Git Package Source ----

    #[test]
    fn a_url_is_a_package_source_and_everything_else_stays_a_path() {
        for url in [
            "https://example.invalid/greeter.git",
            "http://example.invalid/greeter.git",
            "ssh://git@example.invalid/team/greeter.git",
            "git://example.invalid/greeter",
            "git@example.invalid:team/greeter.git",
        ] {
            assert!(GitSource::parse(url).is_some(), "{url}");
        }
        // A path is a path, including one with a colon or an '@' in it, and including a bare name:
        // `add` resolves a bare name against the Official Package Collection instead, and reading
        // one as a URL here would take that decision away from it.
        for path in [
            "/home/dev/src/greeter",
            "./greeter",
            "~/src/greeter",
            "greeter",
            "/home/dev@work/greeter",
            "src/a:b",
        ] {
            assert!(GitSource::parse(path).is_none(), "{path}");
        }
    }

    #[test]
    fn a_package_takes_the_name_of_the_repository_it_comes_from() {
        for (url, name) in [
            ("https://example.invalid/greeter.git", "greeter"),
            ("https://example.invalid/team/my-package", "my-package"),
            ("https://example.invalid/greeter/", "greeter"),
            ("git@example.invalid:greeter.git", "greeter"),
        ] {
            assert_eq!(
                GitSource::parse(url).unwrap().name().unwrap().as_str(),
                name,
                "{url}"
            );
        }

        // The repository's name is the only name available, so one the server build would refuse
        // has to be refused here — with the URL in the message, not just the name taken out of it.
        let error = GitSource::parse("https://example.invalid/2fast.git")
            .unwrap()
            .name()
            .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("2fast.git"), "{error}");
    }

    // ---- `packages add <git-url>` ----

    #[test]
    fn a_repository_root_installs_as_a_package_and_records_its_exact_commit() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&candidate(&tmp, "anything"), FIRST);

        install(&project, &stack);

        let installed = project.packages_dir().join("greeter");
        assert!(installed.join("src/mod.rs").is_file());
        let recorded = ProvenanceStamp::read(&installed).expect("no provenance stamp");
        assert_eq!(recorded.source_kind, SOURCE_GIT);
        assert_eq!(recorded.source, URL);
        assert_eq!(recorded.revision, FIRST);
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the stamp must record the identity of what was actually copied"
        );
    }

    #[test]
    fn the_recorded_commit_is_what_list_and_the_lifecycle_verbs_report() {
        // The commit is the one thing a Git-backed Package has that a local one does not. A report
        // that left it out would leave an operator no way to see which revision is installed.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        install(&project, &repository(&candidate(&tmp, "anything"), FIRST));

        let report = super::super::provenance_report(
            ProvenanceStamp::read(&project.packages_dir().join("greeter")).as_ref(),
        );

        assert!(report.contains(&format!("revision  {FIRST}")), "{report}");
        assert!(
            report.contains(&format!("source    {SOURCE_GIT} {URL}")),
            "{report}"
        );
        super::super::list(&project).unwrap();
    }

    #[test]
    fn what_is_installed_is_a_tree_and_not_a_working_copy() {
        // An installed Package with a .git would give `preflight`, `publish` and `client sync`
        // whatever the last git operation in it left behind, and `packages list` could no longer
        // tell an operator's edits from a checked-out branch. The fake clone writes a .git for
        // exactly this assertion to have something to catch.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&candidate(&tmp, "anything"), FIRST);

        install(&project, &stack);

        assert!(!project.packages_dir().join("greeter/.git").exists());
        let clones = project.state_dir.join(CLONE_DIR);
        assert!(
            !clones.exists() || std::fs::read_dir(clones).unwrap().next().is_none(),
            "the clone is scratch space and must not survive the install"
        );
    }

    #[test]
    fn a_repository_whose_root_is_not_a_package_installs_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let empty = tmp.path().join("sources/not-a-package");
        std::fs::create_dir_all(empty.join("docs")).unwrap();
        std::fs::write(empty.join("docs/readme.md"), "# nothing here\n").unwrap();
        let stack = repository(&empty, FIRST);

        let error =
            super::super::add(&project, &stack.runner(), &Answer("yes"), URL, true).unwrap_err();

        assert!(error.to_string().contains("neither src/"), "{error}");
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    // ---- `packages update` ----

    #[test]
    fn an_update_advances_the_package_to_the_new_commit_and_records_it() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));

        // The repository moves on.
        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();
        let stack = repository(&tree, SECOND);
        update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap();

        let installed = project.packages_dir().join("greeter");
        assert_eq!(
            std::fs::read_to_string(installed.join("src/mod.rs")).unwrap(),
            "pub fn greet_v2() {}\n"
        );
        let recorded = ProvenanceStamp::read(&installed).unwrap();
        assert_eq!(recorded.revision, SECOND);
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the updated Package must not read as locally drifted"
        );
        let preserved = project.state_dir.join(PRESERVED_DIR);
        assert!(
            !preserved.exists() || std::fs::read_dir(preserved).unwrap().next().is_none(),
            "a landed update keeps no copy of the previous revision"
        );
    }

    #[test]
    fn a_repository_with_nothing_newer_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let before = ProvenanceStamp::read(&project.packages_dir().join("greeter")).unwrap();

        let stack = repository(&tree, FIRST);
        update(
            &project,
            &stack.runner(),
            &Answer("no"),
            Some("greeter"),
            false,
        )
        .unwrap();

        assert_eq!(
            ProvenanceStamp::read(&project.packages_dir().join("greeter")),
            Some(before),
            "an update with nothing to do must not restamp the Package"
        );
        // Not even the question was asked: `Answer("no")` would have refused it.
        for call in stack.rendered() {
            assert!(!call.contains("cargo check"), "{call}");
        }
    }

    #[test]
    fn a_locally_changed_package_is_refused_without_discarding_the_changes() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let installed = project.packages_dir().join("greeter");
        std::fs::write(installed.join("src/mod.rs"), "// hours of local work\n").unwrap();

        // `--yes`, so a refusal cannot be the prompt's doing: the drift check is what stops it.
        let stack = repository(&tree, SECOND);
        let error = update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("no longer matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(installed.join("src/mod.rs")).unwrap(),
            "// hours of local work\n"
        );
    }

    #[test]
    fn an_answer_other_than_yes_leaves_the_installed_revision_in_place() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();

        let stack = repository(&tree, SECOND);
        let error = update(
            &project,
            &stack.runner(),
            &Answer("no"),
            Some("greeter"),
            false,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains(short(FIRST)), "{error}");
        let installed = project.packages_dir().join("greeter");
        assert!(std::fs::read_to_string(installed.join("src/mod.rs"))
            .unwrap()
            .contains("game_hook"));
        assert_eq!(
            ProvenanceStamp::read(&installed).unwrap().revision,
            FIRST,
            "{error}"
        );
    }

    #[test]
    fn a_failed_preflight_restores_the_previous_revision_and_names_both_commits() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let installed = project.packages_dir().join("greeter");
        let before = stamp::content_identity(&installed).unwrap();
        std::fs::write(tree.join("src/mod.rs"), "pub fn does_not_compile() {\n").unwrap();

        let stack = repository(&tree, SECOND).fail_on("cargo check", "the new revision is broken");
        let error = update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains(short(FIRST)), "{error}");
        assert!(error.to_string().contains(short(SECOND)), "{error}");
        assert_eq!(
            stamp::content_identity(&installed).unwrap(),
            before,
            "the previous revision must come back byte for byte: {error}"
        );
        assert_eq!(ProvenanceStamp::read(&installed).unwrap().revision, FIRST);
        let preserved = project.state_dir.join(PRESERVED_DIR);
        assert!(
            !preserved.exists() || std::fs::read_dir(preserved).unwrap().next().is_none(),
            "a restored update leaves no second copy behind: {error}"
        );
    }

    // ---- which Packages `update` will act on ----

    #[test]
    fn a_package_without_an_update_source_is_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();
        let folder = candidate(&tmp, "keeper");
        super::super::add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            folder.to_str().unwrap(),
            true,
        )
        .unwrap();
        stamped(&project, "scaffolded", SOURCE_SCAFFOLD, "packages/example/");
        stamped(&project, "kindless", "", "");
        stamped(&project, "urlless", SOURCE_GIT, "");
        stamped(&project, "not-a-url", SOURCE_GIT, "/home/dev/greeter");
        stamped(&project, "commitless", SOURCE_GIT, URL);
        // `stamped` records no revision, so `commitless` is the git stamp that never got one. The
        // other two need one to reach the checks past it.
        for name in ["not-a-url", "urlless"] {
            let dir = project.packages_dir().join(name);
            let mut broken = ProvenanceStamp::read(&dir).unwrap();
            broken.revision = FIRST.to_string();
            broken.write(&dir).unwrap();
        }
        std::fs::create_dir_all(project.packages_dir().join("handmade/src")).unwrap();

        let refusal = |name: &str| {
            update(&project, &stack.runner(), &Answer("yes"), Some(name), true)
                .unwrap_err()
                .to_string()
        };

        assert!(refusal("keeper").contains("a folder on this machine"));
        assert!(refusal("scaffolded").contains("scaffolded"));
        assert!(refusal("handmade").contains("no readable provenance stamp"));
        assert!(refusal("kindless").contains("no Package Source kind"));
        // A stamp that claims a repository but does not name one, or names something that is not a
        // URL, is broken rather than local. Each says which half is missing.
        assert!(refusal("urlless").contains("records no URL"));
        assert!(refusal("commitless").contains("no commit to advance from"));
        assert!(refusal("not-a-url").contains("not a repository URL"));
        // Every one of them refused before any repository was contacted.
        for call in stack.rendered() {
            assert!(!call.contains("git clone"), "{call}");
        }
    }

    #[test]
    fn updating_everything_advances_git_backed_packages_and_leaves_local_packages_alone() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let folder = candidate(&tmp, "keeper");
        super::super::add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            folder.to_str().unwrap(),
            true,
        )
        .unwrap();
        let keeper = ProvenanceStamp::read(&project.packages_dir().join("keeper")).unwrap();

        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();
        let stack = repository(&tree, SECOND);
        update(&project, &stack.runner(), &Answer("yes"), None, true).unwrap();

        assert_eq!(
            ProvenanceStamp::read(&project.packages_dir().join("greeter"))
                .unwrap()
                .revision,
            SECOND
        );
        assert_eq!(
            ProvenanceStamp::read(&project.packages_dir().join("keeper")),
            Some(keeper),
            "a Package installed from a folder is not part of an unnamed update"
        );
    }

    #[test]
    fn a_checkout_with_no_updateable_packages_has_nothing_to_update() {
        // Not an error: "there is nothing to do" is the honest answer, and an exit code would make
        // `packages update` unusable in a script that does not know what is installed.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();
        stamped(&project, "scaffolded", SOURCE_SCAFFOLD, "packages/example/");

        update(&project, &stack.runner(), &Answer("yes"), None, true).unwrap();

        assert!(project
            .packages_dir()
            .join("scaffolded/src/mod.rs")
            .is_file());
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn a_disabled_git_backed_package_is_updated_where_it_sits() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        super::super::lifecycle::disable(&project, "greeter").unwrap();

        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();
        let stack = repository(&tree, SECOND);
        update(&project, &stack.runner(), &Answer("yes"), None, true).unwrap();

        let disabled = project.packages_disabled_dir().join("greeter");
        assert!(!project.packages_dir().join("greeter").exists());
        assert_eq!(ProvenanceStamp::read(&disabled).unwrap().revision, SECOND);
        assert_eq!(
            std::fs::read_to_string(disabled.join("src/mod.rs")).unwrap(),
            "pub fn greet_v2() {}\n"
        );
    }

    #[test]
    fn neither_add_nor_update_publishes_or_synchronizes_a_client() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        let stack = repository(&tree, FIRST);
        install(&project, &stack);

        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();
        let stack = stack.with_stdout("rev-parse HEAD", &format!("{SECOND}\n"));
        update(&project, &stack.runner(), &Answer("yes"), None, true).unwrap();

        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("--pack-client"), "{call}");
        }
    }

    #[test]
    fn a_candidate_that_is_not_installable_names_both_revisions_and_changes_nothing() {
        // The clone is scratch space this command deletes on its way out, so a refusal that named
        // only a path inside it would name a directory the operator can no longer look at.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let installed = project.packages_dir().join("greeter");
        let before = stamp::content_identity(&installed).unwrap();
        // The repository's next commit is not a Package at all.
        std::fs::remove_dir_all(tree.join("src")).unwrap();
        std::fs::create_dir_all(tree.join("docs")).unwrap();
        std::fs::write(tree.join("docs/readme.md"), "# moved elsewhere\n").unwrap();

        let stack = repository(&tree, SECOND);
        let error = update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains(short(FIRST)), "{error}");
        assert!(error.to_string().contains(short(SECOND)), "{error}");
        assert!(error.to_string().contains(URL), "{error}");
        assert_eq!(stamp::content_identity(&installed).unwrap(), before);
        assert_eq!(ProvenanceStamp::read(&installed).unwrap().revision, FIRST);
    }

    /// A prompt that edits the installed Package while the question is on screen, standing in for
    /// an operator who saves a file in another window before answering.
    struct EditWhileAsking {
        file: PathBuf,
    }

    impl Prompt for EditWhileAsking {
        fn ask(&self, _question: &str) -> Result<String> {
            std::fs::write(&self.file, "// saved while the question was on screen\n")?;
            Ok("yes".to_string())
        }
    }

    #[test]
    fn work_saved_while_the_question_is_on_screen_is_not_replaced() {
        // The dirty check runs before the clone, which is a network round trip, and before the
        // prompt, which waits on a human. Both are long enough for an edit to land.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let installed = project.packages_dir().join("greeter");
        std::fs::write(tree.join("src/mod.rs"), "pub fn greet_v2() {}\n").unwrap();

        let stack = repository(&tree, SECOND);
        let error = update(
            &project,
            &stack.runner(),
            &EditWhileAsking {
                file: installed.join("src/mod.rs"),
            },
            Some("greeter"),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("no longer matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(installed.join("src/mod.rs")).unwrap(),
            "// saved while the question was on screen\n",
            "{error}"
        );
    }

    #[test]
    fn a_destination_this_command_did_not_create_is_never_removed() {
        // The Package's name is free for as long as the swap takes, so a concurrent `packages add`
        // can install into the gap. Only two processes can produce that state, so this drives
        // `restore` at its own seam: the alternative is no coverage at all for a branch whose
        // whole job is to not delete somebody else's Package.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        install(&project, &repository(&candidate(&tmp, "anything"), FIRST));
        let package = find(&project, &PackageName::parse("greeter").unwrap()).unwrap();

        let preserved = PreservedPackage::move_aside(&project, &package).unwrap();
        let aside = preserved.aside.clone();
        std::fs::create_dir_all(package.dir.join("src")).unwrap();
        std::fs::write(package.dir.join("src/mod.rs"), "// another process\n").unwrap();

        let error = preserved.restore(
            &package,
            FIRST,
            SECOND,
            Error::Process("the install never landed".to_string()),
            false,
        );

        assert!(error.to_string().contains("did not put there"), "{error}");
        assert_eq!(
            std::fs::read_to_string(package.dir.join("src/mod.rs")).unwrap(),
            "// another process\n",
            "the other process's Package must survive: {error}"
        );
        assert!(
            aside.join("src/mod.rs").is_file(),
            "the previous revision must survive too: {error}"
        );
    }

    #[test]
    fn a_hand_edited_revision_is_refused_rather_than_crashing_the_update() {
        // The stamp is operator-editable input. A revision that is not a commit at all must reach
        // the ordinary refusal path, so shortening it for the report cannot be a byte slice.
        assert_eq!(short("1a2b3c4d5e"), "1a2b3c4");
        assert_eq!(short("üüüüüüüüü"), "üüüüüüü");
        assert_eq!(short("abc"), "abc");

        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let tree = candidate(&tmp, "anything");
        install(&project, &repository(&tree, FIRST));
        let installed = project.packages_dir().join("greeter");
        let mut stamp = ProvenanceStamp::read(&installed).unwrap();
        stamp.revision = "üüüüüüüüü".to_string();
        stamp.write(&installed).unwrap();

        let stack = repository(&tree, SECOND);
        let error = update(
            &project,
            &stack.runner(),
            &Answer("no"),
            Some("greeter"),
            false,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(installed.join("src/mod.rs").is_file(), "{error}");
    }

    #[test]
    fn a_clone_never_waits_on_a_credential_prompt() {
        // A repository that needs credentials must fail rather than sit on a hidden prompt inside
        // a command an operator may have scripted with --yes.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&candidate(&tmp, "anything"), FIRST);

        install(&project, &stack);

        let clone = stack
            .calls()
            .into_iter()
            .find_map(|call| match call {
                crate::proc::fake::Call::Wait(spec) if spec.render().starts_with("git clone") => {
                    Some(spec)
                }
                _ => None,
            })
            .expect("no clone was run");
        assert_eq!(clone.env_value("GIT_TERMINAL_PROMPT"), Some("0"));
    }
}
