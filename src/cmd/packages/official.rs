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
//! THE COMMIT IS RECORDED AT INSTALL AND UPDATE TIME. The stamp records the exact commit the named
//! directory was resolved at. `packages update` re-clones the Official Package Collection and
//! resolves that same name in the candidate tree before it asks to replace the installed Package.
//! A later collection commit never changes an installed Package until the Operator consents to an
//! update that has passed the ordinary Package gates.
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

/// The one cloneable source behind the Official Package Collection.
pub(crate) fn collection_source() -> GitSource {
    GitSource::parse(COLLECTION_URL).expect("COLLECTION_URL is a hardcoded https:// URL")
}

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

    let collection = collection_source();
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
pub(crate) fn resolve(collection: &Path, name: &PackageName) -> Result<PathBuf> {
    let mut top_level = Vec::new();
    for entry in std::fs::read_dir(collection)? {
        let entry = entry?;
        // `.git` and a symlinked entry are both excluded here as a side effect of what they are,
        // not by name: `.git` can never parse as a `PackageName` (it does not start with a
        // letter), and `DirEntry::file_type` reports a symlink's own type without following it, so
        // `is_dir()` is false for one even when it points at a directory. Neither is ever a
        // candidate to match or to name in a near-miss refusal — the collection is a fresh clone
        // this command deletes on its way out, so nothing here is ever followed outside it.
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
             same character to the build. Did you mean `lyracore packages add {near}`? Nothing \
             was installed.",
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
    fn an_install_over_a_git_tracked_destination_is_refused_before_anything_is_written() {
        // `example` is core's tracked Reference Package; a future collection Package that shadows
        // it (or any other tracked `packages/` directory, e.g. `fire_nova`) must be refused too.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["example"]), FIRST)
            .with_stdout("ls-files", "packages/example\n");

        let error = super::super::add(&project, &stack.runner(), &Answer("yes"), "example", true)
            .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        // Both paths named: the Official Package Collection, and the tracked path it collides with.
        assert!(error.to_string().contains(COLLECTION_URL), "{error}");
        assert!(
            error
                .to_string()
                .contains(&project.packages_dir().join("example").display().to_string()),
            "{error}"
        );
        assert!(!project.packages_dir().join("example").exists(), "{error}");
    }

    #[test]
    fn the_collection_clone_scratch_space_does_not_survive_the_install() {
        // The clone (the whole collection, including `.git` at its root and every sibling
        // directory `greeter` never named) lands under `.lyracore/package-clones/`. Nothing of it
        // may still be on disk once the install returns — `an_unknown_name...` and
        // `a_known_top_level_package_installs_by_bare_name` cover that only the named directory's
        // OWN content reaches the inventory; this covers that the scratch space itself is gone.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = repository(&collection(&tmp, &["greeter", "logger"]), FIRST);

        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();

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
    fn a_named_update_advances_an_official_package_to_the_collection_tip() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let root = collection(&tmp, &["greeter"]);
        let stack = repository(&root, FIRST);
        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();
        let installed = project.packages_dir().join("greeter");
        std::fs::write(root.join("greeter/src/mod.rs"), "pub fn greeter_v2() {}\n").unwrap();
        let stack = repository(&root, SECOND);

        super::super::git::update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(installed.join("src/mod.rs")).unwrap(),
            "pub fn greeter_v2() {}\n"
        );
        let recorded = ProvenanceStamp::read(&installed).unwrap();
        assert_eq!(recorded.source_kind, SOURCE_OFFICIAL);
        assert_eq!(recorded.revision, SECOND);
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the updated Package must not read as locally drifted"
        );
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("--pack-client"), "{call}");
        }
    }

    #[test]
    fn an_unnamed_update_advances_every_official_package() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let root = collection(&tmp, &["greeter", "logger"]);
        let stack = repository(&root, FIRST);
        super::super::add(&project, &stack.runner(), &Answer("yes"), "greeter", true).unwrap();
        super::super::add(&project, &stack.runner(), &Answer("yes"), "logger", true).unwrap();

        std::fs::write(root.join("greeter/src/mod.rs"), "pub fn greeter_v2() {}\n").unwrap();
        std::fs::write(root.join("logger/src/mod.rs"), "pub fn logger_v2() {}\n").unwrap();
        let stack = repository(&root, SECOND);

        super::super::git::update(&project, &stack.runner(), &Answer("yes"), None, true).unwrap();

        for name in ["greeter", "logger"] {
            assert_eq!(
                ProvenanceStamp::read(&project.packages_dir().join(name))
                    .unwrap()
                    .revision,
                SECOND,
                "{name} must update in an unnamed sweep"
            );
        }
    }

    #[test]
    fn an_official_package_with_local_drift_is_refused_without_a_clone() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let root = collection(&tmp, &["greeter"]);
        super::super::add(
            &project,
            &repository(&root, FIRST).runner(),
            &Answer("yes"),
            "greeter",
            true,
        )
        .unwrap();
        let installed = project.packages_dir().join("greeter");
        std::fs::write(installed.join("src/mod.rs"), "// local work\n").unwrap();

        let stack = repository(&root, SECOND);
        let error = super::super::git::update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("no longer matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(installed.join("src/mod.rs")).unwrap(),
            "// local work\n"
        );
        assert!(
            stack
                .rendered()
                .iter()
                .all(|call| !call.contains("git clone")),
            "the drift check must run before the collection clone"
        );
    }

    #[test]
    fn a_failed_official_update_restores_the_previous_revision_and_names_both_commits() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let root = collection(&tmp, &["greeter"]);
        super::super::add(
            &project,
            &repository(&root, FIRST).runner(),
            &Answer("yes"),
            "greeter",
            true,
        )
        .unwrap();
        let installed = project.packages_dir().join("greeter");
        let before = stamp::content_identity(&installed).unwrap();
        std::fs::write(root.join("greeter/src/mod.rs"), "pub fn greeter_v2() {}\n").unwrap();

        let stack = repository(&root, SECOND).fail_on("cargo check", "the new revision is broken");
        let error = super::super::git::update(
            &project,
            &stack.runner(),
            &Answer("yes"),
            Some("greeter"),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains(&FIRST[..7]), "{error}");
        assert!(error.to_string().contains(&SECOND[..7]), "{error}");
        assert_eq!(stamp::content_identity(&installed).unwrap(), before);
        assert_eq!(ProvenanceStamp::read(&installed).unwrap().revision, FIRST);
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
