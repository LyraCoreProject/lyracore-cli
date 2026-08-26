//! `lyracore packages add <local-folder>`, `lyracore packages new <name>`, `lyracore packages
//! list`, and the lifecycle verbs in [`lifecycle`]: `enable`, `disable` and `remove`.
//!
//! A Package is a drop-in folder under `packages/<name>/` that the server build compiles into the
//! module with no core-file edits: `module/build.rs` discovers it, generates `pub mod pkg_<name>`
//! for its `src/mod.rs`, and registers every marker it finds. `importer --pack-client` picks up its
//! `client/` half the same way. Installing one is therefore not a configuration change — it is
//! adding trusted code to the realm — so `add` shows a deterministic inventory of what it registers
//! and asks before it copies anything. `new` copies from inside the checkout itself (the reference
//! Package), so there is nothing external to review or ask about.
//!
//! COPY, NEVER SYMLINK. A symlinked Package would compile from a folder outside the checkout, so
//! `preflight`, `publish` and `client sync` would each read whatever that folder happened to say at
//! the time. The copy is the installed Package; the folder it came from is only its Package Source.
//! `packages list` reports the two drifting apart.
//!
//! WHERE PACKAGES LIVE: enabled ones in `packages/` (what the build reads), disabled ones in
//! `.lyracore/packages-disabled/` (git-ignored local state the build cannot see). The location IS
//! the enabled state, which is why [`lifecycle`] can implement `enable` and `disable` as one
//! rename. `add`, `new` and `list` only have to know that BOTH are inventories a new name must not
//! collide with — an installed name that reappears when a Package is re-enabled is a collision the
//! operator would meet much later, holding two folders and no way to tell which one the build
//! compiled.

pub mod build;
pub mod lifecycle;
pub mod review;
pub mod stamp;
pub(crate) mod tree;

use crate::cmd::import::Prompt;
use crate::cmd::preflight;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};
use review::TrustReview;
use stamp::ProvenanceStamp;
use std::path::{Path, PathBuf};

/// The folder name of the maintained reference Package, checked into every LyraCore checkout
/// (including the public mirror) at `packages/example/`. `packages new` copies and renames it; see
/// its own doc comment for what a Package's Rust half looks like.
pub const REFERENCE_PACKAGE: &str = "example";

/// A Package folder name the server build will accept.
///
/// The rule is `module/build.rs::package_ident()`, mirrored exactly rather than approximated: the
/// build maps `my-package` onto the Rust module `pkg_my_package` and PANICS on a name that does not
/// map cleanly. A name this CLI accepted and that build refused would be an install that only fails
/// at the next `preflight`, with a build-script panic naming a folder the operator did not create
/// by hand.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageName(String);

impl PackageName {
    pub fn parse(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw.starts_with(|c: char| c.is_ascii_alphabetic())
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(Error::Usage(format!(
                "'{raw}' is not a usable Package name. A Package folder becomes the Rust module \
                 `pkg_<name>` in the module wasm, so the name must start with a letter and hold \
                 only letters, digits, '_' and '-' (the server build panics on anything else)."
            )));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The Rust module suffix the build derives: hyphens fold to underscores. TWO names that fold
    /// to the same identifier cannot be installed together — the generated `pkg_<ident>` modules
    /// would collide — which is why collision checks compare this and not the folder name.
    pub fn rust_ident(&self) -> String {
        self.0.replace('-', "_")
    }
}

/// Which inventory a Package is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageState {
    /// Under `packages/` — compiled into the module by the next `preflight`/`publish`.
    Enabled,
    /// Under `.lyracore/packages-disabled/` — kept, but invisible to the build.
    Disabled,
}

impl PackageState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageState::Enabled => "enabled",
            PackageState::Disabled => "disabled",
        }
    }
}

/// One Package on disk, from either inventory.
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub name: PackageName,
    pub state: PackageState,
    pub dir: PathBuf,
    /// `None` for a Package created by hand or one that predates `packages add`.
    pub stamp: Option<ProvenanceStamp>,
}

/// Every installed Package, enabled then disabled, each set sorted by name.
///
/// A folder whose name the build would refuse is still listed — it exists, and refusing to mention
/// it would hide the one thing that explains the build failure it causes. Its name is reported
/// verbatim.
pub fn inventory(project: &ProjectLayout) -> Result<Vec<InstalledPackage>> {
    let mut installed = Vec::new();
    for (state, root) in [
        (PackageState::Enabled, project.packages_dir()),
        (PackageState::Disabled, project.packages_disabled_dir()),
    ] {
        if !root.is_dir() {
            continue;
        }
        let mut dirs = Vec::new();
        for result in std::fs::read_dir(&root)? {
            let entry = result?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(Error::State(format!(
                    "{} is a linked Package in the {} inventory. Refusing to follow it outside \
                     the checkout; replace it with an ordinary copied directory.",
                    entry.path().display(),
                    state.as_str()
                )));
            }
            if file_type.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        for dir in dirs {
            let raw = dir.file_name().unwrap_or_default().to_string_lossy();
            installed.push(InstalledPackage {
                name: PackageName::parse(&raw).unwrap_or(PackageName(raw.into_owned())),
                state,
                stamp: ProvenanceStamp::read(&dir),
                dir,
            });
        }
    }
    Ok(installed)
}

/// The Package either inventory already holds under `name`'s generated module identifier, if any.
///
/// `ignoring` is the directory of a Package that is only being MOVED between the inventories: it is
/// still in the one it is leaving, so without this it would collide with itself.
pub(crate) fn collision(
    project: &ProjectLayout,
    name: &PackageName,
    ignoring: Option<&Path>,
) -> Result<Option<InstalledPackage>> {
    let ident = name.rust_ident();
    Ok(inventory(project)?.into_iter().find(|existing| {
        existing.name.rust_ident() == ident && Some(existing.dir.as_path()) != ignoring
    }))
}

/// Why `existing` blocks `name`: the same folder name, or a different spelling of it that the build
/// folds onto the same generated module.
pub(crate) fn collision_reason(existing: &InstalledPackage, name: &PackageName) -> String {
    if existing.name == *name {
        format!(
            "a {} Package is already called '{}'",
            existing.state.as_str(),
            existing.name.as_str()
        )
    } else {
        format!(
            "the {} Package '{}' already folds onto the same module `pkg_{}` ('-' and '_' are the \
             same character to the build)",
            existing.state.as_str(),
            existing.name.as_str(),
            name.rust_ident()
        )
    }
}

/// Refuse a name either inventory already holds, comparing the Rust identifiers the build derives
/// rather than the folder names.
pub fn check_collision(project: &ProjectLayout, name: &PackageName) -> Result<()> {
    match collision(project, name, None)? {
        Some(existing) => Err(Error::Usage(format!(
            "cannot install '{}': {}. It is at {}. Nothing was copied.",
            name.as_str(),
            collision_reason(&existing, name),
            existing.dir.display()
        ))),
        None => Ok(()),
    }
}

/// The shapes `module/build.rs` accepts.
///
/// A `client/` with no `src/` is a legal client-only Package. A `src/` without `src/mod.rs` is not:
/// the build panics rather than silently skipping it, so this refuses it here where the operator
/// still has the folder in front of them. Neither directory means the folder is not a Package at
/// all — the shape the build only warns about, because by then it is too late to ask.
pub fn validate_shape(source: &Path) -> Result<()> {
    let has_src = source.join("src").is_dir();
    let has_client = source.join("client").is_dir();
    if !has_src && !has_client {
        return Err(Error::Usage(format!(
            "{} is not a Package: it has neither src/ (Rust compiled into the module) nor client/ \
             (addons and client overrides). One of the two is required.",
            source.display()
        )));
    }
    if has_src && !source.join("src").join("mod.rs").is_file() {
        return Err(Error::Usage(format!(
            "{}/src/ has no mod.rs. A Package's Rust root must be src/mod.rs — the server build \
             refuses any other spelling rather than skipping the Package silently.",
            source.display()
        )));
    }
    Ok(())
}

// =============================================================================================
//  `packages add`
// =============================================================================================

/// Install a local folder as an enabled Package.
///
/// Order matters and is the whole design: everything that can refuse the install does so BEFORE
/// anything is copied, the operator sees the trust review before they are asked, and preflight runs
/// after the copy because it is the first check that can see the Package compiled in. Publishing is
/// never part of this command — the module on the node is unchanged until the operator runs
/// `lyracore publish` themselves.
pub fn add(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    source: &str,
    yes: bool,
) -> Result<()> {
    let source = std::fs::canonicalize(source).map_err(|e| {
        Error::Usage(format!(
            "cannot read the folder to install: {source} ({e}). `packages add` takes a path to a \
             Package folder on this machine."
        ))
    })?;
    if !source.is_dir() {
        return Err(Error::Usage(format!(
            "{} is a file. `packages add` takes the Package FOLDER — the one holding src/ and/or \
             client/.",
            source.display()
        )));
    }

    let name = PackageName::parse(&source.file_name().unwrap_or_default().to_string_lossy())?;

    let destination = project.packages_dir().join(name.as_str());
    if source.starts_with(project.packages_dir())
        || source.starts_with(project.packages_disabled_dir())
    {
        return Err(Error::Usage(format!(
            "{} is already inside this checkout's Package inventory. `packages add` installs a \
             folder from elsewhere on this machine.",
            source.display()
        )));
    }
    if destination.starts_with(&source) {
        return Err(Error::Usage(format!(
            "cannot install {} into {} because the destination is inside the folder being copied. \
             Choose the Package folder itself, not an ancestor of this checkout. Nothing was \
             copied.",
            source.display(),
            destination.display()
        )));
    }
    check_collision(project, &name)?;
    // Computing the identity validates the WHOLE tree (including client content) before the
    // narrower shape and trust-review passes. The same identity must describe the staged copy
    // after the operator answers, binding consent to the bytes that are actually installed.
    let reviewed_identity = stamp::content_identity(&source)?;
    validate_shape(&source)?;

    let review = TrustReview::scan(&source)?;
    println!();
    print!("{}", review.render(&source));
    println!();

    confirm(
        prompt,
        &format!(
            "Install '{}' into {}?",
            name.as_str(),
            destination.display()
        ),
        "Nothing was copied.",
        yes,
    )?;

    let mut staged = StagedPackage::new(project, &name)?;
    copy_tree(&source, staged.path())?;
    let identity = stamp::content_identity(staged.path())?;
    if identity != reviewed_identity {
        return Err(Error::Process(format!(
            "the Package Source changed after its trust review (reviewed {reviewed_identity}, \
             copied {identity}). Nothing was installed; review the current source and run \
             `lyracore packages add` again."
        )));
    }
    ProvenanceStamp::local(&source, identity.clone(), stamp::now_unix()).write(staged.path())?;
    // Recheck after the prompt and potentially long copy. A concurrent install must not be merged
    // with or overwritten by this one.
    let _claim = PackageClaim::acquire(project, &name)?;
    check_collision(project, &name)?;
    staged.install(&destination)?;
    println!();
    println!("installed {} -> {}", name.as_str(), destination.display());
    println!("  source    local {}", source.display());
    println!("  identity  {identity}");

    println!();
    println!("running preflight with the Package compiled in");
    preflight::run(project, runner).map_err(|e| {
        Error::Process(format!(
            "preflight failed with '{}' installed, so it has NOT been published and the module on \
             the node is unchanged.\n  The Package is on disk at {}. Fix it there and re-run \
             `lyracore preflight`, or undo the install with:\n      rm -rf -- {}\n  ({e})",
            name.as_str(),
            destination.display(),
            shell_quote(&destination)
        ))
    })?;

    println!();
    println!("'{}' is installed and preflight is green. Two steps remain, and this command runs neither:", name.as_str());
    println!("  lyracore publish       compile the Package into the module and publish it to every database");
    if review.addons.is_empty() && review.client_overrides == 0 {
        println!("  lyracore client sync   not needed: this Package ships no client content");
    } else {
        println!(
            "  lyracore client sync   install its {} addon(s) and {} client override(s) into your client",
            review.addons.len(),
            review.client_overrides
        );
    }
    Ok(())
}

/// The operator gate. Only the literal word 'yes' is consent; `--yes` answers it in advance.
///
/// `question` states the whole action and the path it happens to, because the sentence the operator
/// answers is their last chance to see what is about to change. `unchanged` is what the refusal
/// reports as still intact, which differs per command and is the half the operator cares about when
/// they say no.
pub(crate) fn confirm(
    prompt: &dyn Prompt,
    question: &str,
    unchanged: &str,
    yes: bool,
) -> Result<()> {
    if yes {
        println!("Confirmed on the command line (--yes).");
        return Ok(());
    }
    let answer = prompt.ask(&format!("{question} Type 'yes' to continue: "))?;
    if !answer.eq_ignore_ascii_case("yes") {
        return Err(Error::Usage(format!(
            "stopping: the answer was {answer:?}, and only 'yes' is consent. {unchanged}"
        )));
    }
    Ok(())
}

/// A staged Package outside the enabled inventory. Unless [`install`](Self::install) completes,
/// dropping it removes the partial copy so a failed install cannot block the next attempt or enter
/// a concurrent build.
struct StagedPackage {
    path: PathBuf,
    installed: bool,
}

/// An interprocess lock for one generated Rust module identifier.
///
/// The final no-replace rename protects one folder spelling. This lock protects the wider build
/// collision: `foo-bar` and `foo_bar` have different destinations but both generate
/// `pkg_foo_bar`. Holding it across the last inventory check and rename makes that decision one
/// atomic critical section across `packages add` and `packages new` processes.
#[derive(Debug)]
struct PackageClaim {
    _file: std::fs::File,
}

impl PackageClaim {
    fn acquire(project: &ProjectLayout, name: &PackageName) -> Result<Self> {
        let root = project.state_dir.join("package-locks");
        std::fs::create_dir_all(&root)?;
        let path = root.join(format!("{}.lock", name.rust_ident()));
        // `truncate(false)`: the lock is the file's EXISTENCE and its flock, never its content, so
        // an already-held lock file must be opened as it is. Stated rather than left to the
        // default, which `clippy::suspicious_open_options` is right to ask about.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                Error::Process(format!(
                "another Package install is currently claiming module `pkg_{}` ({error}). Wait \
                 for it to finish, then retry; nothing was installed by this process.",
                name.rust_ident()
            ))
            },
        )?;
        Ok(Self { _file: file })
    }
}

impl StagedPackage {
    fn new(project: &ProjectLayout, name: &PackageName) -> Result<Self> {
        let root = project.state_dir.join("package-installs");
        std::fs::create_dir_all(&root)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = root.join(format!("{}-{}-{nonce}", name.as_str(), std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self {
            path,
            installed: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn install(&mut self, destination: &Path) -> Result<()> {
        let parent = destination.parent().ok_or_else(|| {
            Error::State(format!(
                "Package destination {} has no parent directory",
                destination.display()
            ))
        })?;
        std::fs::create_dir_all(parent)?;
        rename_no_replace(&self.path, destination).map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                Error::Usage(format!(
                    "cannot install: {} appeared while the Package was being reviewed and copied. \
                     Nothing was merged or overwritten.",
                    destination.display()
                ))
            } else {
                Error::Process(format!(
                    "could not atomically install the staged Package at {} without overwriting an \
                     existing path: {error}. Nothing was merged or overwritten.",
                    destination.display()
                ))
            }
        })?;
        self.installed = true;
        Ok(())
    }
}

impl Drop for StagedPackage {
    fn drop(&mut self) {
        if !self.installed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Move `from` onto `to`, never replacing whatever is already at `to`.
///
/// The guarantee both the staged install and the enable/disable moves need: an occupied destination
/// must fail the whole operation rather than merge two Packages or overwrite one. Callers report
/// `EEXIST` in their own words, because "the destination appeared while I was copying" and "the
/// other inventory already holds this name" are different things to fix.
fn rename_no_replace(from: &Path, to: &Path) -> std::result::Result<(), rustix::io::Errno> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

/// Copy a validated Package tree file by file into a fresh staging directory.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in tree::collect(source)? {
        let target = destination.join(&entry.relative);
        match entry.kind {
            tree::EntryKind::Directory => std::fs::create_dir(&target)?,
            tree::EntryKind::File => {
                std::fs::copy(&entry.path, &target)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

// =============================================================================================
//  `packages new`
// =============================================================================================

/// Scaffold a new Package by copying and renaming the reference Package (`packages/example/`,
/// checked into this checkout). Fetching the template needs no network; the ordinary preflight at
/// the end still uses Cargo's configured cache/network like `lyracore preflight` itself. Nothing
/// came from outside the checkout, so unlike `add` there is no Package Source or trust review to
/// show.
pub fn new(project: &ProjectLayout, runner: &dyn ProcessRunner, name: &str) -> Result<()> {
    let name = PackageName::parse(name)?;
    check_collision(project, &name)?;

    let reference = project.packages_dir().join(REFERENCE_PACKAGE);
    if !reference.is_dir() {
        return Err(Error::Usage(format!(
            "no reference Package at {}. `packages new` scaffolds by copying `packages/{}/` out of \
             this checkout, so a checkout missing it cannot scaffold — that is a broken or partial \
             checkout, not a problem with the name '{}'.",
            reference.display(),
            REFERENCE_PACKAGE,
            name.as_str()
        )));
    }
    // Validate the complete maintained tree with the same no-links/no-special-files policy as a
    // local install, before an enabled destination exists.
    stamp::content_identity(&reference)?;
    validate_shape(&reference)?;

    let destination = project.packages_dir().join(name.as_str());
    let mut staged = StagedPackage::new(project, &name)?;
    copy_tree(&reference, staged.path())?;
    rewrite_reference_name(staged.path(), &name)?;

    let identity = stamp::content_identity(staged.path())?;
    ProvenanceStamp::scaffolded(
        &format!("packages/{REFERENCE_PACKAGE}/ (the reference Package)"),
        identity.clone(),
        stamp::now_unix(),
    )
    .write(staged.path())?;
    let _claim = PackageClaim::acquire(project, &name)?;
    check_collision(project, &name)?;
    staged.install(&destination)?;
    println!();
    println!("scaffolded {} -> {}", name.as_str(), destination.display());
    println!("  from      packages/{REFERENCE_PACKAGE}/ (the reference Package)");
    println!("  identity  {identity}");

    println!();
    println!("running preflight with the Package compiled in");
    preflight::run(project, runner).map_err(|e| {
        Error::Process(format!(
            "preflight failed after '{}' was scaffolded, so it has NOT been published and the \
             module on the node is unchanged.\n  The Package remains at {}. The failure below may \
             be in its code or in another preflight prerequisite; fix the reported cause and \
             re-run `lyracore preflight`, or undo the scaffold with:\n      rm -rf -- {}\n  ({e})",
            name.as_str(),
            destination.display(),
            shell_quote(&destination)
        ))
    })?;

    println!();
    println!(
        "'{}' is scaffolded and preflight is green. It has no client/ directory yet, so:",
        name.as_str()
    );
    println!("  lyracore publish       compile the Package into the module and publish it to every database");
    println!(
        "  lyracore client sync   nothing to install yet — add a client/ directory (addons under"
    );
    println!(
        "                         client/addons/<Name>/, overrides under client/mpq/) and re-run"
    );
    println!(
        "grow the Rust half in packages/{}/src/: wire more hooks from the catalog in",
        name.as_str()
    );
    println!("module/src/hooks.rs, following the pattern already in its src/mod.rs.");
    Ok(())
}

/// Replace the reference Package's own name inside the copied tree's file contents, so a scaffold
/// named `greeter` does not keep saying `example` in its identifiers. The reference Package is
/// maintained to use the literal word "example" ONLY inside an identifier, never in prose (see its
/// own doc comment), so a whole-file substring replace is exact rather than approximate. A file that
/// is not valid UTF-8 is left untouched — the reference Package ships none, and a future one that
/// did would not be text this rewrite could safely touch anyway.
fn rewrite_reference_name(destination: &Path, name: &PackageName) -> Result<()> {
    let ident = name.rust_ident();
    for entry in tree::collect(destination)? {
        if entry.kind != tree::EntryKind::File || entry.relative == Path::new(stamp::STAMP_FILE) {
            continue;
        }
        match std::fs::read_to_string(&entry.path) {
            Ok(text) if text.contains(REFERENCE_PACKAGE) => {
                std::fs::write(&entry.path, text.replace(REFERENCE_PACKAGE, &ident))?;
            }
            Ok(_) => {}
            // A future reference may carry binary client assets; they have no textual identifier
            // to rewrite and are copied byte-for-byte.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// =============================================================================================
//  `packages list`
// =============================================================================================

/// Report every installed Package: which inventory it is in, where it came from, what its content
/// was when it was installed, whether it still matches, and what it registers.
pub fn list(project: &ProjectLayout) -> Result<()> {
    let installed = inventory(project)?;
    if installed.is_empty() {
        println!("no Packages installed.");
        println!(
            "`lyracore packages add <folder>` installs one from a folder on this machine, into {}.",
            project.packages_dir().display()
        );
        return Ok(());
    }

    let enabled = installed
        .iter()
        .filter(|p| p.state == PackageState::Enabled)
        .count();
    println!(
        "{} Package(s): {enabled} enabled, {} disabled",
        installed.len(),
        installed.len() - enabled
    );

    for package in &installed {
        let identity = stamp::content_identity(&package.dir)?;
        println!();
        println!("{}  {}", package.name.as_str(), package.state.as_str());
        print!("{}", provenance_report(package.stamp.as_ref()));
        match &package.stamp {
            Some(recorded) => print!("{}", identity_report(recorded, &identity)),
            // Created by hand or installed before `packages add` existed. It is a real Package the
            // build compiles; only its provenance is unknown.
            None => println!("  identity  {identity}  (nothing recorded to compare against)"),
        }
        println!(
            "  content   {}",
            TrustReview::scan(&package.dir)?.kinds_summary()
        );
    }
    Ok(())
}

/// Where a Package came from and when, as `packages list` and the lifecycle verbs both print it. A
/// Package with no stamp says so rather than printing blank fields.
pub(crate) fn provenance_report(stamp: Option<&ProvenanceStamp>) -> String {
    match stamp {
        Some(recorded) => format!(
            "  source    {} {}\n  installed {}\n",
            blank_as_unrecorded(&recorded.source_kind),
            blank_as_unrecorded(&recorded.source),
            blank_as_unrecorded(&recorded.installed_at)
        ),
        None => "  source    (unrecorded — no provenance stamp: created by hand, or predates \
                 `packages add`)\n"
            .to_string(),
    }
}

fn drift(recorded: &ProvenanceStamp, current: &str) -> &'static str {
    if recorded.content_identity.is_empty() {
        "(the stamp records no identity to compare against)"
    } else if recorded.content_identity == current {
        "clean"
    } else {
        "LOCALLY DRIFTED — the installed copy no longer matches what was installed"
    }
}

fn identity_report(recorded: &ProvenanceStamp, current: &str) -> String {
    if recorded.content_identity.is_empty() {
        format!(
            "  identity  (unrecorded)  {}\n  current   {current}\n",
            drift(recorded, current)
        )
    } else if recorded.content_identity == current {
        format!("  identity  {}  clean\n", recorded.content_identity)
    } else {
        format!(
            "  identity  {}  {}\n  current   {current}\n",
            recorded.content_identity,
            drift(recorded, current)
        )
    }
}

fn blank_as_unrecorded(value: &str) -> &str {
    if value.is_empty() {
        "(unrecorded)"
    } else {
        value
    }
}

/// Fixtures shared with [`lifecycle`]'s tests, which start from a really installed Package rather
/// than a hand-built directory.
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::proc::fake::{FakeStack, FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
    use tempfile::TempDir;

    /// A checkout `preflight` passes in: the same fixture its own tests use, so an `add` test
    /// exercises the real post-install gate rather than a stubbed one.
    pub(super) fn checkout(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path().join("checkout");
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!(
                "[dependencies]\nspacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst CHARACTER_RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        ProjectLayout::from_root(&root).unwrap()
    }

    /// A valid Rust Package folder outside the checkout.
    pub(super) fn candidate(tmp: &TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join("sources").join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/mod.rs"),
            "#[spacetimedb::table(accessor = pkg_greeter_log)]\npub struct Log { pub id: u64 }\n\
             game_hook!(on_login, fn greet(ctx, payload) { });\n",
        )
        .unwrap();
        dir
    }

    /// A prompt that always gives one answer, so a test can drive the consent gate either way.
    pub(super) struct Answer(pub(super) &'static str);
    impl Prompt for Answer {
        fn ask(&self, _question: &str) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    struct MutatingAnswer {
        file: PathBuf,
    }

    impl Prompt for MutatingAnswer {
        fn ask(&self, _question: &str) -> Result<String> {
            std::fs::write(&self.file, "pub fn code_the_operator_never_reviewed() {}\n")?;
            Ok("yes".to_string())
        }
    }

    // ---- the name contract, mirrored from module/build.rs ----

    #[test]
    fn a_name_is_usable_exactly_when_the_server_build_would_accept_it() {
        // The rule is build.rs's `package_ident`: [a-zA-Z][a-zA-Z0-9_-]*. A name this CLI took and
        // that build panicked on would be an install that breaks the next preflight.
        for ok in ["greeter", "my-package", "my_package", "a", "pkg2"] {
            assert!(PackageName::parse(ok).is_ok(), "{ok}");
        }
        for refused in [
            "",
            "2fast",
            "-leading",
            "_leading",
            "has space",
            "dot.name",
            "üml",
        ] {
            let error = PackageName::parse(refused).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{refused}");
        }
    }

    #[test]
    fn hyphens_and_underscores_fold_onto_the_same_module() {
        assert_eq!(
            PackageName::parse("my-package").unwrap().rust_ident(),
            "my_package"
        );
        assert_eq!(
            PackageName::parse("my_package").unwrap().rust_ident(),
            "my_package"
        );
    }

    // ---- the shape contract ----

    #[test]
    fn a_client_only_package_is_valid_and_a_src_without_mod_rs_is_not() {
        let tmp = TempDir::new().unwrap();

        let client_only = tmp.path().join("ui");
        std::fs::create_dir_all(client_only.join("client/addons/UI")).unwrap();
        validate_shape(&client_only).unwrap();

        let rust_only = tmp.path().join("logic");
        std::fs::create_dir_all(rust_only.join("src")).unwrap();
        std::fs::write(rust_only.join("src/mod.rs"), "").unwrap();
        validate_shape(&rust_only).unwrap();

        let headless = tmp.path().join("headless");
        std::fs::create_dir_all(headless.join("src")).unwrap();
        std::fs::write(headless.join("src/other.rs"), "").unwrap();
        let error = validate_shape(&headless).unwrap_err();
        assert!(error.to_string().contains("mod.rs"), "{error}");

        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = validate_shape(&empty).unwrap_err();
        assert!(error.to_string().contains("neither src/"), "{error}");
    }

    // ---- the install ----

    #[test]
    fn a_confirmed_install_copies_the_tree_stamps_it_and_stops_short_of_publishing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let stack = FakeStack::new();

        add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            false,
        )
        .unwrap();

        let installed = project.packages_dir().join("greeter");
        assert!(installed.join("src/mod.rs").is_file());
        let recorded = ProvenanceStamp::read(&installed).expect("no provenance stamp");
        assert_eq!(recorded.source_kind, stamp::SOURCE_LOCAL);
        assert_eq!(recorded.source, source.to_string_lossy());
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the stamp must record the identity of what was actually copied"
        );
        // The remaining steps are PRINTED, never run: nothing here may touch the node.
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("--pack-client"), "{call}");
        }
    }

    #[test]
    fn the_copy_is_a_copy_never_a_link() {
        // A linked Package would compile from a folder outside the checkout, so preflight, publish
        // and client sync would each read whatever it said at the time.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();

        let installed = project.packages_dir().join("greeter");
        assert!(!installed
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::write(source.join("src/mod.rs"), "// changed at the source\n").unwrap();
        assert!(
            std::fs::read_to_string(installed.join("src/mod.rs"))
                .unwrap()
                .contains("game_hook"),
            "editing the Package Source must not change the installed Package"
        );
    }

    #[test]
    fn an_answer_other_than_yes_copies_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("no"),
            source.to_str().unwrap(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    #[test]
    fn an_invalid_name_or_shape_is_refused_before_anything_is_copied() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let bad_name = tmp.path().join("sources").join("2fast");
        std::fs::create_dir_all(bad_name.join("src")).unwrap();
        std::fs::write(bad_name.join("src/mod.rs"), "").unwrap();
        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            bad_name.to_str().unwrap(),
            true,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(
            !project.packages_dir().exists(),
            "nothing may be created: {error}"
        );
    }

    #[test]
    fn a_source_that_contains_the_checkout_is_refused_before_recursive_copying() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("greeter");
        std::fs::create_dir_all(source.join("src")).unwrap();
        std::fs::write(source.join("src/mod.rs"), "pub fn greet() {}\n").unwrap();
        let root = source.join("checkout");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(&root).unwrap();

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("destination is inside"),
            "{error}"
        );
        assert!(!project.packages_dir().exists(), "{error}");
    }

    #[test]
    fn a_name_either_inventory_already_holds_is_refused_before_the_copy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");

        // Enabled collision.
        std::fs::create_dir_all(project.packages_dir().join("greeter")).unwrap();
        let error = check_collision(&project, &PackageName::parse("greeter").unwrap()).unwrap_err();
        assert!(error.to_string().contains("enabled"), "{error}");

        // ...and a DISABLED one, which the build cannot see today but would collide the moment it
        // was re-enabled.
        std::fs::remove_dir_all(project.packages_dir().join("greeter")).unwrap();
        std::fs::create_dir_all(project.packages_disabled_dir().join("greeter")).unwrap();
        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("disabled"), "{error}");
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    #[test]
    fn a_name_that_folds_onto_an_installed_module_is_refused_too() {
        // `my-package` and `my_package` both generate `pkg_my_package`; installing both would give
        // the module two modules with one name.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("my-package")).unwrap();

        let error =
            check_collision(&project, &PackageName::parse("my_package").unwrap()).unwrap_err();

        assert!(error.to_string().contains("pkg_my_package"), "{error}");
    }

    #[test]
    fn a_failed_preflight_publishes_nothing_and_says_how_to_undo_the_copy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let stack = FakeStack::new().fail_on("cargo check", "the Package does not compile");

        let error = add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();

        let installed = project.packages_dir().join("greeter");
        assert!(error.to_string().contains("preflight failed"), "{error}");
        // The exact undo, not "remove it manually" — the operator is holding a half-finished
        // install and the path is the one thing they need.
        assert!(
            error
                .to_string()
                .contains(&format!("rm -rf -- {}", shell_quote(&installed))),
            "{error}"
        );
        assert!(
            installed.is_dir(),
            "the copy stays so it can be fixed in place"
        );
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
        }
    }

    #[test]
    fn a_symlink_inside_the_candidate_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let outside = tmp.path().join("elsewhere.rs");
        std::fs::write(&outside, "// not part of the reviewed folder\n").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("src/linked.rs")).unwrap();

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(
            !project.packages_dir().join("greeter").exists(),
            "a refused tree must leave no enabled partial Package: {error}"
        );
    }

    #[test]
    fn source_changes_after_the_review_are_not_installed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &MutatingAnswer {
                file: source.join("src/mod.rs"),
            },
            source.to_str().unwrap(),
            false,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("changed after its trust review"),
            "{error}"
        );
        assert!(!project.packages_dir().join("greeter").exists());
        let staging = project.state_dir.join("package-installs");
        assert!(
            !staging.exists() || std::fs::read_dir(staging).unwrap().next().is_none(),
            "the rejected staged copy must be cleaned"
        );
    }

    #[test]
    fn the_final_claim_never_replaces_even_an_empty_destination() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let name = PackageName::parse("greeter").unwrap();
        let mut staged = StagedPackage::new(&project, &name).unwrap();
        std::fs::write(staged.path().join("payload.txt"), "staged").unwrap();
        let destination = project.packages_dir().join(name.as_str());
        std::fs::create_dir_all(&destination).unwrap();

        let error = staged.install(&destination).unwrap_err();

        assert!(error
            .to_string()
            .contains("Nothing was merged or overwritten"));
        assert!(std::fs::read_dir(&destination).unwrap().next().is_none());
    }

    #[test]
    fn folded_package_names_share_one_interprocess_claim() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let hyphen = PackageName::parse("foo-bar").unwrap();
        let underscore = PackageName::parse("foo_bar").unwrap();

        let first = PackageClaim::acquire(&project, &hyphen).unwrap();
        let error = PackageClaim::acquire(&project, &underscore).unwrap_err();

        assert!(error.to_string().contains("pkg_foo_bar"), "{error}");
        drop(first);
        PackageClaim::acquire(&project, &underscore).unwrap();
    }

    #[test]
    fn linked_inventory_entries_are_reported_without_following_them() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir()).unwrap();
        std::os::unix::fs::symlink("/", project.packages_dir().join("outside")).unwrap();

        let error = inventory(&project).unwrap_err();

        assert!(error.to_string().contains("linked Package"), "{error}");
    }

    // ---- `packages new` ----

    /// A checkout that also carries a reference Package, standing in for the real
    /// `packages/example/` this CLI ships in the LyraCore repo. Its source deliberately spells the
    /// literal word "example" only inside an identifier, matching the real reference Package's own
    /// constraint, so a scaffold test can assert the rename actually happened.
    fn checkout_with_reference(tmp: &TempDir) -> ProjectLayout {
        let project = checkout(tmp);
        let reference = project.packages_dir().join(REFERENCE_PACKAGE).join("src");
        std::fs::create_dir_all(&reference).unwrap();
        std::fs::write(
            reference.join("mod.rs"),
            "crate::game_hook!(on_group_invite, fn example_on_group_invite(_ctx, _payload) { });\n",
        )
        .unwrap();
        project
    }

    #[test]
    fn a_scaffold_copies_the_reference_renames_it_stamps_it_and_stops_short_of_publishing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout_with_reference(&tmp);
        let stack = FakeStack::new();

        new(&project, &stack.runner(), "greeter").unwrap();

        let scaffolded = project.packages_dir().join("greeter");
        let source = std::fs::read_to_string(scaffolded.join("src/mod.rs")).unwrap();
        assert!(
            source.contains("greeter_on_group_invite"),
            "the reference Package's own name must be renamed: {source}"
        );
        assert!(!source.contains("example_on_group_invite"), "{source}");

        let recorded = ProvenanceStamp::read(&scaffolded).expect("no provenance stamp");
        assert_eq!(recorded.source_kind, stamp::SOURCE_SCAFFOLD);
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&scaffolded).unwrap(),
            "the stamp must record the identity of what was actually written"
        );
        // The remaining steps are PRINTED, never run: nothing here may touch the node.
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("--pack-client"), "{call}");
        }
    }

    #[test]
    fn a_scaffold_name_either_inventory_already_holds_is_refused_before_anything_is_written() {
        let tmp = TempDir::new().unwrap();
        let project = checkout_with_reference(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("greeter")).unwrap();

        let error = new(&project, &FakeStack::new().runner(), "greeter").unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("enabled"), "{error}");
    }

    #[test]
    fn an_invalid_scaffold_name_is_refused_before_anything_is_written() {
        let tmp = TempDir::new().unwrap();
        let project = checkout_with_reference(&tmp);

        let error = new(&project, &FakeStack::new().runner(), "2fast").unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(!project.packages_dir().join("2fast").exists(), "{error}");
    }

    #[test]
    fn a_checkout_missing_the_reference_package_cannot_scaffold() {
        // `checkout()`, not `checkout_with_reference()` — a checkout without `packages/example/` is
        // exactly the broken/partial state this error names, not something `new` can paper over.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let error = new(&project, &FakeStack::new().runner(), "greeter").unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("reference Package"), "{error}");
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    #[test]
    fn a_failed_preflight_after_scaffolding_publishes_nothing_and_says_how_to_undo_it() {
        let tmp = TempDir::new().unwrap();
        let project = checkout_with_reference(&tmp);
        let stack = FakeStack::new().fail_on("cargo check", "the scaffold does not compile");

        let error = new(&project, &stack.runner(), "greeter").unwrap_err();

        let scaffolded = project.packages_dir().join("greeter");
        assert!(error.to_string().contains("preflight failed"), "{error}");
        assert!(
            error
                .to_string()
                .contains(&format!("rm -rf -- {}", shell_quote(&scaffolded))),
            "{error}"
        );
        assert!(
            scaffolded.is_dir(),
            "the scaffold stays so it can be fixed in place"
        );
    }

    // ---- `packages list` ----

    #[test]
    fn list_reports_both_inventories_and_the_drift_of_each() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();
        let installed = project.packages_dir().join("greeter");

        let installed_packages = inventory(&project).unwrap();
        assert_eq!(installed_packages.len(), 1);
        assert_eq!(installed_packages[0].state, PackageState::Enabled);
        let recorded = installed_packages[0].stamp.clone().unwrap();
        assert_eq!(
            drift(&recorded, &stamp::content_identity(&installed).unwrap()),
            "clean"
        );

        // Edit the installed copy: the recorded identity no longer describes what is on disk.
        std::fs::write(installed.join("src/mod.rs"), "// edited in place\n").unwrap();
        assert!(
            drift(&recorded, &stamp::content_identity(&installed).unwrap()).contains("DRIFTED")
        );
        let current = stamp::content_identity(&installed).unwrap();
        let report = identity_report(&recorded, &current);
        assert!(report.contains(&recorded.content_identity), "{report}");
        assert!(report.contains(&current), "{report}");
        assert!(report.contains("DRIFTED"), "{report}");

        // A disabled Package is in the inventory too, and `list` renders both.
        std::fs::create_dir_all(project.packages_disabled_dir().join("retired/src")).unwrap();
        let installed_packages = inventory(&project).unwrap();
        assert_eq!(installed_packages.len(), 2);
        assert_eq!(installed_packages[1].state, PackageState::Disabled);
        list(&project).unwrap();
    }

    #[test]
    fn a_package_with_no_stamp_is_described_rather_than_crashing_the_listing() {
        // The pre-existing case: a folder somebody dropped into packages/ by hand, or one that
        // predates `packages add`.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("handmade/src")).unwrap();
        std::fs::write(
            project.packages_dir().join("handmade/src/mod.rs"),
            "pub fn helper() {}\n",
        )
        .unwrap();

        let installed_packages = inventory(&project).unwrap();

        assert_eq!(installed_packages.len(), 1);
        assert!(installed_packages[0].stamp.is_none());
        list(&project).unwrap();
    }

    #[test]
    fn an_empty_checkout_lists_nothing_and_says_how_to_install() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        assert!(inventory(&project).unwrap().is_empty());
        list(&project).unwrap();
    }

    #[test]
    fn recovery_paths_are_shell_quoted_as_one_literal_argument() {
        let path = Path::new("/tmp/a package/'quoted';$(do-not-run)");
        assert_eq!(
            shell_quote(path),
            "'/tmp/a package/'\"'\"'quoted'\"'\"';$(do-not-run)'"
        );
    }
}
