//! `lyracore packages check` — is every enabled Package's generated Package Delta still current?
//!
//! `packages build` records a Build Identity next to each artifact it emits (see [`super::
//! identity`]). This verb recomputes every recorded input from the checkout ON DISK right now and
//! refuses, naming the specific input, the moment one no longer matches — the same report `preflight`
//! folds into its own gate on `publish`'s behalf, so a stale artifact never reaches a Shard.
//!
//! # What "current" means here
//!
//! `datascripts/generated/` is regenerated FRESH, every run, with the same `spacetime generate`
//! invocation `packages build`'s typegen step uses — so a Module schema change makes a committed
//! artifact stale even on a clean checkout that never ran `packages build` itself. Nothing else is
//! regenerated: this verb never runs Bun and never re-emits a Datascript, so it needs neither Bun nor
//! a Base Snapshot to do its job.
//!
//! A missing Base Snapshot is reported as UNVERIFIABLE, not stale: the snapshot is the Operator's own
//! client-derived data, and a CI machine holding none cannot regenerate one to compare against. A
//! Base Snapshot that IS present and no longer matches its recorded hash is a real mismatch and fails
//! like any other input.
//!
//! A missing sidecar is treated as stale — it predates identity tracking, so there is nothing to
//! compare against and no way to call the artifact current.

use crate::cmd::packages::artifact::{self, Artifact};
use crate::cmd::packages::build;
use crate::cmd::packages::identity;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// One artifact's problems, and whether its Base Snapshot comparison had to be skipped.
fn check_one(project: &ProjectLayout, artifact: &Artifact) -> Result<(Vec<String>, bool)> {
    let sidecar_path = artifact.path.with_file_name(identity::IDENTITY_FILE);
    let Ok(text) = std::fs::read_to_string(&sidecar_path) else {
        return Ok((
            vec![format!(
                "`{}`: no Build Identity sidecar next to {} (predates identity tracking, or was \
                 removed). Rebuild with `lyracore packages build`.",
                artifact.package,
                artifact.path.display()
            )],
            false,
        ));
    };
    let recorded = identity::Identity::parse(&text, &sidecar_path)?;
    let dir = identity::package_dir(project, &artifact.path)?;
    let (current, snapshot_available) = identity::compute(project, &dir, &artifact.artifact_hash)?;

    let problems = recorded
        .changed_against(&current, snapshot_available)
        .into_iter()
        .map(|input| {
            format!(
                "`{}`: {} changed. Rebuild with `lyracore packages build`.",
                artifact.package,
                input.description()
            )
        })
        .collect();
    Ok((problems, !snapshot_available))
}

/// Verify every enabled Package's generated artifact against its recorded Build Identity.
///
/// A checkout with no Packages at all, or none carrying a generated artifact, is a clean no-op —
/// nothing is regenerated and nothing is read beyond the enabled Package Inventory listing.
pub fn run(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    if !project.packages_dir().is_dir() {
        println!(
            "no {} directory; nothing to check.",
            ProjectLayout::PACKAGES_DIR
        );
        return Ok(());
    }
    let enabled = artifact::read_enabled(&project.packages_dir())?;
    // A Script Artifact has no Build Identity sidecar and no Base Snapshot to drift against, so
    // this verb has nothing to check about one. `packages replay` is what applies them.
    match enabled.scripts.len() {
        0 => {}
        1 => println!(
            "1 Script Artifact is not a Package Delta; `lyracore packages replay` applies it"
        ),
        n => println!(
            "{n} Script Artifacts are not Package Deltas; `lyracore packages replay` applies them"
        ),
    }
    let artifacts = enabled.deltas;
    if artifacts.is_empty() {
        println!("no Package claims a Datascript-generated artifact; nothing to check.");
        return Ok(());
    }

    println!(
        "regenerating Module schema typings -> {} (so a schema change is visible even on a clean \
         checkout)",
        project.datascript_types_dir().display()
    );
    runner
        .run_streaming(&build::typegen_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "could not extract the Module schema as TypeScript, so `packages check` cannot \
                 tell whether the generated typings are current. Nothing was checked.\n  ({e})"
            ))
        })?;

    let mut problems = Vec::new();
    let mut snapshot_unverifiable = false;
    for artifact in &artifacts {
        let (found, unverifiable) = check_one(project, artifact)?;
        problems.extend(found);
        snapshot_unverifiable |= unverifiable;
    }

    if snapshot_unverifiable {
        println!(
            "no Base Snapshot at {} — snapshot drift cannot be checked here (it is the Operator's \
             own client-derived data). This does not fail `packages check`.",
            project.base_snapshot_file().display()
        );
    }

    if problems.is_empty() {
        println!("{} Package Delta artifact(s) are current.", artifacts.len());
        return Ok(());
    }

    Err(Error::Process(format!(
        "{} of {} Package Delta artifact(s) are stale:\n{}",
        problems.len(),
        artifacts.len(),
        problems
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn artifact_json(package: &str) -> String {
        format!(
            r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#
        )
    }

    /// A checkout carrying one enabled Package with a committed, CURRENT artifact + sidecar — the
    /// state `packages build` leaves behind on success.
    fn checked_out(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();

        std::fs::create_dir_all(project.datascripts_src_dir().join("fire_nova")).unwrap();
        std::fs::write(
            project
                .datascripts_src_dir()
                .join("fire_nova")
                .join("spells.ts"),
            "// a Datascript\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.datascript_types_dir()).unwrap();
        std::fs::write(
            project.datascript_types_dir().join("types.ts"),
            "export type Spell = { spellId: number };\n",
        )
        .unwrap();
        std::fs::write(project.base_snapshot_file(), "{\"spells\":[]}\n").unwrap();
        std::fs::create_dir_all(project.datascripts_lib_dir()).unwrap();
        std::fs::write(
            project.datascripts_lib_dir().join("index.ts"),
            "// the authoring library\n",
        )
        .unwrap();
        std::fs::write(project.datascripts_dir().join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(project.datascripts_dir().join("package.json"), "{}\n").unwrap();
        std::fs::write(project.datascripts_dir().join("bun.lock"), "{}\n").unwrap();

        let generated = project
            .packages_dir()
            .join("fire_nova")
            .join("data/.generated");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("spell.json"), artifact_json("fire_nova")).unwrap();

        let artifacts = artifact::read_enabled(&project.packages_dir())
            .unwrap()
            .deltas;
        identity::write_all(&project, &artifacts).unwrap();
        project
    }

    // ---- the clean pass ----

    #[test]
    fn a_current_checkout_passes_and_regenerates_typings_first() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        let calls = stack.rendered();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("spacetime generate"), "{calls:?}");
        assert!(calls[0].contains("--lang typescript"), "{calls:?}");
    }

    /// The seam this verb sits on globs every `*.json` beside the Delta, so a Package that also
    /// ships Runtime Scripts must not make `packages check` refuse the Package Deltas it can check.
    #[test]
    fn a_script_artifact_beside_the_delta_is_skipped_rather_than_failing_the_check() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::write(
            project
                .packages_dir()
                .join("fire_nova/data/.generated/script.json"),
            concat!(
                r#"{"kind":"script","version":1,"package":"fire_nova","#,
                r#""source_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
                r#""scripts":[{"script_id":100001,"name":"fire_nova.greet","event":"on_login","#,
                r#""priority":0,"enabled":true,"source":"grant_xp(event.actor, 10)"}]}"#,
            ),
        )
        .unwrap();
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();
    }

    // ---- no-op checkouts ----

    #[test]
    fn no_packages_directory_is_a_clean_no_op() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn no_datascript_anywhere_is_a_clean_no_op() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        std::fs::create_dir_all(project.packages_dir().join("example")).unwrap();
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();

        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    // ---- staleness ----

    #[test]
    fn a_missing_sidecar_is_stale_and_names_the_rebuild_command() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::remove_file(
            project
                .packages_dir()
                .join("fire_nova/data/.generated")
                .join(identity::IDENTITY_FILE),
        )
        .unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("fire_nova"), "{error}");
        assert!(
            error.to_string().contains("no Build Identity sidecar"),
            "{error}"
        );
        assert!(
            error.to_string().contains("lyracore packages build"),
            "{error}"
        );
    }

    #[test]
    fn an_edited_datascript_source_is_stale_and_names_that_input() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::write(
            project
                .datascripts_src_dir()
                .join("fire_nova")
                .join("spells.ts"),
            "// changed after the build\n",
        )
        .unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("fire_nova"), "{error}");
        assert!(error.to_string().contains("Datascript source"), "{error}");
        assert!(
            error.to_string().contains("lyracore packages build"),
            "{error}"
        );
    }

    #[test]
    fn a_hand_edited_artifact_is_stale() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::write(
            project
                .packages_dir()
                .join("fire_nova/data/.generated/spell.json"),
            artifact_json("fire_nova").replace("1500", "9999"),
        )
        .unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("fire_nova"), "{error}");
        assert!(error.to_string().contains("hand-edited"), "{error}");
    }

    // ---- the missing-snapshot contract ----

    #[test]
    fn a_missing_base_snapshot_is_reported_unverifiable_not_stale() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::remove_file(project.base_snapshot_file()).unwrap();
        let stack = FakeStack::new();

        run(&project, &stack.runner()).unwrap();
    }

    #[test]
    fn a_mismatched_snapshot_that_is_present_locally_still_fails() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        std::fs::write(project.base_snapshot_file(), "{\"spells\":[{}]}\n").unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("Base Snapshot"), "{error}");
    }

    // ---- typegen failure ----

    #[test]
    fn a_failed_typegen_fails_the_check_before_any_artifact_is_read() {
        let tmp = TempDir::new().unwrap();
        let project = checked_out(&tmp);
        let stack = FakeStack::new().fail_on("spacetime generate", "the module does not compile");

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("Module schema"), "{error}");
    }
}
