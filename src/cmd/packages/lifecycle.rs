//! `lyracore packages enable`, `lyracore packages disable` and `lyracore packages remove`.
//!
//! ENABLED IS A LOCATION, not a recorded flag. `packages/` is what the server build discovers,
//! `.lyracore/packages-disabled/` is a directory the build cannot see, and enabling or disabling is
//! the one rename between them. Nothing can disagree with the filesystem about which Packages the
//! next build compiles; a state file, a stamp key or a config entry all could.
//!
//! Both directories are on the same filesystem, so the rename is atomic and its inverse is the
//! other command. That is why `enable` and `disable` do not ask the way `add` and `remove` do:
//! nothing is destroyed, and the undo is one command their own output names. The provenance stamp
//! lives inside the folder, so a move carries it along untouched. A re-enabled Package still says
//! where it came from and what it looked like when it was installed.
//!
//! `remove` is the one that deletes, so it is the one with gates. It requires the Package to be
//! disabled already, because the build must stop compiling a Package before the folder goes. It
//! refuses a tree that no longer matches its stamp: an operator's own edits to an installed copy
//! exist nowhere else, and this command cannot get them back.
//!
//! None of the three publishes anything or synchronizes a client. The module on every database is
//! whatever was last published until the operator publishes again, and each command ends by naming
//! the steps it did not run.

use super::stamp::{self, SOURCE_LOCAL};
use super::{
    collision, collision_reason, confirm, inventory, review::TrustReview, shell_quote,
    InstalledPackage, PackageName, PackageState,
};
use crate::cmd::import::Prompt;
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::Path;

/// Move a disabled Package back into `packages/`, where the next build compiles it.
pub fn enable(project: &ProjectLayout, name: &str) -> Result<()> {
    let name = PackageName::parse(name)?;
    let package = find(project, &name)?;
    if package.state == PackageState::Enabled {
        return Err(Error::Usage(format!(
            "'{}' is already enabled, at {}. Nothing was moved.",
            name.as_str(),
            package.dir.display()
        )));
    }

    let destination = project.packages_dir().join(name.as_str());
    let moved = move_package(project, &package, &destination, "enable")?;

    println!();
    println!("enabled {}", name.as_str());
    print!("{moved}");
    print!("{}", super::provenance_report(package.stamp.as_ref()));

    let review = TrustReview::scan(&destination)?;
    println!();
    println!(
        "'{}' is back in the build. Three steps remain, and this command ran none of them:",
        name.as_str()
    );
    println!("  lyracore preflight     compile the module with the Package in it and run the offline gate");
    println!("  lyracore publish       publish the rebuilt module to every database of this realm");
    println!("  {}", client_sync_step(&review));
    println!();
    println!(
        "undo this move with `lyracore packages disable {}`.",
        name.as_str()
    );
    Ok(())
}

/// Move an enabled Package out of `packages/`, so the build stops seeing it.
pub fn disable(project: &ProjectLayout, name: &str) -> Result<()> {
    let name = PackageName::parse(name)?;
    let package = find(project, &name)?;
    if package.state == PackageState::Disabled {
        return Err(Error::Usage(format!(
            "'{}' is already disabled, at {}. Nothing was moved.",
            name.as_str(),
            package.dir.display()
        )));
    }

    // The tables have to be read while the Package is still where it is, and reported before the
    // move, because they are the one consequence of disabling that the operator cannot undo by
    // enabling it again: by then a publish may already have refused, or dropped, the tables.
    let review = TrustReview::scan(&package.dir)?;
    if !review.tables.is_empty() {
        println!();
        print!("{}", table_warning(&name, &review.tables));
    }

    let destination = project.packages_disabled_dir().join(name.as_str());
    let moved = move_package(project, &package, &destination, "disable")?;

    println!();
    println!("disabled {}", name.as_str());
    print!("{moved}");
    print!("{}", super::provenance_report(package.stamp.as_ref()));

    println!();
    println!(
        "'{}' is out of the build and still on disk. Two steps remain, and this command ran \
         neither:",
        name.as_str()
    );
    println!(
        "  lyracore publish       publish the module WITHOUT the Package to every database of \
         this realm"
    );
    println!("  {}", client_sync_step(&review));
    println!();
    println!(
        "undo this move with `lyracore packages enable {}`.",
        name.as_str()
    );
    Ok(())
}

/// Delete a disabled Package from this checkout, after the operator confirms it.
pub fn remove(project: &ProjectLayout, prompt: &dyn Prompt, name: &str, yes: bool) -> Result<()> {
    let name = PackageName::parse(name)?;
    let package = find(project, &name)?;
    if package.state == PackageState::Enabled {
        return Err(Error::Usage(format!(
            "cannot remove '{}': it is enabled, at {}. Run `lyracore packages disable {}` first, \
             so the build stops compiling the Package before the folder is deleted, and publish \
             that change. Nothing was deleted.",
            name.as_str(),
            package.dir.display(),
            name.as_str()
        )));
    }
    check_recorded_and_clean(&package)?;

    println!();
    println!("{}  {}", name.as_str(), package.state.as_str());
    print!("{}", super::provenance_report(package.stamp.as_ref()));
    println!(
        "  content   {}",
        TrustReview::scan(&package.dir)?.kinds_summary()
    );
    println!();
    print!("{}", recovery_note(package.stamp.as_ref()));

    confirm(
        prompt,
        &format!(
            "Permanently delete '{}' and everything in {}?",
            name.as_str(),
            package.dir.display()
        ),
        "Nothing was deleted.",
        yes,
    )?;
    std::fs::remove_dir_all(&package.dir).map_err(|error| {
        Error::Process(format!(
            "could not delete {}: {error}. Whatever is left of the Package is still there, and \
             this command cannot finish the deletion. Remove it by hand with:\n      rm -rf -- {}",
            package.dir.display(),
            shell_quote(&package.dir)
        ))
    })?;

    println!();
    println!("removed {} from {}", name.as_str(), package.dir.display());
    println!();
    println!(
        "disabling '{}' already took it out of the build, so deleting it changes nothing the \
         module compiles. Two steps may still be outstanding from that disable, and this command \
         ran neither:",
        name.as_str()
    );
    println!(
        "  lyracore publish       publish the module WITHOUT the Package to every database of \
         this realm"
    );
    println!(
        "  lyracore client sync   repack the client; it warns about an addon this Package left \
         behind"
    );
    Ok(())
}

/// The one installed Package with this exact folder name, from either inventory.
///
/// Exact, not folded: these commands move or delete a FOLDER, and picking a near-miss for the
/// operator would act on a directory they did not name. A fold-equal Package is named in the error
/// instead, because it is almost always the one they meant.
fn find(project: &ProjectLayout, name: &PackageName) -> Result<InstalledPackage> {
    let mut folded = None;
    for installed in inventory(project)? {
        if installed.name == *name {
            return Ok(installed);
        }
        if installed.name.rust_ident() == name.rust_ident() {
            folded = Some(installed);
        }
    }
    Err(Error::Usage(match folded {
        Some(near) => format!(
            "no Package called '{}'. The {} Package '{}' folds onto the same module `pkg_{}`, but \
             these commands move a folder, so its name has to be typed exactly. Nothing was \
             changed.",
            name.as_str(),
            near.state.as_str(),
            near.name.as_str(),
            name.rust_ident()
        ),
        None => format!(
            "no Package called '{}'. `lyracore packages list` shows every installed Package in \
             both inventories. Nothing was changed.",
            name.as_str()
        ),
    }))
}

/// Rename one Package directory into the other inventory, refusing every collision first.
///
/// Returns the `from`/`to` report lines, so the caller prints one shape whichever way it moved.
fn move_package(
    project: &ProjectLayout,
    package: &InstalledPackage,
    destination: &Path,
    verb: &str,
) -> Result<String> {
    // The Package being moved is still in the inventory it is leaving, hence `ignoring`. What is
    // left to catch is the other inventory holding a DIFFERENT folder that folds onto the same
    // generated module, which the destination path alone would not reveal.
    if let Some(existing) = collision(project, &package.name, Some(&package.dir))? {
        return Err(Error::Usage(format!(
            "cannot {verb} '{}': {}. It is at {}. Nothing was moved.",
            package.name.as_str(),
            collision_reason(&existing, &package.name),
            existing.dir.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        Error::State(format!(
            "Package destination {} has no parent directory",
            destination.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    super::rename_no_replace(&package.dir, destination).map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            Error::Usage(format!(
                "cannot {verb} '{}': {} appeared while this command was running. Nothing was \
                 merged or overwritten.",
                package.name.as_str(),
                destination.display()
            ))
        } else {
            Error::Process(format!(
                "could not move {} to {}: {error}. The Package is still where it was, so nothing \
                 the build sees has changed.",
                package.dir.display(),
                destination.display()
            ))
        }
    })?;
    Ok(format!(
        "  from      {}\n  to        {}\n",
        package.dir.display(),
        destination.display()
    ))
}

/// Refuse a Package whose tree is not the one its stamp recorded.
///
/// Two refusals, one reason: `remove` may only delete content that exists somewhere else as well.
/// A tree that has drifted from its stamp holds the operator's own edits, and a tree with no
/// readable stamp cannot be shown to hold anything but them.
fn check_recorded_and_clean(package: &InstalledPackage) -> Result<()> {
    let recorded = package
        .stamp
        .as_ref()
        .filter(|stamp| !stamp.content_identity.is_empty())
        .ok_or_else(|| {
            Error::Usage(format!(
                "cannot remove '{}': it has no readable provenance stamp recording a content \
                 identity, so this command cannot tell installed content from local work. It was \
                 created by hand, predates `lyracore packages add`, or its stamp was edited. \
                 Delete it by hand if you are sure:\n      rm -rf -- {}\nNothing was deleted.",
                package.name.as_str(),
                shell_quote(&package.dir)
            ))
        })?;
    let current = stamp::content_identity(&package.dir)?;
    if current != recorded.content_identity {
        return Err(Error::Usage(format!(
            "cannot remove '{}': the folder no longer matches what was installed (stamp {}, on \
             disk {current}). Those local changes exist only here, and this command cannot get \
             them back. Copy them somewhere outside the checkout first, then remove it. Nothing \
             was deleted.",
            package.name.as_str(),
            recorded.content_identity
        )));
    }
    Ok(())
}

/// What a disabled Package's tables mean for the next publish.
///
/// Disabling takes the tables out of the Module schema, so the next publish is a schema change that
/// drops them. `lyracore publish` never passes SpacetimeDB's destructive wipe flag (see
/// `cmd::publish`), so that publish stops rather than deleting rows. This informs; it does not
/// block. Refusing to disable a Package because its tables hold rows would leave the operator no
/// way to take a Package out of the build at all.
fn table_warning(name: &PackageName, tables: &[String]) -> String {
    format!(
        "'{}' registers {} table(s) in the Module schema:\n      {}\n  Disabling it takes them \
         out of that schema, so the next `lyracore publish` is a\n  schema change that removes \
         them. `lyracore publish` never passes SpacetimeDB's\n  destructive wipe flag, so a \
         publish that would drop a table still holding rows\n  STOPS instead of deleting them. \
         The rows are intact if that happens, and\n  `lyracore packages enable {}` puts the \
         tables back.\n",
        name.as_str(),
        tables.len(),
        tables.join(", "),
        name.as_str()
    )
}

/// What still holds a copy of a Package about to be deleted.
///
/// The stamp is the only record of that, and it is the difference between a deletion the operator
/// can undo with one `packages add` and one they cannot undo at all.
fn recovery_note(stamp: Option<&stamp::ProvenanceStamp>) -> String {
    match stamp {
        Some(recorded) if recorded.source_kind == SOURCE_LOCAL && !recorded.source.is_empty() => {
            format!(
                "This command does not touch the Package Source it was installed from, so it can \
                 be\ninstalled again with:\n      lyracore packages add {}\n",
                shell_quote(Path::new(&recorded.source))
            )
        }
        // A scaffold, or a stamp that records no usable source: nothing outside this checkout is
        // known to hold the folder.
        _ => "Nothing outside this checkout is recorded as holding a copy of this Package. \
              Deleting it is final.\n"
            .to_string(),
    }
}

/// The `client sync` line, which says whether there is any client content to sync at all.
fn client_sync_step(review: &TrustReview) -> String {
    if review.addons.is_empty() && review.client_overrides == 0 {
        "lyracore client sync   not needed: this Package ships no client content".to_string()
    } else {
        format!(
            "lyracore client sync   repack the client for its {} addon(s) and {} client \
             override(s)",
            review.addons.len(),
            review.client_overrides
        )
    }
}

#[cfg(test)]
mod tests {
    use super::stamp::ProvenanceStamp;
    use super::*;
    use crate::cmd::packages::add;
    use crate::cmd::packages::tests::{candidate, checkout, Answer};
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    /// Install `name` from a folder outside the checkout, the way an operator would, so every
    /// lifecycle test starts from a real stamped Package rather than a hand-built directory.
    fn installed(tmp: &TempDir, project: &ProjectLayout, name: &str) {
        let source = candidate(tmp, name);
        add(
            project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();
    }

    #[test]
    fn disabling_moves_the_folder_out_of_the_build_and_enabling_moves_it_back() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        let enabled = project.packages_dir().join("greeter");
        let disabled = project.packages_disabled_dir().join("greeter");

        disable(&project, "greeter").unwrap();

        assert!(!enabled.exists(), "the build must not still see it");
        assert!(disabled.join("src/mod.rs").is_file());
        assert_eq!(
            inventory(&project).unwrap()[0].state,
            PackageState::Disabled
        );

        enable(&project, "greeter").unwrap();

        assert!(enabled.join("src/mod.rs").is_file());
        assert!(!disabled.exists());
        assert_eq!(inventory(&project).unwrap()[0].state, PackageState::Enabled);
    }

    #[test]
    fn a_move_carries_the_provenance_stamp_and_the_content_with_it() {
        // The stamp lives inside the folder for exactly this reason. If a move rewrote or dropped
        // it, a re-enabled Package would report itself as unrecorded or as locally drifted.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        let before = ProvenanceStamp::read(&project.packages_dir().join("greeter")).unwrap();

        disable(&project, "greeter").unwrap();
        let disabled = project.packages_disabled_dir().join("greeter");
        assert_eq!(ProvenanceStamp::read(&disabled), Some(before.clone()));
        assert_eq!(
            stamp::content_identity(&disabled).unwrap(),
            before.content_identity,
            "a disabled Package must not read as locally drifted"
        );

        enable(&project, "greeter").unwrap();
        let enabled = project.packages_dir().join("greeter");
        assert_eq!(ProvenanceStamp::read(&enabled), Some(before.clone()));
        assert_eq!(
            stamp::content_identity(&enabled).unwrap(),
            before.content_identity
        );
    }

    #[test]
    fn enabling_a_name_the_enabled_inventory_folds_onto_moves_nothing() {
        // `packages/foo-bar` and a disabled `foo_bar` both generate `pkg_foo_bar`. The destination
        // paths differ, so only the fold check catches this.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("foo-bar/src")).unwrap();
        let disabled = project.packages_disabled_dir().join("foo_bar");
        std::fs::create_dir_all(disabled.join("src")).unwrap();

        let error = enable(&project, "foo_bar").unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("pkg_foo_bar"), "{error}");
        assert!(disabled.is_dir(), "nothing may move: {error}");
        assert!(!project.packages_dir().join("foo_bar").exists(), "{error}");
    }

    #[test]
    fn a_package_that_is_already_in_the_destination_state_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");

        let error = enable(&project, "greeter").unwrap_err();
        assert!(error.to_string().contains("already enabled"), "{error}");

        disable(&project, "greeter").unwrap();
        let error = disable(&project, "greeter").unwrap_err();
        assert!(error.to_string().contains("already disabled"), "{error}");
    }

    #[test]
    fn an_unknown_name_names_the_folded_near_miss_rather_than_acting_on_it() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");

        let error = disable(&project, "absent").unwrap_err();
        assert!(error.to_string().contains("packages list"), "{error}");

        std::fs::rename(
            project.packages_dir().join("greeter"),
            project.packages_dir().join("my-package"),
        )
        .unwrap();
        let error = disable(&project, "my_package").unwrap_err();
        assert!(error.to_string().contains("'my-package'"), "{error}");
        assert!(
            project.packages_dir().join("my-package").is_dir(),
            "the near miss must not be moved: {error}"
        );
    }

    #[test]
    fn disabling_a_package_with_tables_warns_about_publication_before_it_moves() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        let review = TrustReview::scan(&project.packages_dir().join("greeter")).unwrap();
        assert_eq!(review.tables, ["pkg_greeter_log"], "fixture check");

        let warning = table_warning(&PackageName::parse("greeter").unwrap(), &review.tables);

        assert!(warning.contains("pkg_greeter_log"), "{warning}");
        assert!(warning.contains("still holding rows"), "{warning}");
        // Informs, never blocks: the move itself must still happen.
        disable(&project, "greeter").unwrap();
        assert!(project.packages_disabled_dir().join("greeter").is_dir());
    }

    #[test]
    fn removing_deletes_a_confirmed_disabled_package_and_nothing_else() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        installed(&tmp, &project, "keeper");
        let source = tmp.path().join("sources/greeter");
        disable(&project, "greeter").unwrap();

        remove(&project, &Answer("yes"), "greeter", false).unwrap();

        assert!(!project.packages_disabled_dir().join("greeter").exists());
        assert!(
            project.packages_dir().join("keeper/src/mod.rs").is_file(),
            "removing one Package must not touch another"
        );
        assert!(
            source.join("src/mod.rs").is_file(),
            "the Package Source is not this command's to delete"
        );
    }

    #[test]
    fn removing_an_enabled_package_is_refused_and_points_at_disable() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");

        let error = remove(&project, &Answer("yes"), "greeter", true).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(
            error.to_string().contains("packages disable greeter"),
            "{error}"
        );
        assert!(project.packages_dir().join("greeter").is_dir(), "{error}");
    }

    #[test]
    fn removing_a_locally_changed_package_is_refused_before_the_question_is_asked() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        disable(&project, "greeter").unwrap();
        let disabled = project.packages_disabled_dir().join("greeter");
        std::fs::write(disabled.join("src/mod.rs"), "// hours of local work\n").unwrap();

        // `--yes`, so a refusal cannot be the prompt's doing: the drift check is what stops it.
        let error = remove(&project, &Answer("yes"), "greeter", true).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("no longer matches"), "{error}");
        assert!(disabled.join("src/mod.rs").is_file(), "{error}");
    }

    #[test]
    fn removing_a_package_with_no_recorded_identity_is_refused() {
        // A folder somebody dropped into the inventory by hand: nothing says which of its bytes
        // were ever installed from anywhere, so all of them have to be treated as local work.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let handmade = project.packages_disabled_dir().join("handmade");
        std::fs::create_dir_all(handmade.join("src")).unwrap();
        std::fs::write(handmade.join("src/mod.rs"), "pub fn helper() {}\n").unwrap();

        let error = remove(&project, &Answer("yes"), "handmade", true).unwrap_err();

        assert!(error.to_string().contains("provenance stamp"), "{error}");
        assert!(
            error.to_string().contains(&shell_quote(&handmade)),
            "the by-hand remedy must name the exact path: {error}"
        );
        assert!(handmade.join("src/mod.rs").is_file(), "{error}");
    }

    #[test]
    fn an_answer_other_than_yes_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        installed(&tmp, &project, "greeter");
        disable(&project, "greeter").unwrap();
        let disabled = project.packages_disabled_dir().join("greeter");

        let error = remove(&project, &Answer("no"), "greeter", false).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(error.to_string().contains("Nothing was deleted"), "{error}");
        assert!(disabled.join("src/mod.rs").is_file(), "{error}");
    }

    #[test]
    fn the_recovery_note_offers_reinstalling_only_when_a_package_source_holds_a_copy() {
        let local = ProvenanceStamp::local(Path::new("/home/dev/src/greeter"), String::new(), 0);
        let note = recovery_note(Some(&local));
        assert!(note.contains("/home/dev/src/greeter"), "{note}");
        assert!(note.contains("packages add"), "{note}");

        // A scaffold was copied out of this checkout and renamed. Re-adding it would not bring
        // back the Package that is being deleted.
        let scaffold = ProvenanceStamp::scaffolded("packages/example/", String::new(), 0);
        assert!(recovery_note(Some(&scaffold)).contains("final"));
        assert!(recovery_note(None).contains("final"));
    }

    #[test]
    fn no_lifecycle_verb_publishes_or_synchronizes_a_client() {
        // The whole surface, driven end to end: every remaining step is PRINTED, never run. These
        // commands take no ProcessRunner at all, which is the structural half of that promise;
        // this pins the behavioural half.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();
        let source = candidate(&tmp, "greeter");
        add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();
        let calls_after_add = stack.rendered().len();

        disable(&project, "greeter").unwrap();
        enable(&project, "greeter").unwrap();
        disable(&project, "greeter").unwrap();
        remove(&project, &Answer("yes"), "greeter", true).unwrap();

        assert_eq!(
            stack.rendered().len(),
            calls_after_add,
            "a lifecycle verb ran a process: {:?}",
            stack.rendered()
        );
    }
}
