//! Prepare and activate installed Packages on explicitly named Shards or the development topology.
//! Sources build when artifacts are missing or stale. Rust Packages and pending Package Teardown
//! require a Module publish and schedule repair. Artifact provenance skips completed families.
//! Confirmation precedes all Realm writes. Failures stop the run and identify the remaining work.

use std::path::Path;

use crate::cmd::import::{self, Prompt};
use crate::cmd::packages::artifact::{self, Artifact, SPELL_FAMILY};
use crate::cmd::packages::script::{self, ScriptArtifact, SCRIPT_FAMILY};
use crate::cmd::packages::{check, inventory};
use crate::cmd::packages::{recorded_databases, shard_list};
use crate::cmd::publish::validate_database;
use crate::cmd::{preflight, publish};
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// The reducer that applies one family's whole enabled plan in one transaction.
const APPLY_REDUCER: &str = "apply_package_deltas";

/// What `packages apply` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyOptions {
    /// The Shards to apply onto. Empty means the recorded development topology — never a guessed
    /// production list.
    pub databases: Vec<String>,
    pub client_data: Option<String>,
    /// Prepare locally and check the plan without Realm writes.
    pub check: bool,
    /// Answer the confirmation in advance.
    pub yes: bool,
    /// Apply every named Shard, including ones whose provenance already matches.
    pub force_all: bool,
}

/// One Package's provenance row for one family, as `spacetime sql` reports it:
/// `(package, artifact_hash, base_source_sha)`. The script family has no base import, so its rows
/// carry an empty stamp and nothing compares it.
type Applied = (String, String, String);

/// What preflight decided about one target, before anything was written.
///
/// One answer per Import Family: a Shard can hold this checkout's Package Deltas without its
/// Runtime Scripts, which is what a Realm looks like before its first Package script.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    database: String,
    /// Why the spell family does not need this Shard, when it does not.
    spell: Option<String>,
    /// Why the script family does not need this Shard, when it does not.
    script: Option<String>,
}

impl Target {
    /// Whether any family still has work here.
    const fn wanted(&self) -> bool {
        self.spell.is_none() || self.script.is_none()
    }
}

/// The verb name this module's refusals quote back at the operator.
pub const VERB: &str = "packages apply";

/// Validate the Shard names given on the command line.
///
/// Same guard as `publish`: these names reach a rendered subprocess, so a smuggled flag must be
/// refused before any process starts. Never infers a production Realm's Shard list.
pub fn databases(args: &[String]) -> Result<Vec<String>> {
    for name in args {
        validate_database(VERB, name)?;
    }
    Ok(args.to_vec())
}

/// The single-Shard contract this verb orchestrates: reimport the spell family from the client's
/// `Spell.dbc`, then reapply the enabled Packages' Deltas over it. Without `--apply` the importer
/// prints the plan and writes nothing.
fn apply_command(
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
/// Drops the header by matching the asked-for columns, not by counting lines — a header cell
/// silently read as a digest would make every Shard look applied by a Package called `package`.
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

/// One read of one Shard, with the diagnosis a failure needs.
///
/// A failed query is not an empty result — conflating them would read a dead node as "never
/// applied" and apply a Realm this verb cannot actually see.
fn ask(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
    query: &str,
) -> Result<String> {
    runner
        .run_and_wait(&import::sql_command(project, database, query))
        .map_err(|e| {
            Error::Process(format!(
                "could not read Package artifact provenance from '{database}': {e}\n  A failed \
                 query is not an empty result, so nothing was applied. Check that the node is up \
                 and '{database}' is published (`lyracore dev status`), and that its module is \
                 current — `game_package_import` only exists once the Package artifact apply stage \
                 has been published to this Shard."
            ))
        })
}

/// Read one Shard's spell-family provenance.
///
/// One exact-match query — `spacetime sql` supports no `ORDER BY`, `IN`, or subquery
/// (docs/danger-zones.md §2) — per family rather than per Package, because a Package that left the
/// enabled set is visible only as a row this checkout no longer accounts for.
fn read_spell_provenance(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
) -> Result<Vec<Applied>> {
    let recorded = ask(
        project,
        runner,
        database,
        &format!(
            "SELECT package, artifact_hash, base_source_sha FROM game_package_import WHERE family \
             = '{SPELL_FAMILY}'"
        ),
    )?;
    Ok(
        rows(&recorded, &["package", "artifact_hash", "base_source_sha"])
            .into_iter()
            .filter(|row| row.len() >= 3)
            .map(|row| (row[0].clone(), row[1].clone(), row[2].clone()))
            .collect(),
    )
}

/// Read one Shard's script-family provenance.
///
/// Two columns, not the spell family's three: the script family has no base import, so its rows
/// carry no `base_source_sha` — asking for an always-empty column only gives the parser a blank to
/// misread.
fn read_script_provenance(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
) -> Result<Vec<Applied>> {
    let recorded = ask(
        project,
        runner,
        database,
        &format!(
            "SELECT package, artifact_hash FROM game_package_import WHERE family = \
             '{SCRIPT_FAMILY}'"
        ),
    )?;
    Ok(rows(&recorded, &["package", "artifact_hash"])
        .into_iter()
        .filter(|row| row.len() >= 2)
        .map(|row| (row[0].clone(), row[1].clone(), String::new()))
        .collect())
}

/// Read one Shard's `game_import_meta.source_sha` for a family with a base import.
///
/// Empty when the family has never been stamped here, which is a fact rather than a reason to
/// refuse.
fn read_base_stamp(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
    family: &str,
) -> Result<String> {
    let stamped = ask(
        project,
        runner,
        database,
        &format!("SELECT source_sha FROM game_import_meta WHERE family = '{family}'"),
    )?;
    Ok(rows(&stamped, &["source_sha"])
        .first()
        .and_then(|row| row.first().cloned())
        .unwrap_or_default())
}

/// Does this Shard already hold exactly these artifacts for `family`?
///
/// Matches when every enabled Package is recorded with a matching digest, no other Package is
/// recorded, and — when `base` is `Some` — every row sits on the Shard's current base stamp (a claim
/// recorded against a replaced import is stale; the script family has no base import, so it passes
/// `None`). Any mismatch means apply; a total match returns the reason it is complete.
fn already_complete(
    family: &str,
    digests: &[(&str, &str)],
    recorded: &[Applied],
    base: Option<&str>,
) -> Option<String> {
    for (package, artifact_hash) in digests {
        let row = recorded.iter().find(|(recorded, ..)| recorded == package)?;
        if row.1 != *artifact_hash {
            return None;
        }
        if base.is_some_and(|sha| row.2 != sha) {
            return None;
        }
    }
    if recorded.len() != digests.len() {
        return None;
    }
    let on_base = if base.is_some() {
        " and base import"
    } else {
        ""
    };
    Some(match digests.len() {
        0 => format!("no enabled Package ships a {family} artifact, and none is recorded here"),
        1 => format!("1 Package, matching artifact digest{on_base}"),
        n => format!("{n} Packages, matching artifact digests{on_base}"),
    })
}

/// The `(package, artifact_hash)` pairs a family's completeness check compares.
fn delta_digests(artifacts: &[Artifact]) -> Vec<(&str, &str)> {
    artifacts
        .iter()
        .map(|a| (a.package.as_str(), a.artifact_hash.as_str()))
        .collect()
}

fn script_digests(artifacts: &[ScriptArtifact]) -> Vec<(&str, &str)> {
    artifacts
        .iter()
        .map(|a| (a.package.as_str(), a.artifact_hash.as_str()))
        .collect()
}

/// Apply the whole enabled script plan to one Shard.
///
/// `spacetime call`, not the importer: the script family has no base import to reimport into. Both
/// arguments are JSON literals — what `spacetime call` parses each argument as, and the payload's
/// quotes and newlines need escaping either way.
fn script_apply_command(
    project: &ProjectLayout,
    database: &str,
    packed: &str,
) -> Result<CommandSpec> {
    validate_database(VERB, database)?;
    Ok(import::call_command(project, database, APPLY_REDUCER)
        .arg(json_argument(SCRIPT_FAMILY))
        .arg(json_argument(packed)))
}

/// One reducer argument, as a JSON string literal. Encoding a `&str` as JSON cannot fail.
fn json_argument(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

/// What the module said about a refused script plan, without the rendered command.
///
/// The whole plan travels as one argument, so the rendered `spacetime call` IS the payload. Quoting
/// it back at the operator would bury the refusal it exists to explain.
fn refusal(error: &Error) -> String {
    match error {
        Error::SubprocessFailed { code, message, .. } => {
            format!("{APPLY_REDUCER} refused the script plan (exit {code}): {message}")
        }
        other => other.to_string(),
    }
}

/// The command that resumes this run — the same one, because resume is what re-running does.
fn resume_command(shards: &[String], options: &ApplyOptions) -> String {
    let mut line = format!("lyracore packages apply {}", shards.join(" "));
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

/// Apply every enabled Package's Delta across the named Shards.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    options: &ApplyOptions,
) -> Result<()> {
    let shards = if options.databases.is_empty() {
        recorded_databases(project)?
    } else {
        options.databases.clone()
    };
    if shards.is_empty() {
        return Err(Error::Usage(
            "no Shard to apply onto. Name the databases explicitly, e.g. `lyracore packages \
             apply lyracore lyracore-kalimdor`."
                .to_string(),
        ));
    }
    databases(&shards)?;
    check::prepare(project, runner, prompt, options.client_data.as_deref())?;
    let has_rust = inventory(project)?
        .iter()
        .any(|package| package.dir.join("src/mod.rs").is_file());

    // ---- preflight: the artifacts, once, before any target is touched ----
    let root = project.packages_dir();
    let enabled = artifact::read_enabled(&root)?;
    let artifacts = &enabled.deltas;
    let scripts = &enabled.scripts;
    let conflicts = artifact::conflicts(artifacts);
    if !conflicts.is_empty() {
        return Err(Error::Usage(format!(
            "{} claim conflict(s) between enabled Packages — nothing was applied:\n{}\nResolve \
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
    let collisions = script::collisions(scripts);
    if !collisions.is_empty() {
        return Err(Error::Usage(format!(
            "{} Runtime Script collision(s) between enabled Packages — nothing was applied:\n{}\n\
             A script belongs to one Package outright, so there is nothing to merge and no priority \
             to break the tie with. Resolve them by disabling a Package (`lyracore packages disable \
             NAME`) or by renumbering or renaming one script. The module refuses the same plan.",
            collisions.len(),
            collisions
                .iter()
                .map(|c| format!("  {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    let packed = script::pack(scripts);

    println!();
    println!("=== enabled Package Deltas ({}) ===", root.display());
    if artifacts.is_empty() {
        println!("  no enabled Package claims the {SPELL_FAMILY} import family");
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

    println!();
    println!(
        "=== enabled Package Runtime Scripts ({}) ===",
        root.display()
    );
    if scripts.is_empty() {
        println!("  no enabled Package ships a Runtime Script");
    }
    for artifact in scripts {
        println!(
            "  {:<32} {:>3} script(s)   {}",
            artifact.package,
            artifact.scripts().len(),
            artifact.path.display()
        );
        for one in artifact.scripts() {
            println!("    {:>8}  {:<32} {}", one.script_id, one.name, one.event);
        }
    }

    // ---- preflight: every target, before the first write ----
    println!();
    println!("=== targets ===");
    let delta_digests = delta_digests(artifacts);
    let script_digests = script_digests(scripts);
    let mut targets = Vec::with_capacity(shards.len());
    let mut publish_shards = Vec::new();
    for database in &shards {
        let teardown = ask(
            project,
            runner,
            database,
            "SELECT package_name FROM game_package_teardown",
        )?;
        if has_rust || !rows(&teardown, &["package_name"]).is_empty() {
            publish_shards.push(database.clone());
        }
        let spell_rows = read_spell_provenance(project, runner, database)?;
        let base = read_base_stamp(project, runner, database, SPELL_FAMILY)?;
        let script_rows = read_script_provenance(project, runner, database)?;
        let target = if options.force_all {
            Target {
                database: database.clone(),
                spell: None,
                script: None,
            }
        } else {
            Target {
                database: database.clone(),
                spell: already_complete(SPELL_FAMILY, &delta_digests, &spell_rows, Some(&base)),
                script: already_complete(SCRIPT_FAMILY, &script_digests, &script_rows, None),
            }
        };
        report_target(&target);
        targets.push(target);
    }
    if !publish_shards.is_empty() {
        println!(
            "  publish Module and repair schedules: {}",
            shard_list(&publish_shards)
        );
    }

    let wanted: Vec<&Target> = targets.iter().filter(|t| t.wanted()).collect();
    let skipped: Vec<String> = targets
        .iter()
        .filter(|t| !t.wanted())
        .map(|t| t.database.clone())
        .collect();
    let spell_wanted = targets.iter().any(|target| target.spell.is_none());
    // A check still renders an enabled spell plan on every Shard, even where it is already
    // complete. With no Package Deltas and no stale spell provenance, there is no spell plan to
    // execute and the script family's check must not acquire an unrelated DBC prerequisite.
    let spell_check_wanted = options.check && (spell_wanted || !artifacts.is_empty());
    let client_data = if spell_wanted || spell_check_wanted {
        Some(import::resolve_client_data(
            project,
            prompt,
            options.client_data.as_deref(),
        )?)
    } else {
        None
    };

    if options.check {
        if !publish_shards.is_empty() {
            preflight::run(project, runner)?;
            publish::require_loot_roll_upgrade_ready(project, runner, &publish_shards)?;
        }
        println!();
        println!("=== check: the plan, per Shard, writing nothing ===");
        if spell_check_wanted {
            runner.run_streaming(&import::build_importer_command(project))?;
        }
        for target in &targets {
            println!();
            println!("==> checking {}", target.database);
            if spell_check_wanted {
                let client_data = client_data.as_deref().ok_or_else(|| {
                    Error::Process(
                        "the spell family needs client data, but preflight did not resolve it"
                            .to_string(),
                    )
                })?;
                runner.run_streaming(&apply_command(
                    project,
                    &target.database,
                    client_data,
                    false,
                )?)?;
            } else if let Some(reason) = &target.spell {
                println!("  {SPELL_FAMILY}: already complete — {reason}");
            }
            println!(
                "  {SCRIPT_FAMILY}: {} Package(s), {} Runtime Script(s) would be applied",
                scripts.len(),
                script_count(scripts)
            );
        }
        println!();
        println!("check only: no Realm changes. Re-run without --check to apply.");
        return Ok(());
    }

    if wanted.is_empty() && publish_shards.is_empty() {
        println!();
        println!(
            "every named Shard already holds these {} Package Delta(s) on its current base import, \
             and these {} Runtime Script(s): {}",
            artifacts.len(),
            script_count(scripts),
            shard_list(&skipped)
        );
        println!("nothing to apply. Use --force-all to reapply anyway.");
        return Ok(());
    }

    let pending: Vec<String> = wanted.iter().map(|t| t.database.clone()).collect();
    let mut confirmation = if wanted.is_empty() {
        String::new()
    } else {
        format!(
            "{} Proceed?",
            question(&wanted, artifacts, scripts, &pending)
        )
    };
    if !publish_shards.is_empty() {
        confirmation = format!(
            "Publish this checkout's Module and repair schedules on {}. {confirmation}",
            shard_list(&publish_shards)
        );
    }
    println!();
    crate::cmd::packages::confirm(
        prompt,
        &format!("{confirmation} Proceed?"),
        "Nothing was applied.",
        options.yes,
    )?;

    // Only the spell family runs through the importer. A run that has nothing but Runtime Scripts
    // to carry does not build it.
    if wanted.iter().any(|t| t.spell.is_none()) {
        runner.run_streaming(&import::build_importer_command(project))?;
    }
    if !publish_shards.is_empty() {
        publish::run_and_repair(project, runner, &publish_shards).map_err(|error| {
            Error::Process(format!(
                "{error}\n  Artifact application has not started. Retry with:\n    {}",
                resume_command(&shards, options)
            ))
        })?;
    }

    let mut completed = Vec::new();
    let stop_context = StopContext {
        targets: &targets,
        wanted: &wanted,
        shards: &shards,
        options,
    };

    for (index, target) in wanted.iter().enumerate() {
        println!();
        println!("==> applying {}", target.database);

        match &target.spell {
            Some(reason) => println!("  {SPELL_FAMILY}: already complete — {reason}"),
            None => {
                let client_data = client_data.as_deref().ok_or_else(|| {
                    Error::Process(
                        "the spell family needs client data, but preflight did not resolve it"
                            .to_string(),
                    )
                })?;
                let command = apply_command(project, &target.database, client_data, true)?;
                if let Err(error) = runner.run_streaming(&command) {
                    return Err(stopped(
                        &target.database,
                        SPELL_FAMILY,
                        index,
                        &error.to_string(),
                        &completed,
                        &stop_context,
                    ));
                }
                completed.push((target.database.clone(), SPELL_FAMILY));
            }
        }

        match &target.script {
            Some(reason) => println!("  {SCRIPT_FAMILY}: already complete — {reason}"),
            None => {
                println!(
                    "  {SCRIPT_FAMILY}: applying {} Runtime Script(s) from {} Package(s)",
                    script_count(scripts),
                    scripts.len()
                );
                let command = script_apply_command(project, &target.database, &packed)?;
                if let Err(error) = runner.run_and_wait(&command) {
                    return Err(stopped(
                        &target.database,
                        SCRIPT_FAMILY,
                        index,
                        &refusal(&error),
                        &completed,
                        &stop_context,
                    ));
                }
                completed.push((target.database.clone(), SCRIPT_FAMILY));
            }
        }
    }

    println!();
    println!("applied:");
    println!(
        "{}",
        applied(
            SPELL_FAMILY,
            &format!("{} enabled Package Delta(s)", artifacts.len()),
            &written(&wanted, |t| t.spell.is_none()),
        )
    );
    println!(
        "{}",
        applied(
            SCRIPT_FAMILY,
            &format!(
                "{} Runtime Script(s) from {} Package(s)",
                script_count(scripts),
                scripts.len()
            ),
            &written(&wanted, |t| t.script.is_none()),
        )
    );
    if !skipped.is_empty() {
        println!(
            "already complete in both families, untouched: {}",
            shard_list(&skipped)
        );
    }
    Ok(())
}

struct StopContext<'a, 'target> {
    targets: &'a [Target],
    wanted: &'a [&'target Target],
    shards: &'a [String],
    options: &'a ApplyOptions,
}

/// Report a stopped apply by Import Family, including a family that completed on the Shard where
/// a later family refused its plan.
fn stopped(
    failed: &str,
    family: &str,
    index: usize,
    cause: &str,
    completed: &[(String, &'static str)],
    context: &StopContext<'_, '_>,
) -> Error {
    let completed_spell = completed_in(completed, SPELL_FAMILY);
    let completed_script = completed_in(completed, SCRIPT_FAMILY);
    let skipped_spell = already_complete_in(context.targets, |target| target.spell.is_some());
    let skipped_script = already_complete_in(context.targets, |target| target.script.is_some());
    let untouched = context.wanted[index + 1..]
        .iter()
        .map(|target| target.database.clone())
        .collect::<Vec<_>>();

    Error::Process(format!(
        "{cause}\n  apply stopped at: {failed} ({family} family)\n  completed this run:\n    \
         {SPELL_FAMILY}: {completed_spell}\n    {SCRIPT_FAMILY}: {completed_script}\n  already complete \
         before this run:\n    {SPELL_FAMILY}: {skipped_spell}\n    {SCRIPT_FAMILY}: \
         {skipped_script}\n  unattempted artifact Shards: {untouched}\n\n  Earlier completed \
         families stay applied. Script reconciliation is one transaction. A failed spell import \
         may already have replaced base rows. Fix the cause and re-run the SAME command. \
         Provenance makes completed families skip:\n    {resume}",
        completed_spell = shard_list(&completed_spell),
        completed_script = shard_list(&completed_script),
        skipped_spell = shard_list(&skipped_spell),
        skipped_script = shard_list(&skipped_script),
        untouched = shard_list(&untouched),
        resume = resume_command(context.shards, context.options),
    ))
}

fn completed_in(completed: &[(String, &'static str)], family: &str) -> Vec<String> {
    completed
        .iter()
        .filter(|(_, completed_family)| *completed_family == family)
        .map(|(shard, _)| shard.clone())
        .collect()
}

fn already_complete_in(targets: &[Target], complete: impl Fn(&Target) -> bool) -> Vec<String> {
    targets
        .iter()
        .filter(|target| complete(target))
        .map(|target| target.database.clone())
        .collect()
}

/// The Shards a family actually wrote to.
fn written(wanted: &[&Target], needed: impl Fn(&Target) -> bool) -> Vec<String> {
    wanted
        .iter()
        .filter(|t| needed(t))
        .map(|t| t.database.clone())
        .collect()
}

/// One family's closing line: what it applied and where, or that it had nothing to do. A family
/// every named Shard already held must not read as a family this run reapplied.
fn applied(family: &str, what: &str, written: &[String]) -> String {
    if written.is_empty() {
        format!("  {family}: nothing — every named Shard was already complete")
    } else {
        format!("  {family}: {what} -> {}", shard_list(written))
    }
}

/// One target's line in the preflight report: one row per Import Family, because the two are
/// decided independently.
fn report_target(target: &Target) {
    for (name, complete) in [
        (SPELL_FAMILY, &target.spell),
        (SCRIPT_FAMILY, &target.script),
    ] {
        let state = match complete {
            Some(reason) => format!("already complete — {reason}"),
            None => "apply".to_string(),
        };
        // The Shard is named once, on the first line, so two families of one Shard read as one
        // entry rather than two.
        let shard = if name == SPELL_FAMILY {
            target.database.as_str()
        } else {
            ""
        };
        println!("  {shard:<24} {name}: {state}");
    }
}

fn script_count(scripts: &[ScriptArtifact]) -> usize {
    scripts.iter().map(|a| a.scripts().len()).sum()
}

/// What the operator is agreeing to, named as its consequence rather than as a count.
///
/// An empty plan for a family clears that family's whole Package range — legitimate, but exactly
/// what an accidentally disabled Package looks like, so it is said out loud or not at all.
fn question(
    wanted: &[&Target],
    artifacts: &[Artifact],
    scripts: &[ScriptArtifact],
    pending: &[String],
) -> String {
    let mut clauses = Vec::new();
    if wanted.iter().any(|t| t.spell.is_none()) {
        clauses.push(if artifacts.is_empty() {
            "CLEAR the Package Spell Range — every row an unenabled Package invented is deleted"
                .to_string()
        } else {
            format!(
                "reimport Spell.dbc and apply {} enabled Package Delta(s) over it",
                artifacts.len()
            )
        });
    }
    if wanted.iter().any(|t| t.script.is_none()) {
        clauses.push(if scripts.is_empty() {
            "CLEAR the Package Script Range — every Runtime Script a Package shipped is deleted"
                .to_string()
        } else {
            format!(
                "reconcile the Runtime Scripts to the {} shipped by {} enabled Package(s)",
                script_count(scripts),
                scripts.len()
            )
        });
    }
    format!(
        "Applying will {} on {}.",
        clauses.join(", and "),
        shard_list(pending)
    )
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
            std::fs::create_dir_all(project.datascripts_dir()).unwrap();
            for name in ["tsconfig.json", "package.json", "bun.lock"] {
                std::fs::write(project.datascripts_dir().join(name), "{}").unwrap();
            }
            std::fs::create_dir_all(project.datascript_types_dir()).unwrap();
            std::fs::write(project.base_snapshot_file(), "{}").unwrap();
            Self { tmp, project }
        }

        fn client_data(&self) -> String {
            self.tmp.path().join("client-data").display().to_string()
        }

        fn generated(&self, package: &str) -> std::path::PathBuf {
            let dir = self
                .tmp
                .path()
                .join("packages")
                .join(package)
                .join("data/.generated");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// An enabled Package whose artifact tunes one column of one real spell.
        fn with_package(&self, package: &str, spell_id: u32, value: u32) -> &Self {
            std::fs::write(
                self.generated(package).join("spell.json"),
                format!(
                    r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":{spell_id}}},"operation":"update","fields":{{"cooldown_ms":{{"type":"u32","value":{value}}}}}}}]}}"#
                ),
            )
            .unwrap();
            super::super::identity::write_all(&self.project, &self.enabled().deltas).unwrap();
            self
        }

        /// An enabled Package that ships one Runtime Script.
        fn with_script(&self, package: &str, script_id: u32, name: &str) -> &Self {
            std::fs::write(
                self.generated(package).join("script.json"),
                format!(
                    r#"{{"kind":"script","version":1,"package":"{package}","source_hash":"{HASH_A}","scripts":[{{"script_id":{script_id},"name":"{name}","event":"on_login","priority":0,"enabled":true,"source":"grant_xp(event.actor, 10)"}}]}}"#
                ),
            )
            .unwrap();
            self
        }

        fn enabled(&self) -> artifact::Enabled {
            artifact::read_enabled(&self.project.packages_dir()).expect("the inventory reads")
        }

        fn artifact_hash(&self, package: &str) -> String {
            self.enabled()
                .deltas
                .into_iter()
                .find(|a| a.package == package)
                .expect("the package is enabled")
                .artifact_hash
        }

        fn script_hash(&self, package: &str) -> String {
            self.enabled()
                .scripts
                .into_iter()
                .find(|a| a.package == package)
                .expect("the package ships a Script Artifact")
                .artifact_hash
        }

        /// The canonical payload this checkout's Script Artifacts pack into.
        fn script_payload(&self) -> String {
            script::pack(&self.enabled().scripts)
        }

        fn options(&self, shards: &[&str]) -> ApplyOptions {
            ApplyOptions {
                databases: shards.iter().map(|s| (*s).to_string()).collect(),
                client_data: Some(self.client_data()),
                yes: true,
                ..Default::default()
            }
        }

        fn options_without_client_data(&self, shards: &[&str]) -> ApplyOptions {
            ApplyOptions {
                databases: shards.iter().map(|s| (*s).to_string()).collect(),
                yes: true,
                ..Default::default()
            }
        }

        fn remove_client_data(&self) {
            std::fs::remove_dir_all(self.tmp.path().join("client-data")).unwrap();
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

    /// What `game_package_import` holds for one Shard's spell family.
    fn with_spell_provenance(stack: FakeStack, database: &str, rows: &[Vec<String>]) -> FakeStack {
        stack.with_stdout(
            &format!(
                "{database} SELECT package, artifact_hash, base_source_sha FROM \
                 game_package_import WHERE family = '{SPELL_FAMILY}'"
            ),
            &sql_rows(&["package", "artifact_hash", "base_source_sha"], rows),
        )
    }

    /// What it holds for the script family. Two columns, matching the query that has no base import
    /// to ask about.
    fn with_script_provenance(stack: FakeStack, database: &str, rows: &[Vec<String>]) -> FakeStack {
        stack.with_stdout(
            &format!(
                "{database} SELECT package, artifact_hash FROM game_package_import WHERE family = \
                 '{SCRIPT_FAMILY}'"
            ),
            &sql_rows(&["package", "artifact_hash"], rows),
        )
    }

    /// A Shard that has never applied a Package artifact but has a stamped spell import.
    fn fresh_shard(stack: FakeStack, database: &str) -> FakeStack {
        let stack = stack.with_stdout(
            &format!("{database} SELECT source_sha"),
            &sql_rows(&["source_sha"], &[vec![BASE_SHA.to_string()]]),
        );
        let stack = with_spell_provenance(stack, database, &[]);
        with_script_provenance(stack, database, &[])
    }

    /// A Shard already running exactly `package` at `hash` in the spell family, on the current base
    /// import, and holding no Runtime Script.
    fn applied_shard(stack: FakeStack, database: &str, package: &str, hash: &str) -> FakeStack {
        let stack = fresh_shard(stack, database);
        with_spell_provenance(
            stack,
            database,
            &[vec![
                package.to_string(),
                hash.to_string(),
                BASE_SHA.to_string(),
            ]],
        )
    }

    /// A Shard already holding exactly `package`'s Script Artifact at `hash`.
    fn scripted_shard(stack: FakeStack, database: &str, package: &str, hash: &str) -> FakeStack {
        with_script_provenance(
            stack,
            database,
            &[vec![package.to_string(), hash.to_string()]],
        )
    }

    fn applies(stack: &FakeStack) -> Vec<String> {
        stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("lyracore-importer") && r.contains("--apply"))
            .collect()
    }

    /// Every `apply_package_deltas` call this run made — the script family's whole write path.
    fn script_applies(stack: &FakeStack) -> Vec<String> {
        stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains(APPLY_REDUCER))
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
    fn every_named_shard_is_applied_in_order() {
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
            message.contains("apply stopped at: lyracore-kalimdor"),
            "{message}"
        );
        assert!(
            message.contains("spell: lyracore\n    script: (none)"),
            "{message}"
        );
        assert!(
            message.contains("unattempted artifact Shards: lyracore-instances"),
            "{message}"
        );
        assert!(
            message
                .contains("lyracore packages apply lyracore lyracore-kalimdor lyracore-instances"),
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
        let options = ApplyOptions {
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
    fn a_shard_on_a_different_base_import_is_applied_even_though_its_digests_match() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = with_spell_provenance(
            fresh_shard(FakeStack::new(), "lyracore-kalimdor"),
            "lyracore-kalimdor",
            &[vec![
                "bolt".to_string(),
                hash,
                "an-older-spell-dbc".to_string(),
            ]],
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

    /// Removing a Package must apply every affected target with the REMAINING set. The payload is
    /// built by the importer from the enabled inventory, so the proof is that the disabled
    /// Package's artifact is no longer in the tree the importer is pointed at, and that the Shard
    /// still holding its provenance row is not treated as complete.
    #[test]
    fn a_shard_still_recording_a_disabled_package_is_applied_with_the_remaining_set() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let bolt = checkout.artifact_hash("bolt");
        // `lyracore-kalimdor` also recorded `ember`, which is no longer enabled in this checkout.
        let stack = with_spell_provenance(
            fresh_shard(FakeStack::new(), "lyracore-kalimdor"),
            "lyracore-kalimdor",
            &[
                vec!["bolt".to_string(), bolt, BASE_SHA.to_string()],
                vec![
                    "ember".to_string(),
                    "whatever-ember-hashed-to".to_string(),
                    BASE_SHA.to_string(),
                ],
            ],
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
            "an extra provenance row forces a apply: {applied:?}"
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
        let options = ApplyOptions {
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

    /// An enabled spell plan is still checked on a complete Shard. This is an explicit check, not
    /// the shortest apply path.
    #[test]
    fn check_reports_every_named_shard_including_the_complete_ones() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let hash = checkout.artifact_hash("bolt");
        let stack = applied_shard(FakeStack::new(), "lyracore-kalimdor", "bolt", &hash);
        let options = ApplyOptions {
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
    fn an_empty_enabled_inventory_states_what_it_clears_before_it_applies() {
        let checkout = Checkout::new();
        // Nothing enabled, but the Shard still records a Package from an earlier run.
        let stack = applied_shard(FakeStack::new(), "lyracore", "bolt", "stale-digest");
        let prompt = ScriptedPrompt::new(&["yes"]);

        run(
            &checkout.project,
            &stack.runner(),
            &prompt,
            &ApplyOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap();

        let asked = prompt.asked();
        assert!(asked.iter().any(|q| q.contains("CLEAR")), "{asked:?}");
        assert!(
            asked.iter().any(|q| q.contains("Package Spell Range")),
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
            &ApplyOptions {
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
            stack
                .rendered()
                .iter()
                .all(|call| !call.contains("spacetime sql")),
            "no Shard may be read before the artifacts are cleared: {:?}",
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
    fn a_refused_confirmation_applies_nothing() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["no"]),
            &ApplyOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("Nothing was applied"), "{error}");
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
        // Package Teardown and each artifact family are read before writes.
        assert_eq!(queries.len(), 4, "{queries:?}");
        for query in &queries {
            assert!(!query.contains("ORDER BY"), "{query}");
            assert!(!query.contains(" IN ("), "{query}");
        }
        assert_eq!(
            queries
                .iter()
                .filter(|q| q.contains("WHERE family = 'script'"))
                .count(),
            1,
            "{queries:?}"
        );
    }

    // ---- the script family ----

    /// The acceptance criterion the issue is written for: one command carries a Package's script
    /// edit to every Shard, with no hand-written `spacetime call` behind it.
    #[test]
    fn the_script_family_reaches_every_named_shard_in_one_run() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let mut stack = FakeStack::new();
        for shard in ["lyracore", "lyracore-kalimdor"] {
            stack = fresh_shard(stack, shard);
        }

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore", "lyracore-kalimdor"]),
        )
        .unwrap();

        let applied = script_applies(&stack);
        assert_eq!(applied.len(), 2, "{applied:?}");
        for (rendered, shard) in applied.iter().zip(["lyracore", "lyracore-kalimdor"]) {
            assert!(
                rendered.contains(&format!(" {shard} {APPLY_REDUCER} ")),
                "{rendered}"
            );
        }
    }

    /// Both arguments are JSON literals, which is what `spacetime call` parses each argument as.
    /// The payload is the canonical artifact, so the digest the Shard records describes what the
    /// artifact SAYS rather than how it was spelled.
    #[test]
    fn the_script_plan_travels_as_json_arguments_carrying_the_canonical_artifact() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap();

        let applied = script_applies(&stack);
        assert_eq!(applied.len(), 1, "{applied:?}");
        assert!(applied[0].contains(r#" "script" "#), "{}", applied[0]);
        assert!(
            applied[0].contains(&json_argument(&checkout.script_payload())),
            "{}",
            applied[0]
        );
        assert!(
            checkout.script_payload().contains(r#""name":"bolt.greet""#),
            "{}",
            checkout.script_payload()
        );
    }

    #[test]
    fn a_shard_whose_script_provenance_already_matches_is_reported_complete_and_never_reapplied() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let hash = checkout.script_hash("bolt");
        let stack = scripted_shard(
            fresh_shard(
                fresh_shard(FakeStack::new(), "lyracore"),
                "lyracore-kalimdor",
            ),
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

        let applied = script_applies(&stack);
        assert_eq!(applied.len(), 1, "{applied:?}");
        assert!(applied[0].contains(" lyracore "), "{applied:?}");
    }

    #[test]
    fn force_all_reapplies_a_script_plan_a_shard_already_holds() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let hash = checkout.script_hash("bolt");
        let stack = scripted_shard(
            fresh_shard(FakeStack::new(), "lyracore"),
            "lyracore",
            "bolt",
            &hash,
        );
        let options = ApplyOptions {
            force_all: true,
            ..checkout.options(&["lyracore"])
        };

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &options,
        )
        .unwrap();

        assert_eq!(script_applies(&stack).len(), 1, "{:?}", stack.rendered());
    }

    /// Reconciliation is how a disabled Package's scripts leave a Shard, so the empty plan is a
    /// statement that must still be sent — and, because it deletes, one the operator says out loud.
    #[test]
    fn an_empty_script_plan_is_still_applied_where_a_package_script_is_recorded() {
        let checkout = Checkout::new();
        let stack = scripted_shard(
            fresh_shard(FakeStack::new(), "lyracore"),
            "lyracore",
            "bolt",
            "whatever-bolt-hashed-to",
        );
        let prompt = ScriptedPrompt::new(&["yes"]);

        run(
            &checkout.project,
            &stack.runner(),
            &prompt,
            &ApplyOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap();

        let asked = prompt.asked();
        assert!(
            asked
                .iter()
                .any(|q| q.contains("CLEAR the Package Script Range")),
            "{asked:?}"
        );
        let applied = script_applies(&stack);
        assert_eq!(applied.len(), 1, "{applied:?}");
        assert!(
            applied[0].ends_with(r#" "script" """#),
            "the empty plan is sent explicitly: {}",
            applied[0]
        );
    }

    /// The other half of the same decision: with nothing enabled AND nothing recorded, the Package
    /// script range is already empty. There is no work, so there is no question and no write.
    #[test]
    fn an_empty_script_plan_over_a_shard_holding_no_package_script_is_already_complete() {
        let checkout = Checkout::new();
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &ApplyOptions {
                yes: false,
                ..checkout.options(&["lyracore"])
            },
        )
        .unwrap();

        assert!(script_applies(&stack).is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn a_refused_script_plan_stops_the_run_naming_the_shard_the_family_and_the_refusal() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let mut stack = FakeStack::new();
        for shard in ["lyracore", "lyracore-kalimdor", "lyracore-instances"] {
            stack = fresh_shard(stack, shard);
        }
        let stack = stack.fail_on(
            &format!("lyracore-kalimdor {APPLY_REDUCER}"),
            "1 Runtime Script conflicts, nothing applied",
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
            message.contains("apply stopped at: lyracore-kalimdor (script family)"),
            "{message}"
        );
        assert!(message.contains("nothing applied"), "{message}");
        assert!(
            message.contains("script: lyracore\n  already complete"),
            "{message}"
        );
        assert!(
            message.contains("unattempted artifact Shards: lyracore-instances"),
            "{message}"
        );
        assert!(
            !script_applies(&stack)
                .iter()
                .any(|r| r.contains(" lyracore-instances ")),
            "the run must stop at the failure: {:?}",
            script_applies(&stack)
        );
    }

    /// The whole plan is one argument, so the rendered command IS the payload. A refusal has to
    /// report what the module said, not quote a megabyte of Lua back at the operator.
    #[test]
    fn a_refusal_reports_what_the_module_said_rather_than_the_rendered_payload() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let stack = fresh_shard(FakeStack::new(), "lyracore")
            .fail_on(APPLY_REDUCER, "script 100001 is not in the Package band");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("is not in the Package band"), "{message}");
        assert!(
            !message.contains("grant_xp"),
            "the payload is not quoted back: {message}"
        );
    }

    #[test]
    fn a_runtime_script_collision_fails_the_run_before_the_first_shard_is_read() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "shared.greet");
        checkout.with_script("ember", 100_002, "shared.greet");
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(
            error.to_string().contains("Runtime Script collision"),
            "{error}"
        );
        assert!(
            stack
                .rendered()
                .iter()
                .all(|call| !call.contains("spacetime sql")),
            "no Shard may be read before the artifacts are cleared: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_script_only_check_needs_no_client_data_and_calls_no_reducer() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        checkout.remove_client_data();
        let stack = fresh_shard(FakeStack::new(), "lyracore");
        let options = ApplyOptions {
            check: true,
            yes: false,
            ..checkout.options_without_client_data(&["lyracore"])
        };

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &options,
        )
        .unwrap();

        assert!(script_applies(&stack).is_empty(), "{:?}", stack.rendered());
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|call| call.contains("lyracore-importer")),
            "{:?}",
            stack.rendered()
        );
    }

    /// Only the spell family runs through the importer, so a run carrying nothing but Runtime
    /// Scripts neither needs client data nor pays for building it.
    #[test]
    fn a_script_only_run_needs_no_client_data_and_never_builds_the_importer() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        checkout.remove_client_data();
        let stack = fresh_shard(FakeStack::new(), "lyracore");

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["lyracore"]),
        )
        .unwrap();

        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("cargo build") && r.contains("lyracore-importer")),
            "{:?}",
            stack.rendered()
        );
        assert_eq!(script_applies(&stack).len(), 1, "{:?}", stack.rendered());
    }

    #[test]
    fn a_script_only_no_op_needs_no_client_data() {
        let checkout = Checkout::new();
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let hash = checkout.script_hash("bolt");
        checkout.remove_client_data();
        let stack = scripted_shard(
            fresh_shard(FakeStack::new(), "lyracore"),
            "lyracore",
            "bolt",
            &hash,
        );

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &ApplyOptions {
                yes: false,
                ..checkout.options_without_client_data(&["lyracore"])
            },
        )
        .unwrap();

        assert!(script_applies(&stack).is_empty(), "{:?}", stack.rendered());
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|call| call.contains("lyracore-importer")),
            "{:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_script_refusal_reports_the_spell_family_that_completed_on_the_same_shard() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let stack = fresh_shard(FakeStack::new(), "lyracore")
            .fail_on(APPLY_REDUCER, "the script plan was refused");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap_err();

        let message = error.to_string();
        assert_eq!(applies(&stack).len(), 1, "{:?}", stack.rendered());
        assert!(
            message.contains("completed this run:\n    spell: lyracore\n    script: (none)"),
            "{message}"
        );
        assert!(
            message.contains("Any family that completed earlier stays applied."),
            "{message}"
        );
    }

    /// The two families are decided independently: a Shard can hold this checkout's Package Deltas
    /// and none of its Runtime Scripts, which is what every Realm looks like the first time a
    /// Package ships a script.
    #[test]
    fn a_shard_complete_for_one_family_is_still_applied_for_the_other() {
        let checkout = Checkout::new();
        checkout.with_package("bolt", 133, 1500);
        checkout.with_script("bolt", 100_001, "bolt.greet");
        let hash = checkout.artifact_hash("bolt");
        let stack = applied_shard(FakeStack::new(), "lyracore", "bolt", &hash);

        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&["yes"]),
            &checkout.options(&["lyracore"]),
        )
        .unwrap();

        assert!(
            applies(&stack).is_empty(),
            "the spell family is complete here: {:?}",
            applies(&stack)
        );
        assert_eq!(
            script_applies(&stack).len(),
            1,
            "the script family is not: {:?}",
            stack.rendered()
        );
    }
    impl Checkout {
        fn with_module(&self) -> &Self {
            use crate::proc::fake::{FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
            let root = &self.project.root;
            std::fs::write(
                root.join(ProjectLayout::RUST_TOOLCHAIN),
                format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
            )
            .unwrap();
            std::fs::create_dir_all(root.join("module/src")).unwrap();
            std::fs::write(
                root.join("module/Cargo.toml"),
                format!("spacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"),
            )
            .unwrap();
            std::fs::write(root.join("module/src/lib.rs"),
                "#[client_visibility_filter]\nconst RLS: Filter = Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n").unwrap();
            self
        }

        fn with_rust(&self) -> &Self {
            self.with_module();
            let dir = self.project.packages_dir().join("greeter/src");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("mod.rs"), "pub fn register() {}\n").unwrap();
            self
        }

        fn with_script_source(&self) -> &Self {
            let dir = self.project.package_scripts_dir("greeter");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("welcome.ts"),
                "// @id 100300\n// @event on_login\n",
            )
            .unwrap();
            self
        }
    }

    /// Substitutes only the compiler and snapshot extractor subprocesses. Artifact discovery,
    /// Build Identity checks, preparation, and activation use the real command implementation.
    struct PackageCompiler<'a> {
        checkout: &'a Checkout,
        stack: FakeStack,
    }

    impl ProcessRunner for PackageCompiler<'_> {
        fn run_and_wait(&self, cmd: &CommandSpec) -> Result<String> {
            self.stack.runner().run_and_wait(cmd)
        }
        fn run_capturing_stderr(&self, cmd: &CommandSpec) -> Result<String> {
            self.stack.runner().run_capturing_stderr(cmd)
        }
        fn run_with_secret_stdin(&self, cmd: &CommandSpec, secret: &[u8]) -> Result<String> {
            self.stack.runner().run_with_secret_stdin(cmd, secret)
        }
        fn spawn_logged(&self, cmd: &CommandSpec, log: &Path) -> Result<u32> {
            self.stack.runner().spawn_logged(cmd, log)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.stack.runner().terminate(pid)
        }
        fn run_streaming(&self, cmd: &CommandSpec) -> Result<()> {
            self.stack.runner().run_streaming(cmd)?;
            if cmd.args().iter().any(|arg| arg == "--spell-snapshot") {
                std::fs::write(self.checkout.project.base_snapshot_file(), "{}")?;
            }
            if cmd.program() == "bun" {
                if cmd
                    .args()
                    .iter()
                    .any(|arg| arg.ends_with("build-scripts.ts"))
                {
                    self.checkout
                        .with_script("greeter", 100300, "greeter.login");
                    std::fs::rename(
                        self.checkout.generated("greeter").join("script.json"),
                        self.checkout
                            .generated("greeter")
                            .join("greeter.script.json"),
                    )?;
                }
                if cmd
                    .args()
                    .iter()
                    .any(|arg| arg.ends_with("datascripts/spells.ts"))
                {
                    self.checkout.with_package("greeter", 133, 2000);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn missing_script_artifacts_build_and_activate_in_one_command() {
        let checkout = Checkout::new();
        checkout.with_script_source();
        let stack = FakeStack::new().with_stdout("bun --version", "1.3.7");
        run(
            &checkout.project,
            &PackageCompiler {
                checkout: &checkout,
                stack: stack.clone(),
            },
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one"]),
        )
        .unwrap();
        assert_eq!(checkout.enabled().scripts.len(), 1);
        assert!(
            script::stale(&checkout.project, &checkout.enabled().script_paths())
                .unwrap()
                .is_empty()
        );
        assert!(script_applies(&stack)[0].contains("greeter.login"));
        assert!(!stack
            .rendered()
            .iter()
            .any(|call| call.contains("spacetime publish")));
    }

    #[test]
    fn changed_script_sources_rebuild_before_activation() {
        let checkout = Checkout::new();
        checkout
            .with_script_source()
            .with_script("greeter", 100299, "greeter.old");
        super::super::identity::write_script_identities(
            &checkout.project,
            &checkout.enabled().script_paths(),
        )
        .unwrap();
        std::fs::write(
            checkout
                .project
                .package_scripts_dir("greeter")
                .join("welcome.ts"),
            "// changed\n",
        )
        .unwrap();
        let stack = FakeStack::new().with_stdout("bun --version", "1.3.7");
        run(
            &checkout.project,
            &PackageCompiler {
                checkout: &checkout,
                stack: stack.clone(),
            },
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one"]),
        )
        .unwrap();
        assert!(script_applies(&stack)[0].contains("greeter.login"));
        assert!(!script_applies(&stack)[0].contains("greeter.old"));
    }

    #[test]
    fn current_source_built_scripts_need_neither_bun_nor_publish() {
        let checkout = Checkout::new();
        checkout
            .with_script_source()
            .with_script("greeter", 100300, "greeter.login");
        super::super::identity::write_script_identities(
            &checkout.project,
            &checkout.enabled().script_paths(),
        )
        .unwrap();
        let stack = FakeStack::new();
        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one"]),
        )
        .unwrap();
        assert_eq!(script_applies(&stack).len(), 1);
        assert!(stack.rendered().iter().all(|call| !call.starts_with("bun ")
            && !call.contains("spacetime publish")
            && !call.contains("spacetime generate")));
    }

    #[test]
    fn datascript_sources_extract_a_missing_snapshot_then_build_and_apply() {
        let checkout = Checkout::new();
        let dir = checkout.project.packages_dir().join("greeter/datascripts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spells.ts"), "// trusted Datascript\n").unwrap();
        std::fs::remove_file(checkout.project.base_snapshot_file()).unwrap();
        let stack = FakeStack::new().with_stdout("bun --version", "1.3.7");
        run(
            &checkout.project,
            &PackageCompiler {
                checkout: &checkout,
                stack: stack.clone(),
            },
            &ScriptedPrompt::new(&[]),
            &checkout.options(&["one"]),
        )
        .unwrap();
        assert!(checkout.project.base_snapshot_file().is_file());
        assert_eq!(checkout.enabled().deltas.len(), 1);
        assert_eq!(applies(&stack).len(), 1);
    }

    #[test]
    fn a_build_failure_leaves_every_shard_untouched() {
        let checkout = Checkout::new();
        checkout.with_script_source();
        let stack = FakeStack::new()
            .with_stdout("bun --version", "1.3.7")
            .fail_on("build-scripts.ts", "invalid event");
        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one"]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid event"), "{error}");
        assert!(!stack
            .rendered()
            .iter()
            .any(|call| call.starts_with("spacetime sql")
                || call.starts_with("spacetime call")
                || call.starts_with("spacetime publish")));
    }

    #[test]
    fn rust_packages_publish_and_repair_each_shard_even_without_artifacts() {
        let checkout = Checkout::new();
        checkout.with_rust();
        let stack = FakeStack::new();
        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one", "two"]),
        )
        .unwrap();
        let writes: Vec<_> = stack
            .rendered()
            .into_iter()
            .filter(|call| {
                call.starts_with("spacetime publish") || call.contains("debug_repair_after_publish")
            })
            .collect();
        assert_eq!(writes.len(), 4, "{writes:?}");
        assert!(writes[0].ends_with("one") && writes[1].contains("one debug_repair_after_publish"));
        assert!(writes[2].ends_with("two") && writes[3].contains("two debug_repair_after_publish"));
    }

    #[test]
    fn pending_teardown_publishes_after_the_last_rust_folder_was_removed() {
        let checkout = Checkout::new();
        checkout.with_module();
        let stack = FakeStack::new().with_stdout(
            "SELECT package_name FROM game_package_teardown",
            " package_name\n greeter\n",
        );
        run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one"]),
        )
        .unwrap();
        assert!(stack
            .rendered()
            .iter()
            .any(|call| call.starts_with("spacetime publish")));
    }

    #[test]
    fn check_and_declined_confirmation_never_publish_rust_packages() {
        for check in [false, true] {
            let checkout = Checkout::new();
            checkout.with_rust();
            let stack = FakeStack::new();
            let mut options = checkout.options_without_client_data(&["one"]);
            options.check = check;
            options.yes = false;
            let result = run(
                &checkout.project,
                &stack.runner(),
                &ScriptedPrompt::new(&["no"]),
                &options,
            );
            assert_eq!(result.is_ok(), check, "{result:?}");
            assert!(!stack
                .rendered()
                .iter()
                .any(|call| call.starts_with("spacetime publish")
                    || call.starts_with("spacetime call")));
        }
    }

    #[test]
    fn publish_failure_repairs_completed_shards_and_stops_before_artifacts() {
        let checkout = Checkout::new();
        checkout
            .with_rust()
            .with_script("greeter", 100300, "greeter.login");
        let stack = FakeStack::new().fail_on("--yes two", "migration refused");
        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one", "two", "three"]),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("completed: one") && message.contains("not attempted: three"),
            "{message}"
        );
        assert!(stack
            .rendered()
            .iter()
            .any(|call| call.contains("one debug_repair_after_publish")));
        assert!(script_applies(&stack).is_empty());
        assert!(
            message.contains("lyracore packages apply one two three"),
            "{message}"
        );
    }

    #[test]
    fn schedule_repair_failure_reports_the_published_shard_and_stops() {
        let checkout = Checkout::new();
        checkout.with_rust();
        let stack = FakeStack::new().fail_on("one debug_repair_after_publish", "operator only");
        let error = run(
            &checkout.project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &checkout.options_without_client_data(&["one", "two"]),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("'one' was published, but schedule repair failed"),
            "{error}"
        );
        assert!(!stack
            .rendered()
            .iter()
            .any(|call| call.starts_with("spacetime publish") && call.ends_with("two")));
    }
}
