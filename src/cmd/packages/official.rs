//! The Official Package Collection: `lyracore packages add <name>`, resolving a bare Package name
//! against the one first-party collection repository rather than the filesystem.
//!
//! `LyraCoreProject/packages` holds several Packages side by side, one top-level directory each.
//! Installing one is a clone of the whole collection followed by the same `install` contract a
//! local folder or a Git Package Source goes through, scoped to the named directory: the rest of
//! the clone is never copied anywhere. The clone is scratch space, exactly like a Git Package
//! Source's — see [`RepositoryClone`] — so this module reuses that machinery rather than a second
//! copy of it.
//!
//! THE COMMIT IS PINNED AT INSTALL TIME, NOT THE NAME. The stamp records the exact commit the named
//! directory was resolved at, and nothing re-resolves it later: `packages update` refuses an
//! Official Package Source by name (`git_backing`'s catch-all), the same way it refuses a local
//! folder or a scaffold. A later commit to the collection — even one that renames or removes the
//! directory this Package came from — cannot change what is already installed.
//!
//! Out of scope, by design: a general Package registry, and more than this one repository. Adding
//! either later is a new resolution rule here, not a change to how a local folder or a Git Package
//! Source is read.

use super::git::{GitSource, RepositoryClone};
use super::{Origin, PackageName};
use crate::cmd::import::Prompt;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// The one Official Package Collection this CLI resolves bare names against. Hardcoded rather than
/// configurable: a general Package registry, or more than one first-party repository, is out of
/// scope for this resolution rule.
pub(crate) const COLLECTION_URL: &str = "https://github.com/LyraCoreProject/packages";

/// Install the top-level directory `name` names in the collection, the same way `packages add`
/// installs a local folder or a Git Package Source: clone, resolve the one directory, then hand it
/// to [`install`](super::install).
pub(crate) fn add(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    name: &PackageName,
    yes: bool,
) -> Result<()> {
    // Before the network: an install the inventory would refuse anyway is not worth a clone.
    super::check_collision(project, name)?;

    let collection =
        GitSource::parse(COLLECTION_URL).expect("COLLECTION_URL is a hardcoded https:// URL");
    let clone = RepositoryClone::fetch(project, runner, &collection)?;
    let source = resolve(clone.path(), name)?;

    super::install(
        project,
        runner,
        prompt,
        &source,
        name,
        &Origin::Official {
            revision: clone.revision(),
        },
        yes,
    )
}

/// The one top-level directory in a cloned collection that `name` names, or a refusal that installs
/// nothing.
///
/// A near miss that only differs by hyphen/underscore folding is named in the refusal — it is
/// almost always the one the operator meant — but never installed in the requested name's place:
/// these verbs act on the name the operator typed, exactly, the same rule [`find`](super::find)
/// applies to an already-installed Package.
fn resolve(collection: &Path, name: &PackageName) -> Result<PathBuf> {
    let mut top_level = Vec::new();
    for entry in std::fs::read_dir(collection)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            top_level.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    top_level.sort();

    if let Some(exact) = top_level
        .iter()
        .find(|candidate| candidate.as_str() == name.as_str())
    {
        return Ok(collection.join(exact));
    }
    let folded = top_level.iter().find(|candidate| {
        PackageName::parse(candidate).is_ok_and(|parsed| parsed.rust_ident() == name.rust_ident())
    });
    Err(Error::Usage(match folded {
        Some(near) => format!(
            "no Package called '{}' in the Official Package Collection ({COLLECTION_URL}). It \
             carries '{near}', which folds onto the same module `pkg_{}` — '-' and '_' are the \
             same character to the build. Nothing was installed.",
            name.as_str(),
            name.rust_ident()
        ),
        None => format!(
            "no Package called '{}' in the Official Package Collection ({COLLECTION_URL}). \
             Nothing was installed.",
            name.as_str()
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::packages::stamp::{self, ProvenanceStamp, SOURCE_OFFICIAL};
    use crate::cmd::packages::tests::{checkout, Answer};
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const FIRST: &str = "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b";
    const SECOND: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c";

    /// A fake Official Package Collection: several named Packages side by side at its root, the
    /// shape the real `LyraCoreProject/packages` keeps.
    fn collection(tmp: &TempDir, names: &[&str]) -> PathBuf {
        let root = tmp.path().join("collection");
        for name in names {
            let dir = root.join(name).join("src");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("mod.rs"),
                format!("pub fn {name}() {{}}\n").replace('-', "_"),
            )
            .unwrap();
        }
        root
    }

    /// A machine whose `git clone` produces `tree` and whose `git rev-parse HEAD` answers
    /// `revision` — modelling the collection repository at one commit.
    fn repository(tree: &Path, revision: &str) -> FakeStack {
        FakeStack::new()
            .with_git_clone(tree)
            .with_stdout("rev-parse HEAD", &format!("{revision}\n"))
    }

    #[test]
    fn a_known_top_level_package_installs_by_bare_name() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["greeter", "logger"]), FIRST);

        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();

        let installed = project.packages_dir().join("greeter");
        assert!(installed.join("src/mod.rs").is_file());
        assert!(
            !project.packages_dir().join("logger").exists(),
            "only the named directory is installed, not the rest of the collection"
        );
        let recorded = ProvenanceStamp::read(&installed).expect("no provenance stamp");
        assert_eq!(recorded.source_kind, SOURCE_OFFICIAL);
        assert_eq!(recorded.source, COLLECTION_URL);
        assert_eq!(recorded.revision, FIRST);
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the stamp must record the identity of what was actually copied"
        );
    }

    #[test]
    fn the_collection_clone_itself_is_never_installed() {
        // Only `greeter/` may land in the inventory; the collection's own `.git` and its sibling
        // Package directories must not.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["greeter", "logger"]), FIRST);

        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();

        assert!(!project.packages_dir().join("greeter/.git").exists());
        let clones = project.state_dir.join("package-clones");
        assert!(
            !clones.exists() || std::fs::read_dir(clones).unwrap().next().is_none(),
            "the clone is scratch space and must not survive the install"
        );
    }

    #[test]
    fn an_unknown_name_fails_with_no_partial_install() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["greeter"]), FIRST);

        let error = super::super::add(&project, &stack.runner(), &Answer("yes"), "missing", true)
            .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(
            error.to_string().contains("no Package called 'missing'"),
            "{error}"
        );
        assert!(!project.packages_dir().exists(), "{error}");
    }

    #[test]
    fn a_fold_equal_near_miss_is_named_but_not_installed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["foo-bar"]), FIRST);

        let error = super::super::add(&project, &stack.runner(), &Answer("yes"), "foo_bar", true)
            .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(
            error.to_string().contains("no Package called 'foo_bar'"),
            "{error}"
        );
        assert!(error.to_string().contains("'foo-bar'"), "{error}");
        assert!(error.to_string().contains("pkg_foo_bar"), "{error}");
        assert!(!project.packages_dir().exists(), "{error}");
    }

    #[test]
    fn a_name_already_installed_is_refused_before_the_collection_is_cloned() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("greeter")).unwrap();
        let stack = repository(&collection(&tmp, &["greeter"]), FIRST);

        let error = super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true)
            .unwrap_err();

        assert!(error.to_string().contains("already"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("git clone"), "{call}");
        }
    }

    #[test]
    fn a_collection_that_moves_on_does_not_change_what_is_already_installed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let root = collection(&tmp, &["greeter"]);
        let stack = repository(&root, FIRST);

        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();
        let installed = project.packages_dir().join("greeter");
        let before = ProvenanceStamp::read(&installed).unwrap();
        assert_eq!(before.revision, FIRST);

        // The collection moves on: same directory, different content, a later commit. Nothing in
        // this checkout re-clones it on its own.
        std::fs::write(root.join("greeter/src/mod.rs"), "pub fn greeter_v2() {}\n").unwrap();

        assert_eq!(
            ProvenanceStamp::read(&installed),
            Some(before),
            "an installed revision does not move on its own"
        );
        assert!(
            !std::fs::read_to_string(installed.join("src/mod.rs"))
                .unwrap()
                .contains("greeter_v2"),
            "the collection's later commit must not reach an already-installed Package"
        );
    }

    #[test]
    fn packages_update_refuses_an_official_source_package_by_name() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["greeter"]), FIRST);
        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();

        let error = super::super::git::update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("'official'"), "{error}");
        assert!(
            error
                .to_string()
                .contains("only advances Git Package Sources"),
            "{error}"
        );
        let recorded = ProvenanceStamp::read(&project.packages_dir().join("greeter")).unwrap();
        assert_eq!(
            recorded.revision, FIRST,
            "the refusal must not touch the recorded revision"
        );
    }

    #[test]
    fn the_consent_question_never_names_the_scratch_clone() {
        // The clone lands under `.lyracore/package-clones/...`; the operator never typed that path
        // and must not be asked to recognise it. `Origin::Official` carries no path at all, so this
        // is true by construction — asserted here so a future field addition cannot regress it.
        let named = Origin::Official { revision: SECOND }.named();
        assert!(named.contains("Official Package Collection"), "{named}");
        assert!(named.contains(COLLECTION_URL), "{named}");
        assert!(!named.contains("package-clones"), "{named}");
        assert!(!named.contains(".lyracore"), "{named}");
    }
}
