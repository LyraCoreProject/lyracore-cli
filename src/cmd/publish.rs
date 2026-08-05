//! `lyracore publish [DATABASE …]` — deploy the SpacetimeDB module to the LOCAL node, the ONE
//! correct way.
//!
//! This absorbs the server repo's `scripts/publish-module.sh`. Two flags are baked in and one is
//! unreachable:
//!
//! * `--build-options=--features=debug_reducers` — the debug module (`game_debug_readout` and
//!   friends) is behind that Cargo feature. A plain build omits it, so publish reports a FALSE
//!   "Removed table" breaking change and aborts. With the feature the build matches the deployed
//!   (debug) schema, so there is no false alarm.
//! * `--yes` — SpacetimeDB prompts "All clients will be disconnected" for ANY schema change, even
//!   an additive END-appended `#[default]` column. It confirms ONLY that disconnect warning; it
//!   does NOT bypass the data-destructive path. Without it, a non-interactive stdin makes even a
//!   safe additive migration abort on EOF.
//! * `-c` / `--delete-data` — the destructive wipe (docs/danger-zones.md §1). It is not baked in,
//!   and it cannot be passed through: this command's arguments are database NAMES, and anything
//!   flag-shaped is REFUSED before a single `spacetime` process is started.
//!
//! WHY IT TAKES DATABASE NAMES: a schema change needs EVERY shard republished. The original
//! hardcoded one database, so every other shard was published by hand-typing the raw command — and
//! the documented form of that dropped `--yes`. That is how two shards were once left behind
//! missing a table while the others were current, which the gateway reported only as "realm-core
//! unreachable — LOGONS WILL BE REFUSED". Passing them all in one command is what makes "all of
//! them" checkable instead of remembered.

use crate::cmd::preflight;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// Flags that would destroy data. They are refused by the general flag guard below; naming them
/// buys a message that says WHY rather than "that is not a database name".
const DESTRUCTIVE: [&str; 4] = ["-c", "--clear-database", "--delete-data", "--clear"];

/// Reject anything that is not a plain database name.
///
/// This is the whole safety contract of the command, so it lives in one function that both argument
/// parsing and command rendering go through — a database name cannot reach a rendered
/// `spacetime publish` without passing here.
pub fn validate_database(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Usage(
            "`lyracore publish` takes database NAMES; got an empty one. Refusing.".to_string(),
        ));
    }
    if DESTRUCTIVE.contains(&name) {
        return Err(Error::Usage(format!(
            "`lyracore publish` takes database NAMES, not flags (got '{name}'). Refusing.\n  \
             That flag is the DESTRUCTIVE wipe: it deletes every row and breaks login until the \
             realm is re-provisioned. This CLI has no path to it, by design."
        )));
    }
    if name.starts_with('-') {
        return Err(Error::Usage(format!(
            "`lyracore publish` takes database NAMES, not flags (got '{name}'). Refusing.\n  \
             Forwarding an unexamined flag to `spacetime publish` is the one unrecoverable mistake \
             available here, so no flag is forwarded at all."
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Usage(format!(
            "'{name}' is not a database name (expected letters, digits, `-` or `_`). Refusing."
        )));
    }
    Ok(())
}

/// Which databases a bare argument list means. No arguments is the single seeded fixture database.
pub fn databases(args: &[String]) -> Result<Vec<String>> {
    if args.is_empty() {
        return Ok(vec![ProjectLayout::DATABASE.to_string()]);
    }
    for name in args {
        validate_database(name)?;
    }
    Ok(args.to_vec())
}

/// The ONE correct publish invocation.
pub fn publish_command(project: &ProjectLayout, database: &str) -> Result<CommandSpec> {
    validate_database(database)?;
    Ok(CommandSpec::new("spacetime")
        .arg("publish")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg("-p")
        // Absolute, so `lyracore` publishes this checkout's module from any subdirectory of it.
        .arg(project.module_dir().to_string_lossy().to_string())
        .arg(format!(
            "--build-options={}",
            ProjectLayout::DEPLOY_FEATURES
        ))
        .arg("--yes")
        .arg(database))
}

/// Publish to each database in turn, stopping at the first failure.
///
/// Fail-fast is deliberate: a half-published realm is the state the gateway reports as an unrelated
/// mid-session hang, so continuing past a failure would bury the one line that explains it.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    databases: &[String],
    skip_preflight: bool,
) -> Result<()> {
    if skip_preflight {
        println!(
            "· skipping preflight (--skip-preflight): nothing has validated this module's schema, \
             #[default] encodings or visibility filters."
        );
    } else {
        preflight::run(project, runner)?;
    }

    for database in databases {
        println!();
        println!("==> publishing to {database}");
        runner.run_streaming(&publish_command(project, database)?)?;
    }

    println!();
    println!("published to: {}", databases.join(" "));
    if databases == [ProjectLayout::DATABASE] {
        println!(
            "NOTE: only {}. A SCHEMA change must be published to every shard of a sharded realm, \
             or the gateway",
            ProjectLayout::DATABASE
        );
        println!("      refuses logons on the ones left behind. See docs/danger-zones.md §3.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn names(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    // ---- the guard contract, case for case with the shell script's CI job ----

    #[test]
    fn a_bare_dash_c_is_refused_rather_than_forwarded() {
        // `spacetime publish -c` is the destructive wipe. This is the single reason the wrapper
        // exists, so it is asserted FUNCTIONALLY: the refusal happens before any process starts.
        let error = databases(&names(&["-c"])).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("Refusing"), "{error}");
        assert!(error.to_string().contains("DESTRUCTIVE"), "{error}");
    }

    #[test]
    fn every_other_flag_shaped_argument_is_refused_the_same_way() {
        for flag in [
            "--clear",
            "--delete-data",
            "--clear-database",
            "-y",
            "--anything",
            "-",
        ] {
            let error = databases(&names(&[flag])).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "{flag} must be a usage error"
            );
            assert!(error.to_string().contains("Refusing"), "{flag}: {error}");
        }
    }

    #[test]
    fn a_flag_hidden_after_a_valid_database_is_still_refused() {
        // The dangerous shape: `lyracore publish lyracore -c` — one good argument in front of a
        // wipe. Nothing may be published before the whole list has been checked.
        let error = databases(&names(&["lyracore", "-c"])).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("Refusing"), "{error}");
    }

    #[test]
    fn a_flag_cannot_be_smuggled_past_argument_parsing_into_a_rendered_command() {
        // Defence in depth: even a caller constructing the command directly is refused.
        let tmp = TempDir::new().unwrap();
        assert!(publish_command(&project(&tmp), "-c").is_err());
        assert!(publish_command(&project(&tmp), "--delete-data").is_err());
        assert!(publish_command(&project(&tmp), "a database").is_err());
    }

    #[test]
    fn plain_database_names_are_accepted() {
        assert_eq!(
            databases(&names(&["lyracore-world-1", "realm_core"])).unwrap(),
            names(&["lyracore-world-1", "realm_core"])
        );
    }

    #[test]
    fn no_arguments_means_the_single_seeded_fixture_database() {
        assert_eq!(databases(&[]).unwrap(), vec![ProjectLayout::DATABASE]);
    }

    // ---- what the rendered command must always contain ----

    #[test]
    fn the_two_flags_the_wrapper_exists_for_are_always_present_and_the_wipe_never_is() {
        let tmp = TempDir::new().unwrap();
        let rendered = publish_command(&project(&tmp), "lyracore")
            .unwrap()
            .render();
        assert!(
            rendered.contains("--build-options=--features=debug_reducers"),
            "{rendered}"
        );
        assert!(rendered.contains("--yes"), "{rendered}");
        let args: Vec<&str> = rendered.split_whitespace().collect();
        for wipe in ["-c", "--clear-database", "--delete-data", "--clear"] {
            assert!(
                !args.contains(&wipe),
                "{wipe} must never be baked in: {rendered}"
            );
        }
        // The server alias is pinned and never re-selected.
        assert!(rendered.contains("-s local"), "{rendered}");
        assert!(!rendered.contains("server set-default"), "{rendered}");
    }

    #[test]
    fn the_module_path_is_absolute_so_the_cwd_cannot_decide_what_is_published() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let rendered = publish_command(&project, "lyracore").unwrap().render();
        assert!(
            rendered.contains(&project.module_dir().to_string_lossy().to_string()),
            "{rendered}"
        );
    }

    // ---- the run ----

    /// A checkout `preflight` passes, so these tests exercise `publish`'s own behaviour.
    fn healthy(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!(
                "[toolchain]\nchannel = \"{}\"\n",
                crate::proc::fake::FAKE_RUST_VERSION
            ),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!(
                "spacetimedb = {{ version = \"={}\" }}\n",
                crate::proc::fake::FAKE_SPACETIME_VERSION
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        ProjectLayout::from_root(root).unwrap()
    }

    #[test]
    fn several_databases_publish_in_order() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        let wanted = names(&["lyracore", "lyracore-world-1", "realm-core"]);
        run(&healthy(&tmp), &stack.runner(), &wanted, true).unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(published.len(), 3, "{published:?}");
        for (rendered, database) in published.iter().zip(&wanted) {
            assert!(rendered.ends_with(database), "{rendered}");
            assert!(rendered.contains("--yes"), "{rendered}");
            assert!(
                rendered.contains("--features=debug_reducers"),
                "every shard gets the deploy features, not just the first: {rendered}"
            );
        }
    }

    #[test]
    fn a_failed_shard_stops_the_run_instead_of_leaving_a_half_published_realm() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new().fail_on("lyracore-world-1", "migration rejected");
        let wanted = names(&["lyracore", "lyracore-world-1", "realm-core"]);

        let error = run(&healthy(&tmp), &stack.runner(), &wanted, true).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(
            !stack.rendered().iter().any(|r| r.ends_with("realm-core")),
            "the run must stop at the failure: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn preflight_runs_first_and_a_failing_one_publishes_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        std::fs::write(project.rust_toolchain_file(), "channel = \"1.0.0\"\n").unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner(), &names(&["lyracore"]), false).unwrap_err();
        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime publish")),
            "a failing preflight must publish nothing: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn preflight_precedes_the_publish_rather_than_following_it() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        run(
            &healthy(&tmp),
            &stack.runner(),
            &names(&["lyracore"]),
            false,
        )
        .unwrap();

        let rendered = stack.rendered();
        let first_publish = rendered
            .iter()
            .position(|r| r.contains("spacetime publish"))
            .expect("published");
        let last_check = rendered
            .iter()
            .rposition(|r| r.contains("cargo check") || r.contains("spacetime generate"))
            .expect("preflighted");
        assert!(last_check < first_publish, "{rendered:?}");
    }

    #[test]
    fn skip_preflight_is_the_only_way_past_the_gate() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        // A checkout preflight would reject...
        std::fs::write(project.rust_toolchain_file(), "channel = \"1.0.0\"\n").unwrap();
        let stack = FakeStack::new();
        run(&project, &stack.runner(), &names(&["lyracore"]), true).unwrap();
        assert!(
            stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime publish")),
            "{:?}",
            stack.rendered()
        );
        assert!(
            !stack.rendered().iter().any(|r| r.contains("cargo check")),
            "--skip-preflight must actually skip it: {:?}",
            stack.rendered()
        );
    }
}
