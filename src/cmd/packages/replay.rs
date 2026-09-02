//! `lyracore packages replay [DATABASE …] [--check] [--yes] [--force-all]` — reapply every enabled
//! Package's Delta to every Shard that holds a copy of the spell catalogue.
//!
//! A base import replaces a whole Import Family, so a Package's claims are not a one-shot edit: they
//! replay as the last stage of that family's import. The importer already does this for ONE Shard,
//! idempotently and in one transaction. What was missing is the Realm: a catalogue lives on every
//! World Shard and Instance Pool that owns a copy, and applying it Shard by Shard from memory is how
//! a Realm ends up running two different Package sets without anything saying so.
//!
//! # What this verb adds over running the importer per Shard
//!
//! * **Preflight.** Every artifact is read, digested and traced ONCE, and every target is read from,
//!   before the first write. A Claim Conflict or an unreachable Shard fails the run at Shard 0.
//! * **Resume.** Each Shard records what it applied in `game_package_import`. A Shard whose
//!   provenance already matches this checkout's artifacts AND its current base import is reported
//!   complete and skipped, so re-running after a failure costs nothing on the Shards that finished.
//! * **An honest report.** A failure names the Shards that completed, the one that failed, and the
//!   ones never touched — then prints the command to resume, which is the same command.
//!
//! Fail-fast, never rollback: #310 puts distributed atomic transactions and automatic rollback of
//! completed Shards out of scope. A half-applied Realm is recoverable by re-running; a Realm that
//! reported success while half-applied is not.

use std::path::Path;

use crate::cmd::import::{self, Prompt};
use crate::cmd::packages::artifact::{self, Artifact, SPELL_FAMILY};
use crate::cmd::packages::{recorded_databases, shard_list};
use crate::cmd::publish::validate_database;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// What `packages replay` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplayOptions {
    /// The Shards to replay onto. Empty means the recorded development topology — never a guessed
    /// production list.
    pub databases: Vec<String>,
    pub client_data: Option<String>,
    /// Run the whole plan and write nothing: preflight, then the importer's own check mode per
    /// Shard. No confirmation, because nothing changes.
    pub check: bool,
    /// Answer the confirmation in advance.
    pub yes: bool,
    /// Replay every named Shard, including ones whose provenance already matches.
    pub force_all: bool,
}

/// One Shard's Package Delta provenance, as `spacetime sql` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Provenance {
    /// `(package, artifact_hash, base_source_sha)` for every Package this Shard has applied.
    applied: Vec<(String, String, String)>,
    /// This Shard's `game_import_meta.source_sha` for the spell family. Empty when the family has
    /// never been stamped here, which is a fact rather than a reason to refuse.
    base_source_sha: String,
}

/// What preflight decided about one target, before anything was written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    database: String,
    /// Why this Shard does not need the run, when it does not.
    complete: Option<String>,
}

/// The verb name this module's refusals quote back at the operator.
pub const VERB: &str = "packages replay";

/// Validate the Shard names given on the command line.
///
/// The same guard `publish` uses, for the same reason: these names reach a rendered subprocess, and
/// a flag smuggled in among them must be refused before any process starts. There is deliberately no
/// path here that infers a production Realm's Shard list.
pub fn databases(args: &[String]) -> Result<Vec<String>> {
    for name in args {
        validate_database(VERB, name)?;
    }
    Ok(args.to_vec())
}

/// The single-Shard contract this verb orchestrates: reimport the spell family from the client's
/// `Spell.dbc`, then reapply the enabled Packages' Deltas over it. Without `--apply` the importer
/// prints the plan and writes nothing.
fn replay_command(
    project: &ProjectLayout,
    database: &str,
    client_data: &Path,
    apply: bool,
) -> Result<CommandSpec> {
    validate_database(VERB, database)?;
    let command = import::importer_command(project, database)
        .arg("--dbc")
        .arg(client_data.to_string_lossy().to_string())
        .arg("--spells")
        .arg("--packages")
        .arg(project.packages_dir().to_string_lossy().to_string());
    Ok(if apply {
        command.arg("--apply")
    } else {
        command
    })
}

/// The answer's data rows, without the column header `spacetime sql` prints above them.
///
/// The header is dropped by MATCHING the columns that were asked for rather than by counting lines:
/// this reads digests straight out of the first row, and a header cell silently read as a digest
/// would make every Shard look like it was applied by a Package called `package`.
fn rows(output: &str, columns: &[&str]) -> Vec<Vec<String>> {
    let mut rows = import::table_cells(output);
    let header_first = rows.first().is_some_and(|row| {
        row.len() == columns.len() && row.iter().zip(columns).all(|(cell, name)| cell == name)
    });
    if header_first {
        rows.remove(0);
    }
    rows
}

/// Read one Shard's Package Delta provenance.
///
/// Two exact-match queries, no `ORDER BY`, no `IN` and no subquery: `spacetime sql` supports none of
/// them (docs/danger-zones.md §2). One query per FAMILY rather than one per Package, because the
/// extra rows are the point — a Package that left the enabled set is visible only as a row this
/// checkout no longer accounts for.
///
/// A failed query is NOT an empty result. Conflating them would turn one dead node into "this Shard
/// has never applied anything", and this verb would then happily replay a Realm it cannot see.
fn read_provenance(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
) -> Result<Provenance> {
    let ask = |query: &str| {
        runner
            .run_and_wait(&import::sql_command(project, database, query))
            .map_err(|e| {
                Error::Process(format!(
                    "could not read Package Delta provenance from '{database}': {e}\n  A failed \
                     query is not an empty result, so nothing was replayed. Check that the node is \
                     up and '{database}' is published (`lyracore dev status`), and that its module \
                     is current — `game_package_import` only exists once the Package Delta apply \
                     stage has been published to this Shard."
                ))
            })
    };

    let stamped = ask(&format!(
        "SELECT source_sha FROM game_import_meta WHERE family = '{SPELL_FAMILY}'"
    ))?;
    let base_source_sha = rows(&stamped, &["source_sha"])
        .first()
        .and_then(|row| row.first().cloned())
        .unwrap_or_default();

    let recorded = ask(&format!(
        "SELECT package, artifact_hash, base_source_sha FROM game_package_import WHERE family = \
         '{SPELL_FAMILY}'"
    ))?;
    let applied = rows(&recorded, &["package", "artifact_hash", "base_source_sha"])
        .into_iter()
        .filter(|row| row.len() >= 3)
        .map(|row| (row[0].clone(), row[1].clone(), row[2].clone()))
        .collect();

    Ok(Provenance {
        applied,
        base_source_sha,
    })
}

/// Does this Shard already hold exactly these artifacts, on this base import?
///
/// Every enabled Package must be recorded with the digest this checkout produces, no Package may be
/// recorded that is no longer enabled, and every row must sit on the Shard's CURRENT base stamp. A
/// mismatch anywhere means replay; only a total match skips.
///
/// Returns the reason it is complete, or `None`.
fn already_complete(artifacts: &[Artifact], provenance: &Provenance) -> Option<String> {
    for artifact in artifacts {
        let recorded = provenance
            .applied
            .iter()
            .find(|(package, ..)| *package == artifact.package)?;
        if recorded.1 != artifact.artifact_hash || recorded.2 != provenance.base_source_sha {
            return None;
        }
    }
    if provenance.applied.len() != artifacts.len() {
        return None;
    }
    Some(match artifacts.len() {
        0 => "no Package claims the spell family, and none is recorded here".to_string(),
        1 => "1 Package, matching artifact digest and base import".to_string(),
        n => format!("{n} Packages, matching artifact digests and base import"),
    })
}

/// The command that resumes this run — the same one, because resume is what re-running does.
fn resume_command(shards: &[String], options: &ReplayOptions) -> String {
    let mut line = format!("lyracore packages replay {}", shards.join(" "));
    if let Some(path) = &options.client_data {
        line.push_str(&format!(" --client-data {path}"));
    }
    if options.force_all {
        line.push_str(" --force-all");
    }
    if options.yes {
        line.push_str(" --yes");
    }
    line
}

/// Replay every enabled Package's Delta across the named Shards.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    options: &ReplayOptions,
) -> Result<()> {
    let shards = if options.databases.is_empty() {
        recorded_databases(project)?
    } else {
        options.databases.clone()
    };
    if shards.is_empty() {
        return Err(Error::Usage(
            "no Shard to replay onto. Name the databases explicitly, e.g. `lyracore packages \
             replay lyracore lyracore-kalimdor`."
                .to_string(),
        ));
    }

    // ---- preflight: the artifacts, once, before any target is touched ----
    let root = project.packages_dir();
    let enabled = artifact::read_enabled(&root)?;
    let artifacts = &enabled.deltas;
    let conflicts = artifact::conflicts(artifacts);
    if !conflicts.is_empty() {
        return Err(Error::Usage(format!(
            "{} claim conflict(s) between enabled Packages — nothing was replayed:\n{}\nResolve \
             them by disabling a Package (`lyracore packages disable NAME`) or by changing what one \
             of them claims. The module refuses the same plan.",
            conflicts.len(),
            conflicts
                .iter()
                .map(|c| format!("  {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }

    println!();
    println!("=== enabled Package Deltas ({}) ===", root.display());
    if artifacts.is_empty() {
        println!("  no enabled Package claims the {SPELL_FAMILY} import family");
    }
    if let Some(note) = enabled.skipped_note() {
        println!("  {note}");
    }
    for artifact in artifacts {
        println!(
            "  {:<32} {:>3} updated  {:>3} invented   {}",
            artifact.package,
            artifact.updated_rows,
            artifact.inserted_rows,
            artifact.path.display()
        );
    }

    let client_data = import::resolve_client_data(project, prompt, options.client_data.as_deref())?;

    // ---- preflight: every target, before the first write ----
    println!();
    println!("=== targets ===");
    let mut targets = Vec::with_capacity(shards.len());
    for database in &shards {
        let provenance = read_provenance(project, runner, database)?;
        let complete = if options.force_all {
            None
        } else {
            already_complete(artifacts, &provenance)
        };
        match &complete {
            Some(reason) => println!("  {database:<24} already complete — {reason}"),
            None => println!("  {database:<24} replay"),
        }
        targets.push(Target {
            database: database.clone(),
            complete,
        });
    }

    let wanted: Vec<&Target> = targets.iter().filter(|t| t.complete.is_none()).collect();
    let skipped: Vec<String> = targets
        .iter()
        .filter(|t| t.complete.is_some())
        .map(|t| t.database.clone())
        .collect();

    if options.check {
        println!();
        println!("=== check: the plan, per Shard, writing nothing ===");
        runner.run_streaming(&import::build_importer_command(project))?;
        for target in &targets {
            println!();
            println!("==> checking {}", target.database);
            runner.run_streaming(&replay_command(
                project,
                &target.database,
                &client_data,
                false,
            )?)?;
        }
        println!();
        println!("check only — nothing was written. Re-run without --check to apply.");
        return Ok(());
    }

    if wanted.is_empty() {
        println!();
        println!(
            "every named Shard already holds these {} Package Delta(s) on its current base import: \
             {}",
            artifacts.len(),
            shard_list(&skipped)
        );
        println!("nothing to replay. Use --force-all to reapply anyway.");
        return Ok(());
    }

    let pending: Vec<String> = wanted.iter().map(|t| t.database.clone()).collect();
    println!();
    // An empty enabled inventory is a legitimate plan and a destructive one: it clears the Package
    // spell range. The operator says it deliberately or not at all.
    let question = if artifacts.is_empty() {
        format!(
            "No enabled Package claims the {SPELL_FAMILY} family. Replaying will CLEAR the Package \
             spell range on {} — every row an unenabled Package invented is deleted. Proceed?",
            shard_list(&pending)
        )
    } else {
        format!(
            "Reapply {} enabled Package Delta(s) to {}? Each Shard reimports Spell.dbc and then \
             replays these claims over it.",
            artifacts.len(),
            shard_list(&pending)
        )
    };
    crate::cmd::packages::confirm(prompt, &question, "Nothing was replayed.", options.yes)?;

    runner.run_streaming(&import::build_importer_command(project))?;

    for (index, target) in wanted.iter().enumerate() {
        println!();
        println!("==> replaying {}", target.database);
        let command = replay_command(project, &target.database, &client_data, true)?;
        if let Err(error) = runner.run_streaming(&command) {
            let untouched: Vec<String> = wanted[index + 1..]
                .iter()
                .map(|t| t.database.clone())
                .collect();
            return Err(Error::Process(format!(
                "{error}\n  replay stopped at: {failed}\n  completed: {completed}\n  already \
                 complete (skipped): {skipped}\n  untouched: {untouched}\n\n  The failed Shard \
                 wrote nothing: the apply is one transaction, so it lands whole or not at all. \
                 Fix the cause and re-run the SAME command — the Shards above verify their \
                 provenance and skip:\n    {resume}",
                failed = target.database,
                completed = shard_list(
                    &wanted[..index]
                        .iter()
                        .map(|t| t.database.clone())
                        .collect::<Vec<_>>()
                ),
                skipped = shard_list(&skipped),
                untouched = shard_list(&untouched),
                resume = resume_command(&shards, options),
            )));
        }
    }

    println!();
    println!(
        "replayed {} enabled Package Delta(s) to: {}",
        artifacts.len(),
        shard_list(&pending)
    );
    if !skipped.is_empty() {
        println!("already complete, untouched: {}", shard_list(&skipped));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::import::tests::ScriptedPrompt;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const BASE_SHA: &str = "b9d1e0aa11223344b9d1e0aa11223344b9d1e0aa11223344b9d1e0aa11223344";

    /// A checkout with a client-data directory, an enabled Package Inventory, and nothing else the
    /// verb needs.
    struct Checkout {
        tmp: TempDir,
        project: ProjectLayout,
    }

    impl Checkout {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
            std::fs::create_dir_all(root.join("packages")).unwrap();
            // A 1.12.1 client Data/ directory, as `import` validates it.
            let data = root.join("client-data");
            std::fs::create_dir_all(&data).unwrap();
            for archive in ["dbc.MPQ", "terrain.MPQ", "model.MPQ", "wmo.MPQ"] {
                std::fs::write(data.join(archive), "").unwrap();
            }
            let project = ProjectLayout::from_root(root).unwrap();
            Self { tmp, project }
        }

        fn client_data(&self) -> String {
            self.tmp.path().join("client-data").display().to_string()
        }

        /// An enabled Package whose artifact tunes one column of one real spell.
        fn with_package(&self, package: &str, spell_id: u32, value: u32) -> &Self {
            let dir = self
                .tmp
                .path()
                .join("packages")
                .join(package)
                .join("data/.generated");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("spell.json"),
                format!(
                    r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":{spell_id}}},"operation":"update","fields":{{"cooldown_ms":{{"type":"u32","value":{value}}}}}}}]}}"#
                ),
            )
            .unwrap();
            self
        }

        fn artifact_hash(&self, package: &str) -> String {
            artifact::read_enabled(&self.project.packages_dir())
                .unwrap()
                .deltas
                .into_iter()
                .find(|a| a.package == package)
                .expect("the package is enabled")
                .artifact_hash
        }

        fn options(&self, shards: &[&str]) -> ReplayOptions {
            ReplayOptions {
                databases: shards.iter().map(|s| (*s).to_string()).collect(),
                client_data: Some(self.client_data()),
                yes: true,
                ..Default::default()
            }
        }
    }

    /// A `spacetime sql` answer in the tabular shape the real CLI prints.
    fn sql_rows(header: &[&str], rows: &[Vec<String>]) -> String {
        let mut out = format!(" {} \n", header.join(" | "));
        for row in rows {
            out.push_str(&format!(" {} \n", row.join(" | ")));
        }
        out
    }

    /// A Shard that has never applied a Package Delta but has a stamped spell import.
    fn fresh_shard(stack: FakeStack, database: &str) -> FakeStack {
        stack
            .with_stdout(
                &format!("{database} SELECT source_sha"),
                &sql_rows(&["source_sha"], &[vec![BASE_SHA.to_string()]]),
            )
            .with_stdout(
                &format!("{database} SELECT package"),
                &sql_rows(&["package", "artifact_hash", "base_source_sha"], &[]),
            )
    }

    /// A Shard already running exactly `package` at `hash`, on the current base import.
    fn applied_shard(stack: FakeStack, database: &str, package: &str, hash: &str) -> FakeStack {
        stack
            .with_stdout(
                &format!("{database} SELECT source_sha"),
                &sql_rows(&["source_sha"], &[vec![BASE_SHA.to_string()]]),
            )
            .with_stdout(
                &format!("{database} SELECT package"),
                &sql_rows(
                    &["package", "artifact_hash", "base_source_sha"],
                    &[vec![
                        package.to_string(),
                        hash.to_string(),
                        BASE_SHA.to_string(),
                    ]],
                ),
            )
    }

    fn applies(stack: &FakeStack) -> Vec<String> {
        stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("lyracore-importer") && r.contains("--apply"))
            .collect()
    }

    // ---- the guard, inherited from `publish` ----

    #[test]
    fn flag_shaped_shard_names_are_refused_rather_than_forwarded() {
        for flag in ["-c", "--delete-data", "--yes", "--anything", "-"] {
            let error = databases(&[flag.to_string()]).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{flag}");
            assert!(error.to_string().contains("Refusing"), "{flag}: {error}");
        }
    }

    #[test]
    fn a_flag_hidden_after_a_valid_shard_name_is_still_refused() {
        let error = databases(&["lyracore".to_string(), "-c".to_string()]).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
    }

    // ---- the run ----

    #[test]
    fn every_named_shard_is_replayed_in_order() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let mut stack = FakeStack::new();
        for shard in ["lyracore", "lyracore-kalimdor", "lyracore-instances"] {
            stack = fresh_shard(stack, shard);
        }

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore", "lyracore-kalimdor", "lyracore-instances"]),
        )
        .unwrap();

        let applied = applies(&stack);
        assert_eq!(applied.len(), 3, "{applied:?}");
        for (rendered, shard) in
            applied
                .iter()
                .zip(["lyracore", "lyracore-kalimdor", "lyracore-instances"])
        {
            assert!(rendered.contains(&format!("--db {shard}")), "{rendered}");
            assert!(rendered.contains("--spells"), "{rendered}");
            assert!(rendered.contains("--packages"), "{rendered}");
        }
    }

    /// The acceptance criterion the whole verb exists for: a mid-run failure must never read as a
    /// Realm-wide success, and the report must place every Shard.
    #[test]
    fn a_failure_between_two_shards_stops_the_run_and_places_every_shard() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let mut stack = FakeStack::new();
        for shard in ["lyracore", "lyracore-kalimdor", "lyracore-instances"] {
            stack = fresh_shard(stack, shard);
        }
        let stack = stack.fail_on(
            "--db lyracore-kalimdor --server",
            "claim conflict, nothing applied",
        );

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore", "lyracore-kalimdor", "lyracore-instances"]),
        )
        .unwrap_err();

        let message = error.to_string();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(
            message.contains("replay stopped at: lyracore-kalimdor"),
            "{message}"
        );
        assert!(message.contains("completed: lyracore\n"), "{message}");
        assert!(
            message.contains("untouched: lyracore-instances"),
            "{message}"
        );
        assert!(
            message
                .contains("lyracore packages replay lyracore lyracore-kalimdor lyracore-instances"),
            "the resume command is the same command: {message}"
        );
        assert!(
            !applies(&stack)
                .iter()
                .any(|r| r.contains("--db lyracore-instances")),
            "the run must stop at the failure: {:?}",
            applies(&stack)
        );
    }

    #[test]
    fn a_shard_whose_provenance_already_matches_is_reported_complete_and_never_reapplied() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = applied_shard(
            fresh_shard(FakeStack::new(), "lyracore"),
            "lyracore-kalimdor",
            "bolt",
            &hash,
        );

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore", "lyracore-kalimdor"]),
        )
        .unwrap();

        let applied = applies(&stack);
        assert_eq!(applied.len(), 1, "{applied:?}");
        assert!(applied[0].contains("--db lyracore"), "{applied:?}");
        assert!(
            !applied[0].contains("--db lyracore-kalimdor"),
            "a verified-complete Shard is not reapplied: {applied:?}"
        );
    }

    #[test]
    fn force_all_reapplies_a_shard_whose_provenance_already_matches() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = applied_shard(FakeStack::new(), "lyracore-kalimdor", "bolt", &hash);
        let options = ReplayOptions {
            force_all: true,
            ..checkout.options(&["lyracore-kalimdor"])
        };

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &options,
        )
        .unwrap();

        assert_eq!(applies(&stack).len(), 1, "{:?}", applies(&stack));
    }

    /// A Shard stamped from a DIFFERENT base import is not complete, whatever its artifact digests
    /// say: the claims sit on rows the base import has since replaced.
    #[test]
    fn a_shard_on_a_different_base_import_is_replayed_even_though_its_digests_match() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = FakeStack::new()
            .with_stdout(
                "lyracore-kalimdor SELECT source_sha",
                &sql_rows(&["source_sha"], &[vec![BASE_SHA.to_string()]]),
            )
            .with_stdout(
                "lyracore-kalimdor SELECT package",
                &sql_rows(
                    &["package", "artifact_hash", "base_source_sha"],
                    &[vec![
                        "bolt".to_string(),
                        hash,
                        "an-older-spell-dbc".to_string(),
                    ]],
                ),
            );

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore-kalimdor"]),
        )
        .unwrap();

        assert_eq!(applies(&stack).len(), 1, "{:?}", applies(&stack));
    }

    /// Removing a Package must replay every affected target with the REMAINING set. The payload is
    /// built by the importer from the enabled inventory, so the proof is that the disabled
    /// Package's artifact is no longer in the tree the importer is pointed at, and that the Shard
    /// still holding its provenance row is not treated as complete.
    #[test]
    fn a_shard_still_recording_a_disabled_package_is_replayed_with_the_remaining_set() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let bolt = checkout.artifact_hash("bolt");
        // `lyracore-kalimdor` also recorded `ember`, which is no longer enabled in this checkout.
        let stack = FakeStack::new()
            .with_stdout(
                "lyracore-kalimdor SELECT source_sha",
                &sql_rows(&["source_sha"], &[vec![BASE_SHA.to_string()]]),
            )
            .with_stdout(
                "lyracore-kalimdor SELECT package",
                &sql_rows(
                    &["package", "artifact_hash", "base_source_sha"],
                    &[
                        vec!["bolt".to_string(), bolt, BASE_SHA.to_string()],
                        vec![
                            "ember".to_string(),
                            "whatever-ember-hashed-to".to_string(),
                            BASE_SHA.to_string(),
                        ],
                    ],
                ),
            );

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore-kalimdor"]),
        )
        .unwrap();

        let applied = applies(&stack);
        assert_eq!(
            applied.len(),
            1,
            "an extra provenance row forces a replay: {applied:?}"
        );
        assert!(
            !std::fs::read_dir(checkout.project.packages_dir())
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "ember"),
            "the disabled Package is not in the tree the importer reads its payload from"
        );
    }

    #[test]
    fn check_runs_the_whole_plan_and_renders_no_apply() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let mut stack = FakeStack::new();
        for shard in ["lyracore", "lyracore-kalimdor"] {
            stack = fresh_shard(stack, shard);
        }
        let options = ReplayOptions {
            check: true,
            yes: false,
            ..checkout.options(&["lyracore", "lyracore-kalimdor"])
        };

        // No terminal, and no prompt is reached: a check writes nothing, so it asks nothing.
        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &options,
        )
        .unwrap();

        assert!(applies(&stack).is_empty(), "{:?}", stack.rendered());
        let checks: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("lyracore-importer") && r.contains("--packages"))
            .collect();
        assert_eq!(checks.len(), 2, "{checks:?}");
    }

    /// A check still visits a Shard the run would have skipped: the operator asked what the plan is,
    /// not what the shortest path to it would be.
    #[test]
    fn check_reports_every_named_shard_including_the_complete_ones() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = applied_shard(FakeStack::new(), "lyracore-kalimdor", "bolt", &hash);
        let options = ReplayOptions {
            check: true,
            ..checkout.options(&["lyracore-kalimdor"])
        };

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &options,
        )
        .unwrap();

        let checks: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("lyracore-importer") && r.contains("--packages"))
            .collect();
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert!(applies(&stack).is_empty(), "{:?}", stack.rendered());
    }

    /// An empty enabled inventory is a real statement — "no Package claims this family" — and the
    /// importer clears the Package spell range for it. It must never be an accident, so the
    /// confirmation names the consequence rather than counting Packages.
    #[test]
    fn an_empty_enabled_inventory_states_what_it_clears_before_it_replays() {
        let checkout = Checkout::new();
        // Nothing enabled, but the Shard still records a Package from an earlier run.
        let stack = applied_shard(FakeStack::new(), "lyracore", "bolt", "stale-digest");
        let prompt = ScriptedPrompt::new(&["yes"]);

        run(
            &checkout.project,
            &stack.runner(),
            &prompt,
            &ReplayOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap();

        let asked = prompt.asked();
        assert!(asked.iter().any(|q| q.contains("CLEAR")), "{asked:?}");
        assert!(
            asked.iter().any(|q| q.contains("Package spell range")),
            "{asked:?}"
        );
        let applied = applies(&stack);
        assert_eq!(
            applied.len(),
            1,
            "the plan is still sent explicitly: {applied:?}"
        );
        assert!(applied[0].contains("--packages"), "{applied:?}");
    }

    /// The other half of the same decision: with nothing enabled AND nothing recorded, the Package
    /// spell range is already clear. There is no work, so there is no question and no write.
    #[test]
    fn an_empty_inventory_over_a_shard_that_never_applied_one_is_already_complete() {
        let checkout = Checkout::new();
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &ReplayOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap();

        assert!(applies(&stack).is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn a_claim_conflict_fails_the_run_before_the_first_shard_is_read() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        checkout.with_package("ember", 133, 3000);
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("claim conflict"), "{error}");
        assert!(
            stack.rendered().is_empty(),
            "nothing may run before the artifacts are cleared: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn an_unreachable_shard_fails_the_run_before_any_shard_is_written() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let stack = fresh_shard(FakeStack::new(), "lyracore")
            .fail_on("lyracore-kalimdor SELECT", "connection refused");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore", "lyracore-kalimdor"]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("lyracore-kalimdor"), "{error}");
        assert!(
            applies(&stack).is_empty(),
            "preflight must precede the first write: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_refused_confirmation_replays_nothing() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["no"]),
            &ReplayOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("Nothing was replayed"),
            "{error}"
        );
        assert!(applies(&stack).is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn the_provenance_queries_carry_no_order_by_and_no_subquery() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap();

        let queries: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("spacetime sql"))
            .collect();
        assert_eq!(queries.len(), 2, "{queries:?}");
        for query in &queries {
            assert!(!query.contains("ORDER BY"), "{query}");
            assert!(!query.contains(" IN ("), "{query}");
            assert!(query.contains("WHERE family = 'spell'"), "{query}");
        }
    }
}
