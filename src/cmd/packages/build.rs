//! `lyracore packages build` — regenerate the Datascript typings, then typecheck against them.
//!
//! A version gate, then three steps, in this order, and the order is the contract:
//!
//! 0. `bun --version` must match the checkout's exact pin. A hard failure, unlike `doctor`'s Bun
//!    check: `doctor` only reports drift on a machine that may never run Bun at all, but this
//!    command is about to run `bun install` and the locked `tsc` for real, against whatever `bun`
//!    is on PATH. Verifying first means a stale or missing Bun fails with the exact reinstall
//!    command instead of a confusing `bun install` or `tsc` error two steps later.
//! 1. `spacetime generate --lang typescript` extracts the Module schema THROUGH the wasm and writes
//!    it to `datascripts/generated/`. Offline: it builds the module and reads it, and touches no
//!    database.
//! 2. `bun install --frozen-lockfile` installs exactly what `datascripts/bun.lock` records. Frozen,
//!    not merely locked: a build that silently resolved a newer dependency would typecheck against
//!    a library the next author does not have.
//! 3. Bun runs the locked, project-local TypeScript compiler with `--noEmit`. Nothing is emitted —
//!    the answer is the exit code.
//!
//! Steps 1-3 STREAM to the terminal rather than being captured. `tsc` writes its diagnostics to
//! stdout, so a captured run surfaced an empty error and lost the file and line this command
//! promises; the other two are chatty enough (a cargo build, a dependency install) that live
//! progress beats a held-back transcript. Step 0 captures instead: a `--version` banner is one
//! line, and the gate needs to read it, not relay it.
//!
//! Generating FIRST among the streamed steps is what gives the gate teeth. The typings are derived
//! from the Module every run, so a Datascript naming a column the Module renamed fails here, at
//! author time, rather than surviving into a Package Delta.
//!
//! WHY THE TYPINGS ARE NOT COMMITTED: they are a 400-file, 2 MB projection of the Module wasm. A
//! committed copy would put a large mechanical diff in every schema change and, worse, would be a
//! second source of truth that can disagree with the Module. `generated/` is git-ignored and this
//! command reproduces it.
//!
//! Bun is needed HERE and nowhere else. An Operator applying a prebuilt Package Delta runs no part
//! of this, which is why `doctor`'s Bun check is a warning rather than a launch blocker.

use crate::cmd::doctor::{self, BunVersionCheck};
use crate::cmd::preflight;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// Extract the Module schema as TypeScript.
///
/// The same deploy feature set `preflight` and `publish` build under, so the typings describe the
/// module that actually ships rather than a plain-build variant of it. Installed Packages need no
/// mention: `module/build.rs` compiles every enabled Package into the same wasm, so their tables
/// are in the schema this reads.
pub fn typegen_command(project: &ProjectLayout) -> CommandSpec {
    preflight::schema_command_for_language(project, &project.datascript_types_dir(), "typescript")
}

/// Install exactly what the lockfile records, and fail rather than update it.
pub fn install_command(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("bun")
        .arg("install")
        .arg("--frozen-lockfile")
        .cwd(project.datascripts_dir())
}

/// The gate. `--noEmit`: this build produces typings and a verdict, never JavaScript.
pub fn typecheck_command(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("bun")
        // Do not use `bun x tsc`: bunx may fall back to npm/global cache when the local binary is
        // absent, and TypeScript's node shebang may then require an unreported Node runtime.
        .arg("./node_modules/typescript/bin/tsc")
        .arg("--noEmit")
        .cwd(project.datascripts_dir())
}

/// Verify Bun before anything is installed or typechecked with it. Missing, mismatched, and
/// unparsable banners all take the exact wording `doctor`'s Bun check reports as a warning — this
/// caller's own wording is only "hard failure" versus "warning", not a second explanation.
fn verify_bun(runner: &dyn ProcessRunner) -> Result<()> {
    let banner = runner
        .run_capturing_stderr(&CommandSpec::new("bun").arg("--version"))
        .ok();
    let install = doctor::bun_install_hint();
    match doctor::bun_version_check(banner.as_deref()) {
        BunVersionCheck::Pinned(_) => Ok(()),
        BunVersionCheck::Missing => Err(Error::PrerequisiteMissing(format!(
            "`bun` not found. `packages build` needs the pinned {} to install and typecheck \
             Datascripts; install it with `{install}`",
            doctor::REQUIRED_BUN
        ))),
        BunVersionCheck::Mismatched(found) => Err(Error::PrerequisiteMissing(format!(
            "bun reports {found}, but this checkout's Datascript toolchain is pinned to {}. \
             `packages build` is not verified against a different Bun runtime; install the pinned \
             version with `{install}`",
            doctor::REQUIRED_BUN
        ))),
        BunVersionCheck::Unparsable => Err(Error::PrerequisiteMissing(format!(
            "could not read a version from `bun --version`; expected exactly {}. Reinstall it \
             with `{install}`",
            doctor::REQUIRED_BUN
        ))),
    }
}

pub fn run(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    let datascripts = project.datascripts_dir();
    let missing = ["package.json", "bun.lock", "tsconfig.json"]
        .into_iter()
        .filter(|name| !datascripts.join(name).is_file())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(Error::PrerequisiteMissing(format!(
            "the Datascript project at {} is incomplete (missing {}). `packages build` needs the \
             checked-in Bun project before it can regenerate datascripts/generated/ and \
             typecheck the Datascripts. Restore the missing files from this checkout's branch.",
            datascripts.display(),
            missing.join(", ")
        )));
    }

    verify_bun(runner)?;

    println!(
        "regenerating Module schema typings -> {}",
        project.datascript_types_dir().display()
    );
    runner
        .run_streaming(&typegen_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "could not extract the Module schema as TypeScript. This builds the module wasm and \
                reads it — the same step `lyracore preflight` runs — so a module that does not \
                 compile fails here first. Nothing was typechecked.\n  ({e})"
            ))
        })?;
    println!("installing the pinned Datascript dependencies (bun.lock)");
    runner
        .run_streaming(&install_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "`bun install --frozen-lockfile` failed in {}. The lockfile is the pin: if a \
             dependency in package.json changed, run `bun install` there yourself and commit the \
             updated bun.lock. Nothing was typechecked.\n  ({e})",
                datascripts.display()
            ))
        })?;

    println!("typechecking the Datascripts against the regenerated typings");
    runner
        .run_streaming(&typecheck_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "the Datascripts do not typecheck against the current Module schema. This is the \
                 gate doing its job — the errors above name the file, line and column to fix. A \
                 column the Module renamed, retyped or removed shows up there.\n  ({e})"
            ))
        })?;

    println!();
    println!("Datascripts typecheck against the current Module schema.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    /// A checkout carrying the committed Datascript project.
    fn checkout(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path().join("checkout");
        std::fs::create_dir_all(root.join("datascripts/src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("datascripts/package.json"), "{}\n").unwrap();
        std::fs::write(root.join("datascripts/bun.lock"), "{}\n").unwrap();
        std::fs::write(root.join("datascripts/tsconfig.json"), "{}\n").unwrap();
        ProjectLayout::from_root(&root).unwrap()
    }

    #[test]
    fn a_build_verifies_bun_generates_typings_installs_the_lockfile_then_typechecks_in_that_order()
    {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let calls = stack.rendered();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert!(calls[0].starts_with("bun --version"), "{calls:?}");
        assert!(calls[1].contains("spacetime generate"), "{calls:?}");
        assert!(calls[1].contains("--lang typescript"), "{calls:?}");
        assert!(
            calls[2].contains("bun install --frozen-lockfile"),
            "{calls:?}"
        );
        assert_eq!(
            calls[3], "bun ./node_modules/typescript/bin/tsc --noEmit",
            "{calls:?}"
        );
    }

    #[test]
    fn the_typings_are_generated_into_the_stable_author_facing_location() {
        // Datascripts import from this path by name. Moving it silently would break every one.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let rendered = typegen_command(&project).render();

        assert!(rendered.contains("datascripts/generated"), "{rendered}");
        assert!(rendered.contains("--no-config"), "{rendered}");
        assert_eq!(
            typegen_command(&project).cwd_value(),
            Some(project.root.as_path())
        );
        // The deploy feature set, so the typings describe the module that actually publishes.
        assert!(
            rendered.contains(ProjectLayout::DEPLOY_FEATURES),
            "{rendered}"
        );
    }

    #[test]
    fn bun_runs_inside_the_datascript_project_not_the_callers_directory() {
        // `lyracore` runs from any subdirectory of a checkout; a bun that inherited the caller's
        // cwd would read whatever package.json happened to be above it.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        for cmd in [install_command(&project), typecheck_command(&project)] {
            assert_eq!(cmd.cwd_value(), Some(project.datascripts_dir().as_path()));
        }
    }

    #[test]
    fn a_failed_typegen_stops_before_anything_is_typechecked() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().fail_on("spacetime generate", "the module does not compile");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("Module schema"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("tsc"), "{call}");
        }
    }

    #[test]
    fn a_lockfile_that_no_longer_matches_package_json_fails_with_the_fix() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().fail_on("bun install", "lockfile had changes");

        let error = run(&project, &stack.runner()).unwrap_err();

        // The author is holding a lockfile that does not match; the exact command is the one
        // thing they need, and `packages build` deliberately does not run it for them.
        assert!(error.to_string().contains("commit the"), "{error}");
        assert!(error.to_string().contains("bun.lock"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("tsc"), "{call}");
        }
    }

    #[test]
    fn a_datascript_naming_a_renamed_column_fails_the_build() {
        // The acceptance behaviour, at this seam: whatever `tsc` refuses, `packages build` refuses.
        // `tsc`'s own diagnostics reach the terminal directly — it writes them to stdout, and this
        // step streams — so the error here supplies the reading, not a copy of the transcript.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().fail_on("tsc", "exit status 1");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("do not typecheck"), "{error}");
        assert!(error.to_string().contains("errors above"), "{error}");
    }

    #[test]
    fn a_checkout_without_the_datascript_project_is_an_operational_failure() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("bare");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(&root).unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE, "{error}");
        assert!(error.to_string().contains("package.json"), "{error}");
        assert!(error.to_string().contains("bun.lock"), "{error}");
        assert!(error.to_string().contains("tsconfig.json"), "{error}");
        assert!(stack.rendered().is_empty(), "{error}");
    }

    #[test]
    fn every_required_datascript_project_file_is_checked_before_typegen() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::remove_file(project.datascripts_dir().join("tsconfig.json")).unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("tsconfig.json"), "{error}");
        assert!(stack.rendered().is_empty(), "{error}");
    }

    // ---- the Bun version gate ----

    #[test]
    fn a_missing_bun_fails_before_anything_is_generated_or_installed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().fail_on("bun --version", "command not found");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE, "{error}");
        assert!(error.to_string().contains("bun-v1.3.7"), "{error}");
        assert!(stack.rendered().iter().all(|call| call == "bun --version"));
    }

    #[test]
    fn a_bun_older_than_the_pin_fails_the_build() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().with_stdout("bun --version", "1.3.6");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("1.3.7"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("spacetime generate"), "{call}");
        }
    }

    #[test]
    fn a_bun_newer_than_the_pin_also_fails_the_build() {
        // The pin is exact, not a floor: a Datascript's typecheck is only verified against 1.3.7.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().with_stdout("bun --version", "1.4.0");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("bun-v1.3.7"), "{error}");
    }

    #[test]
    fn a_malformed_bun_banner_fails_the_build_rather_than_guessing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().with_stdout("bun --version", "bun 1.3.7");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(
            error.to_string().contains("could not read a version"),
            "{error}"
        );
        assert!(error.to_string().contains("bun-v1.3.7"), "{error}");
    }

    #[test]
    fn the_pinned_bun_lets_the_build_proceed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().with_stdout("bun --version", "1.3.7");

        run(&project, &stack.runner()).unwrap();
    }

    #[test]
    fn the_build_never_publishes_and_never_touches_a_database() {
        // `packages build` is author-time only: it produces typings and a verdict, nothing else.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("spacetime call"), "{call}");
            assert!(!call.contains("spacetime sql"), "{call}");
        }
    }
}
