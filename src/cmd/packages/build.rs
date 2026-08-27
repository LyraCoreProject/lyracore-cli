//! `lyracore packages build` — regenerate the Datascript typings, typecheck against them, then run
//! and validate every enabled Package's Datascripts.
//!
//! A version gate, then up to seven steps, in this order, and the order is the contract:
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
//! 4. The Base Snapshot check. Skipped entirely when no enabled Package carries a Datascript — a
//!    checkout with no Datascripts builds exactly as it did before this step existed. Otherwise,
//!    `datascripts/generated/base-snapshot.json` must already exist, or the build fails fast with
//!    the exact `lyracore-importer --spell-snapshot` command to build one, rather than letting every
//!    Datascript fail with the same confusing "cannot read" one at a time.
//! 5. Every enabled Package with a `datascripts/src/<package>/` folder runs each `.ts` file there,
//!    in name order, as its own `bun run` SUBPROCESS — never imported. The library hashes
//!    `Bun.main`, the running process's own entry script, into the artifact's `source_hash`;
//!    importing the script into one host process instead would hash the host, not the script. The
//!    first script to fail stops the build: later scripts, and later Packages, never run.
//! 6. `cargo run -p lyracore-package-delta --bin lyracore-delta-check` traces every enabled
//!    Package's generated artifacts TOGETHER, in one invocation. A Claim Conflict is between two
//!    Packages, so checking one artifact at a time could only ever prove that one artifact parses.
//!    This is the authoritative Rust-side check — the same trace `packages replay` runs before it
//!    writes to a Shard — so a Package Delta a Datascript just emitted is validated by the code that
//!    also decides whether it may apply, not by a second, looser implementation of the same rules.
//! 7. A Build Identity sidecar (`packages::identity`) is written next to each artifact that just
//!    validated: every input the artifact was built from, so `packages check`, `preflight` and CI
//!    can tell later whether it is still current. Writing it only after step 6 succeeds means a
//!    sidecar never describes an artifact this build itself would have refused.
//!
//! Steps 1-3, 5 and 6 STREAM to the terminal rather than being captured. `tsc` writes its
//! diagnostics to stdout, so a captured run surfaced an empty error and lost the file and line this
//! command promises; a Datascript's own thrown error and the validator's own conflict report are the
//! same kind of thing. Step 0 captures instead: a `--version` banner is one line, and the gate needs
//! to read it, not relay it.
//!
//! Generating FIRST among the streamed steps is what gives the gate teeth. The typings are derived
//! from the Module every run, so a Datascript naming a column the Module renamed fails here, at
//! author time, rather than surviving into a Package Delta.
//!
//! TYPECHECKING BEFORE EMISSION is the same reasoning one step further: a Datascript that does not
//! typecheck should not run at all, so step 3 gates step 5 the same way step 1 gates step 3.
//! Validation runs LAST, after every Package has emitted, because a Claim Conflict can only be seen
//! once every artifact exists.
//!
//! WHY THE TYPINGS ARE NOT COMMITTED: they are a 400-file, 2 MB projection of the Module wasm. A
//! committed copy would put a large mechanical diff in every schema change and, worse, would be a
//! second source of truth that can disagree with the Module. `generated/` is git-ignored and this
//! command reproduces it. The Base Snapshot rides the same git-ignore rule for a different reason:
//! it is the OPERATOR's own client-derived data, not something to commit — see step 4.
//!
//! Bun is needed HERE and nowhere else. An Operator applying a prebuilt Package Delta runs no part
//! of this, which is why `doctor`'s Bun check is a warning rather than a launch blocker.

use std::path::{Path, PathBuf};

use crate::cmd::doctor::{self, BunVersionCheck};
use crate::cmd::packages::{artifact, identity};
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

// ---- Datascript emission and validation ----

/// Enabled Packages carrying a Datascript, in the order step 5 runs them: the folder-name sort,
/// same rule `packages/`'s own directory listing already uses to mean "enabled".
///
/// A Package's Datascripts live under `datascripts/src/<package>/`, named after the Package folder
/// under `packages/` — not the other way around, so a Datascript for a disabled or removed Package
/// simply has no Package to match and never runs.
fn packages_with_datascripts(project: &ProjectLayout) -> Result<Vec<String>> {
    let packages_dir = project.packages_dir();
    if !packages_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(&packages_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| project.datascripts_src_dir().join(name).is_dir())
        .collect();
    names.sort();
    Ok(names)
}

/// One Package's Datascripts, in the deterministic order step 5 runs them: file-name sort.
fn datascripts_of(project: &ProjectLayout, package: &str) -> Result<Vec<PathBuf>> {
    let dir = project.datascripts_src_dir().join(package);
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "ts"))
        .collect();
    scripts.sort();
    Ok(scripts)
}

/// The Base Snapshot must exist before any Datascript runs — `lib/index.ts` throws a "cannot read"
/// error naming the same command if it does not, one script at a time. Failing here, once, before
/// the first `bun run`, gives the author the fix instead of a wall of identical errors.
fn verify_base_snapshot(project: &ProjectLayout, packages: &[String]) -> Result<()> {
    let snapshot = project.base_snapshot_file();
    if snapshot.is_file() {
        return Ok(());
    }
    Err(Error::PrerequisiteMissing(format!(
        "no Base Snapshot at {snapshot}. {n} enabled Package(s) carry a Datascript ({names}), and \
         a Datascript reads its base spell data only from it. Build one first, from a 1.12.1 \
         client's Data/ directory (the one containing dbc.MPQ and terrain.MPQ):\n  \
         ./{importer_bin} --dbc <client Data/ dir> --spell-snapshot {snapshot}\nNothing was run.",
        snapshot = snapshot.display(),
        n = packages.len(),
        names = packages.join(", "),
        importer_bin = ProjectLayout::IMPORTER_BIN,
    )))
}

/// One Datascript entry file, run as a SUBPROCESS. `LYRACORE_BASE_SNAPSHOT` and
/// `LYRACORE_PACKAGES_ROOT` are the two overrides `datascripts/lib/index.ts` reads; this checkout's
/// own layout is not their default by accident — `lyracore` may run from a checkout whose
/// `datascripts/` a Datascript's own path math would not otherwise find.
fn datascript_command(project: &ProjectLayout, script: &Path) -> CommandSpec {
    CommandSpec::new("bun")
        .arg("run")
        .arg(script.to_string_lossy().to_string())
        .env(
            "LYRACORE_BASE_SNAPSHOT",
            project.base_snapshot_file().to_string_lossy().to_string(),
        )
        .env(
            "LYRACORE_PACKAGES_ROOT",
            project.packages_dir().to_string_lossy().to_string(),
        )
        .cwd(project.root.clone())
}

/// Run every enabled Package's Datascripts, fail-fast: the first script to throw stops the build
/// before any later script — in that Package or the next one — ever renders.
fn run_datascripts(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    packages: &[String],
) -> Result<()> {
    for package in packages {
        for script in datascripts_of(project, package)? {
            println!("running Datascript {} ({package})", script.display());
            runner
                .run_streaming(&datascript_command(project, &script))
                .map_err(|e| {
                    Error::Process(format!(
                        "Datascript {} ({package}) did not emit a Package Delta. Its own \
                         diagnostic is above — a script that throws writes nothing, so the \
                         Package's artifact is exactly what it was before this run.\n  ({e})",
                        script.display()
                    ))
                })?;
        }
    }
    Ok(())
}

/// The Rust-side authoritative check: every enabled Package's generated artifacts, traced together
/// in one `lyracore-delta-check` invocation. Discovered the same way `packages replay` discovers
/// them — a folder listing, not a re-parse by this command — so build-time validation and
/// replay-time preflight can never disagree about which files are in play.
///
/// Returns what it discovered and validated, so step 7 can write each artifact's Build Identity
/// without re-reading a tree this step just finished checking.
fn validate_generated_deltas(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
) -> Result<Vec<artifact::Artifact>> {
    let artifacts = artifact::read_enabled(&project.packages_dir())?;
    if artifacts.is_empty() {
        return Ok(artifacts);
    }

    println!();
    println!(
        "checking {} generated Package Delta artifact(s)",
        artifacts.len()
    );
    let mut command = CommandSpec::new("cargo")
        .arg("run")
        .arg("-p")
        .arg("lyracore-package-delta")
        .arg("--bin")
        .arg("lyracore-delta-check")
        .cwd(project.root.clone())
        .arg("--");
    for artifact in &artifacts {
        command = command.arg(artifact.path.to_string_lossy().to_string());
    }

    runner.run_streaming(&command).map_err(|e| {
        Error::Process(format!(
            "the generated Package Delta artifacts do not check out. The validator's own report is \
             above, naming the file and the exact claim to fix.\n  ({e})"
        ))
    })?;
    Ok(artifacts)
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

    let datascript_packages = packages_with_datascripts(project)?;
    if datascript_packages.is_empty() {
        return Ok(());
    }

    verify_base_snapshot(project, &datascript_packages)?;

    println!();
    println!(
        "running Datascripts for {} enabled Package(s): {}",
        datascript_packages.len(),
        datascript_packages.join(", ")
    );
    run_datascripts(project, runner, &datascript_packages)?;

    let artifacts = validate_generated_deltas(project, runner)?;
    identity::write_all(project, &artifacts)?;

    println!();
    println!("Package Deltas emitted and validated.");
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

    /// Any enabled Package that carries no Datascript — the ordinary shape most Packages have.
    fn with_package(project: &ProjectLayout, name: &str) {
        std::fs::create_dir_all(project.packages_dir().join(name)).unwrap();
    }

    /// An enabled Package carrying one Datascript, at `datascripts/src/<package>/<script>.ts`.
    fn with_datascript(project: &ProjectLayout, package: &str, script: &str) {
        with_package(project, package);
        let src_dir = project.datascripts_src_dir().join(package);
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join(format!("{script}.ts")), "// a Datascript\n").unwrap();
    }

    fn with_base_snapshot(project: &ProjectLayout) {
        std::fs::create_dir_all(project.datascript_types_dir()).unwrap();
        std::fs::write(project.base_snapshot_file(), "{}\n").unwrap();
    }

    const ARTIFACT_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The artifact a Datascript's own `bun run` would have written. `FakeStack` records a `bun
    /// run` call but does not execute Bun, so a test that needs one on disk (to exercise step 6,
    /// the validator) writes it directly — the same way `packages/replay.rs`'s tests do.
    fn with_generated_artifact(project: &ProjectLayout, package: &str) {
        let dir = project.packages_dir().join(package).join("data/.generated");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("spell.json"),
            format!(
                r#"{{"version":1,"package":"{package}","source_hash":"{ARTIFACT_HASH}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#
            ),
        )
        .unwrap();
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
        // `packages build` is author-time only: it produces typings and a verdict, nothing else —
        // true with a plain build and true once Datascripts are running and being validated too.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        assert!(
            stack.rendered().iter().any(|c| c.contains("bun run")),
            "the Datascript stages must actually have run: {:?}",
            stack.rendered()
        );
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("spacetime call"), "{call}");
            assert!(!call.contains("spacetime sql"), "{call}");
        }
    }

    // ---- Datascript emission and validation ----

    #[test]
    fn a_build_typegens_installs_typechecks_emits_then_validates_in_that_order() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let calls = stack.rendered();
        assert_eq!(calls.len(), 6, "{calls:?}");
        assert!(calls[0].starts_with("bun --version"), "{calls:?}");
        assert!(calls[1].contains("spacetime generate"), "{calls:?}");
        assert!(
            calls[2].contains("bun install --frozen-lockfile"),
            "{calls:?}"
        );
        assert!(
            calls[3].starts_with("bun ./node_modules/typescript/bin/tsc"),
            "{calls:?}"
        );
        assert!(calls[4].starts_with("bun run"), "{calls:?}");
        assert!(calls[4].contains("fire_nova/spells.ts"), "{calls:?}");
        assert!(calls[5].contains("cargo run"), "{calls:?}");
        assert!(calls[5].contains("lyracore-package-delta"), "{calls:?}");
        assert!(calls[5].contains("lyracore-delta-check"), "{calls:?}");
        assert!(
            calls[5].contains("fire_nova/data/.generated/spell.json"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_successful_build_writes_a_build_identity_sidecar_next_to_each_validated_artifact() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let sidecar = project
            .packages_dir()
            .join("fire_nova/data/.generated")
            .join(crate::cmd::packages::identity::IDENTITY_FILE);
        assert!(sidecar.is_file(), "no sidecar written next to the artifact");
        let artifacts = artifact::read_enabled(&project.packages_dir()).unwrap();
        let identity = crate::cmd::packages::identity::Identity::parse(
            &std::fs::read_to_string(&sidecar).unwrap(),
            &sidecar,
        )
        .unwrap();
        assert_eq!(identity.artifact_hash, artifacts[0].artifact_hash);
    }

    #[test]
    fn a_validator_failure_writes_no_identity_sidecar() {
        // The invariant the ordering exists for: a sidecar must never describe an artifact this
        // build itself would have refused.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new().fail_on(
            "lyracore-delta-check",
            "1 claim conflict(s) between the named Packages",
        );

        assert!(run(&project, &stack.runner()).is_err());

        let sidecar = project
            .packages_dir()
            .join("fire_nova/data/.generated")
            .join(crate::cmd::packages::identity::IDENTITY_FILE);
        assert!(
            !sidecar.exists(),
            "a failed validation must write no sidecar"
        );
    }

    #[test]
    fn the_datascript_subprocess_carries_the_base_snapshot_and_packages_root_overrides() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let run_call = stack
            .calls()
            .into_iter()
            .find_map(|call| match call {
                crate::proc::fake::Call::Stream(spec) if spec.render().starts_with("bun run") => {
                    Some(spec)
                }
                _ => None,
            })
            .expect("the Datascript ran");
        assert_eq!(
            run_call.env_value("LYRACORE_BASE_SNAPSHOT"),
            Some(project.base_snapshot_file().to_string_lossy().as_ref())
        );
        assert_eq!(
            run_call.env_value("LYRACORE_PACKAGES_ROOT"),
            Some(project.packages_dir().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_build_with_no_datascript_anywhere_stays_exactly_as_it_was() {
        // Packages without Datascripts must keep building — the emission and validation stages do
        // not exist for a checkout that never uses them.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_package(&project, "example"); // a Rust-only Package, no Datascript
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let calls = stack.rendered();
        assert_eq!(calls.len(), 4, "{calls:?}");
        for call in &calls {
            assert!(!call.contains("bun run"), "{calls:?}");
            assert!(!call.contains("lyracore-delta-check"), "{calls:?}");
        }
    }

    #[test]
    fn a_missing_base_snapshot_fails_with_the_remediation_command_before_any_datascript_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        // No with_base_snapshot(): the snapshot is missing.
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE, "{error}");
        assert!(error.to_string().contains("no Base Snapshot"), "{error}");
        assert!(error.to_string().contains("fire_nova"), "{error}");
        assert!(error.to_string().contains("--spell-snapshot"), "{error}");
        assert!(
            error
                .to_string()
                .contains("datascripts/generated/base-snapshot.json"),
            "{error}"
        );
        for call in stack.rendered() {
            assert!(!call.contains("bun run"), "{call}");
        }
    }

    #[test]
    fn a_mid_loop_script_failure_stops_the_build_and_a_later_package_never_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        // Folder-name order runs "alpha" before "zeta".
        with_datascript(&project, "alpha", "spells");
        with_datascript(&project, "zeta", "spells");
        with_base_snapshot(&project);
        let stack = FakeStack::new().fail_on("alpha/spells.ts", "the script threw");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("alpha"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("zeta"), "{call}");
        }
    }

    #[test]
    fn a_validator_failure_fails_the_build() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_datascript(&project, "fire_nova", "spells");
        with_base_snapshot(&project);
        with_generated_artifact(&project, "fire_nova");
        let stack = FakeStack::new().fail_on(
            "lyracore-delta-check",
            "1 claim conflict(s) between the named Packages",
        );

        let error = run(&project, &stack.runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE, "{error}");
        assert!(error.to_string().contains("do not check out"), "{error}");
    }

    #[test]
    fn a_disabled_packages_datascript_never_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        // `ghost` sits in `datascripts/src/` (the checkout-wide toolchain) but is NOT enabled: its
        // folder lives under `.lyracore/packages-disabled/`, not `packages/`.
        let ghost_src = project.datascripts_src_dir().join("ghost");
        std::fs::create_dir_all(&ghost_src).unwrap();
        std::fs::write(ghost_src.join("spells.ts"), "// a Datascript\n").unwrap();
        std::fs::create_dir_all(project.packages_disabled_dir().join("ghost")).unwrap();
        // An unrelated enabled Package with no Datascript, so the build still has work to skip
        // cleanly through rather than trivially having nothing enabled at all.
        with_package(&project, "example");
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        for call in stack.rendered() {
            assert!(!call.contains("ghost"), "{call}");
        }
    }
}
