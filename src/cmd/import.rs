//! `lyracore import [--accept] [--client-data PATH]` — build the real world.
//!
//! A fresh checkout publishes a FIXTURE: a handful of demo creatures, one quest, enough to prove
//! the stack is wired up. The real Elwynn/Westfall corridor — ~950 creature templates, ~640 quests,
//! the loot/vendor/trainer tables, the terrain heightmap and the navigation grid — is not in this
//! repository and never will be. It is RECONSTRUCTED on your machine, from two sources that are
//! yours to obtain and not ours to redistribute:
//!
//!   1. cmangos' `classic-db` world database (GPL-3.0), pulled from cmangos' own public repository
//!      at a commit this repo pins.
//!   2. YOUR 1.12.1 client's DBC/terrain/model archives, read out of the `Data/` directory of a
//!      client you already own.
//!
//! So this command asks for CONSENT before it does anything at all, and the consent text says
//! exactly that. Nothing is fetched, read or written before the operator says yes (or passes
//! `--accept`, for a scripted run).
//!
//! WHY A FAÇADE AND NOT RUST: the stages below are shell scripts under `importer/scripts/` that
//! have been run against real dumps for months, with post-ETL assertions tuned against real counts.
//! Absorbing them into Rust is worth doing later — the same path `publish` took — but doing it in
//! the same change that first makes them user-runnable would trade a tested pipeline for an untested
//! one. What this command adds is the part the scripts do NOT have: an ordered, consent-gated,
//! per-stage-diagnosable run that a first-time user can get through.
//!
//! HOW MANY STAGES: the pull, the client, then an ETL + a class-spell pass PER POPULATED DATABASE —
//! four on `dev up --single`, six on the sharded fixture, which since #108 routes dungeons to
//! `lyracore-instances` and therefore has to populate it. See [`populated_databases`].

use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::{ProjectLayout, Topology};
use crate::state::RuntimeState;
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// What `import` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportOptions {
    /// `--accept`: the consent above was given non-interactively.
    pub accept: bool,
    /// `--client-data PATH`: where the 1.12.1 client's `Data/` directory is.
    pub client_data: Option<String>,
}

/// Asking the operator something on their terminal.
///
/// A trait for the same reason the process runner is one: consent that cannot be exercised in a
/// test is consent nobody can prove is enforced.
pub trait Prompt {
    /// Ask `question` and return the answer, trimmed. `Err` means there is no terminal to ask on.
    fn ask(&self, question: &str) -> Result<String>;
}

/// The real one: prompts on the controlling terminal, not on stdin.
///
/// `/dev/tty` rather than stdin deliberately — `lyracore import < /dev/null` or a run inside a
/// pipeline must not be able to "answer" the consent question with an EOF that some readers
/// report as an empty line. No terminal means no consent, and the error says to use `--accept`.
pub struct TtyPrompt;

impl Prompt for TtyPrompt {
    fn ask(&self, question: &str) -> Result<String> {
        let tty = std::fs::File::open("/dev/tty").map_err(|_| {
            Error::Usage(
                "no terminal to ask on. Re-run attached to a terminal, or pass --accept (and \
                 --client-data PATH) to answer in advance."
                    .to_string(),
            )
        })?;
        eprint!("{question}");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        BufReader::new(tty).read_line(&mut answer)?;
        Ok(answer.trim().to_string())
    }
}

/// The consent interstitial. Printed in full, every time, before anything else happens.
pub const CONSENT: &str = "\
────────────────────────────────────────────────────────────────────────────────
 lyracore import — reconstructing the world on THIS machine

 LyraCore ships a game server. It does not ship a game world, and this command
 does not download one from us. It assembles one here, from two sources:

   1. cmangos' `classic-db` — a community-maintained vanilla world database,
      licensed GPL-3.0, pulled directly from cmangos' own public GitHub
      repository at a commit pinned in importer/scripts/classic-db.lock.
      Its CONTENT describes Blizzard Entertainment's copyrighted game world:
      creature names and stats, quest text, item tables, spawn coordinates.
      cmangos offers it as non-commercial fair-use demo content. It is not
      ours, we do not host it, and we never distribute it or anything built
      from it.

   2. YOUR OWN World of Warcraft 1.12.1 (build 5875) client. The spell, talent,
      area, faction and terrain data come out of that client's Data/ archives,
      read directly off your disk. We do not supply a client, and this command
      cannot work without one you already own.

 What this produces is a local database on your machine. Publishing, hosting or
 redistributing that database is your decision and your responsibility — it is
 not something this project does or endorses.

 Nothing has been fetched or read yet.
────────────────────────────────────────────────────────────────────────────────
";

const CONSENT_QUESTION: &str = "Proceed? Type 'yes' to continue: ";

/// The client archives the ETL hard-requires, and what dies without each.
const REQUIRED_ARCHIVES: [(&str, &str); 2] = [
    (
        "dbc.MPQ",
        "spells, talents, areas, factions and character-creation tables",
    ),
    ("terrain.MPQ", "the ground-height map"),
];

/// Archives only the navigation pass needs. Missing means a thinner nav grid, not a failed import.
const OPTIONAL_ARCHIVES: [&str; 2] = ["model.MPQ", "wmo.MPQ"];

/// Archives that exist ONLY in The Burning Crusade and later. Their presence means the path is a
/// client of the wrong expansion, which would otherwise surface as a DBC schema parse error deep
/// inside the first stage.
const POST_VANILLA_ARCHIVES: [&str; 2] = ["common.MPQ", "expansion.MPQ"];

pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    options: &ImportOptions,
) -> Result<()> {
    // An explicitly-passed --client-data is checked BEFORE the consent text, because a typo in it
    // is a usage mistake and there is no reason to make somebody read a licence notice and then a
    // half-hour clone before hearing about it. This is validation of an argument, not the
    // "locate the client" stage — that still happens in order, below.
    if let Some(raw) = &options.client_data {
        validate_client_data(Path::new(raw))?;
    }

    // ---- consent, first and always ------------------------------------------------------------
    consent(prompt, options.accept)?;

    for script in [
        project.pull_classic_db_script(),
        project.import_world_script(),
        project.import_class_spells_script(),
    ] {
        if !script.exists() {
            return Err(Error::PrerequisiteMissing(format!(
                "{} is missing from this checkout — `lyracore import` drives the scripts under {}/",
                script.display(),
                ProjectLayout::IMPORTER_SCRIPTS_DIR
            )));
        }
    }

    // Which databases this import POPULATES, and therefore how many stages there are. Not
    // `world_shards()`: `lyracore-kalimdor` is in that list and is deliberately not here, because
    // map 1 has no content in the dump slice this ETL imports — importing it would be a long run
    // that lands nothing.
    let databases = populated_databases(RuntimeState::load(&project.state_file())?.topology());
    // Two fixed stages, then an ETL + a class-spell pass per database.
    let total = 2 + 2 * databases.len() as u8;

    // ---- stage 1: the world database dump -----------------------------------------------------
    stage(
        1,
        total,
        "pulling cmangos/classic-db (pinned commit, checksum-verified)",
    );
    runner.run_streaming(&pull_command(project)).map_err(|e| {
        stage_failure(
            "pulling classic-db",
            "Check network access to github.com. To deliberately track upstream master instead of \
             the pinned commit, run it yourself:\n    \
             CLASSIC_DB_REF=master bash importer/scripts/pull-classic-db.sh --skip-verify\n  A \
             CHECKSUM MISMATCH is not a network problem: it means the pinned commit no longer \
             produces the dump this repo recorded. Do not import that file until you know why.",
            e,
        )
    })?;

    // ---- stage 2: the client ------------------------------------------------------------------
    stage(2, total, "locating your 1.12.1 client data");
    let client_data = resolve_client_data(project, prompt, options)?;
    println!("   client data: {}", client_data.display());

    // ---- one ETL + one class-spell pass per database, INTERLEAVED -----------------------------
    // Interleaved rather than all-ETLs-then-all-spells so a failure part way leaves one COMPLETE
    // database rather than two half ones — and because ETL-then-spells is the order the scripts
    // have been run in against real dumps.
    for (i, database) in databases.iter().enumerate() {
        let n = 3 + 2 * i as u8;
        stage(
            n,
            total,
            &format!(
                "importing the world into {database} (creatures, quests, loot, vendors, terrain, \
                 navigation)"
            ),
        );
        if i == 0 {
            println!(
                "   this is the long one — tens of minutes, and it prints its own assertions at \
                 the end."
            );
        } else {
            // Why a second full run and not a map-36 slice: the ETL has no map-36-only mode, and
            // adding one means teaching the terrain/nav assertion floors, the post-run one-continent
            // re-assert AND `db_never_imported`'s fresh-shard signal to stand down — four ways to
            // pass an import that imported nothing, to save wall time on a command that already
            // asks for consent. The duplicated map-0 rows are inert: routing never reads them, and
            // the creature tick's movement/AI passes are seeded from PLAYERS
            // (`module/src/creatures/tick/mod.rs`, `active_cell_creatures`), so a playerless second
            // copy of Elwynn costs one table scan per tick, not a second simulation.
            println!(
                "   the instance pool needs its OWN map-36 spawn rows — it is the database that \
                 spawns\n   a Deadmines run's population, and nothing reports an empty instance at \
                 runtime.\n   As long again as the pass above; the duplicated map-0 rows are never \
                 routed to."
            );
        }
        runner
            .run_streaming(&world_command(project, &client_data, database))
            .map_err(|e| stage_failure(
                &format!("importing the world into {database}"),
                "The ETL prints a FAIL line for every assertion that did not meet its floor; read those \
                 first — they say which family imported nothing. If it never got that far, check that \
                 the SpacetimeDB node is running and this database is published (`lyracore dev up`).",
                e,
            ))?;

        // The world ETL runs this too, but swallows its exit status inside a reporting pipeline. Run
        // it again as a stage of its own: it is idempotent (a surgical per-id delete+reload), and it
        // is the difference between "no class can train past its starter kit" failing loudly here and
        // failing silently in play.
        stage(
            n + 1,
            total,
            &format!("importing the curated class spells into {database} (all nine classes)"),
        );
        runner
            .run_streaming(&class_spells_command(project, &client_data, database))
            .map_err(|e| {
                stage_failure(
                    &format!("importing class spells into {database}"),
                    "This reads Spell.dbc out of the client archives and writes trainer offerings. A \
                 \"requested N ids but matched M\" warning names ids your client build does not have.",
                    e,
                )
            })?;
    }

    println!();
    println!(
        "import complete. The realm now holds the imported world rather than the seed fixture."
    );
    println!("Nothing produced here is redistributable — see the notice this command opened with.");
    Ok(())
}

/// The databases `import` populates, in the order it populates them.
///
/// The default world shard always, and the instance pool whenever the fixture routes dungeons there
/// (#108) — because that database is the one that spawns a dungeon run's population, and a realm
/// where it was skipped reports NOTHING: the entry is routed correctly and the instance comes up
/// empty. `lyracore-kalimdor` is deliberately absent: map 1 has no content in this dump slice.
fn populated_databases(topology: Topology) -> Vec<&'static str> {
    match topology {
        Topology::Single => vec![ProjectLayout::DATABASE],
        Topology::Sharded => vec![ProjectLayout::DATABASE, ProjectLayout::INSTANCE_POOL],
    }
}

/// Print the notice and require an affirmative answer. `--accept` answers it in advance; nothing
/// else does.
fn consent(prompt: &dyn Prompt, accept: bool) -> Result<()> {
    print!("{CONSENT}");
    let _ = std::io::stdout().flush();
    if accept {
        println!("Consent given on the command line (--accept).");
        return Ok(());
    }
    let answer = prompt.ask(CONSENT_QUESTION)?;
    if !answer.eq_ignore_ascii_case("yes") {
        return Err(Error::Usage(format!(
            "not proceeding: the answer was {answer:?}, and only 'yes' is consent. Nothing was \
             fetched, read or changed."
        )));
    }
    Ok(())
}

/// Stage 2: the flag if there is one, otherwise the persisted config, otherwise ask — and save a
/// freshly prompted-for answer so the NEXT run does not have to ask again.
///
/// The chain, in order: `--client-data` wins outright (a typo in it is a usage mistake `run`
/// already refused before consent was spent); the `config.json` value is used if it still
/// validates, and reported-then-abandoned if it does not (a stale config must not turn into a
/// silent prompt-less failure); the interactive prompt is the fallback of last resort, and its
/// answer is written back to `config.json` once it validates.
fn resolve_client_data(
    project: &ProjectLayout,
    prompt: &dyn Prompt,
    options: &ImportOptions,
) -> Result<PathBuf> {
    if let Some(raw) = &options.client_data {
        let path = Path::new(raw);
        validate_client_data(path)?;
        // Canonicalize AFTER validating, so the diagnostics above quote what the operator typed.
        return Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    let config_path = project.config_file();
    let mut config = crate::config::Config::load(&config_path)?;
    if let Some(raw) = config.client_data.clone() {
        let path = Path::new(&raw);
        match inspect_client_data(path) {
            Ok(notes) => {
                for note in notes {
                    println!("{note}");
                }
                println!("   client data (from {}): {}", config_path.display(), raw);
                return Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
            }
            Err(e) => println!(
                "   the configured client data ({raw}) is no longer valid: {e}\n   (re-set it \
                 with `lyracore config set client-data <path>`) — asking instead."
            ),
        }
    }

    println!("   Where is your 1.12.1 client's Data/ directory?");
    println!("   (the one containing dbc.MPQ and terrain.MPQ — e.g. /games/WoW-1.12.1/Data)");
    let answer = prompt.ask("   Path: ")?;
    if answer.is_empty() {
        return Err(Error::Usage(
            "no client data path given. LyraCore does not supply a client; point \
             --client-data at the Data/ directory of a 1.12.1 install you own."
                .to_string(),
        ));
    }
    let path = Path::new(&answer);
    validate_client_data(path)?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    config.client_data = Some(canonical.to_string_lossy().to_string());
    config.save(&config_path)?;
    println!("   saved to {} — won't ask again.", config_path.display());
    Ok(canonical)
}

/// Does this look like the `Data/` directory of a 1.12.1 client? The printing wrapper around
/// [`inspect_client_data`] — same checks, same error strings, but the optional-archive notes go
/// straight to stdout instead of coming back as data.
fn validate_client_data(path: &Path) -> Result<()> {
    for note in inspect_client_data(path)? {
        println!("{note}");
    }
    Ok(())
}

/// The pure form of [`validate_client_data`]: does this look like the `Data/` directory of a
/// 1.12.1 client, and if so, what should the operator be told about it?
///
/// Cheap checks only, but the three that matter: it exists, it is the Data directory rather than
/// the install directory ABOVE it (by far the most common mistake), and it is vanilla rather than
/// a later expansion (whose merged archives would otherwise fail as an inscrutable DBC parse error
/// several minutes in). `Ok` carries the optional-archive notes rather than printing them, so
/// `lyracore doctor` can ask the same question without a `check_client_data` that talks to a
/// terminal.
pub fn inspect_client_data(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Err(Error::Usage(format!(
            "no such directory: {}. Point --client-data at your 1.12.1 client's Data/ directory.",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(Error::Usage(format!(
            "{} is not a directory. --client-data wants the client's Data/ directory itself, not a \
             file inside it.",
            path.display()
        )));
    }

    let missing: Vec<&str> = REQUIRED_ARCHIVES
        .iter()
        .filter(|(name, _)| !path.join(name).exists())
        .map(|(name, _)| *name)
        .collect();

    if !missing.is_empty() {
        // The install directory, one level up from Data/, is the usual near-miss. Say so rather
        // than making the operator guess what "does not look like" means.
        let nested = path.join("Data");
        if REQUIRED_ARCHIVES
            .iter()
            .all(|(name, _)| nested.join(name).exists())
        {
            return Err(Error::Usage(format!(
                "{} looks like the client's INSTALL directory, not its Data/ directory. Use:\n  \
                 --client-data {}",
                path.display(),
                nested.display()
            )));
        }
        let detail: Vec<String> = REQUIRED_ARCHIVES
            .iter()
            .filter(|(name, _)| missing.contains(name))
            .map(|(name, needed_for)| format!("{name} ({needed_for})"))
            .collect();
        return Err(Error::Usage(format!(
            "{} does not look like a 1.12.1 client Data/ directory — missing {}. A 1.12.1 (build \
             5875) install has dbc.MPQ, terrain.MPQ, model.MPQ and wmo.MPQ side by side in Data/.",
            path.display(),
            detail.join(", ")
        )));
    }

    if let Some(found) = POST_VANILLA_ARCHIVES
        .iter()
        .find(|name| path.join(name).exists())
    {
        return Err(Error::Usage(format!(
            "{} contains {found}, which only exists in The Burning Crusade and later. LyraCore \
             speaks the 1.12.1 (build 5875) protocol and reads 1.12.1 DBC schemas; a later \
             client's tables would fail to parse partway through the import.",
            path.display()
        )));
    }

    let mut notes = Vec::new();
    for name in OPTIONAL_ARCHIVES {
        if !path.join(name).exists() {
            notes.push(format!(
                "   NOTE: no {name} in {} — the navigation/line-of-sight grid will be built from \
                 whatever geometry is available, which may be less than the full world.",
                path.display()
            ));
        }
    }
    Ok(notes)
}

// ---- the script invocations -----------------------------------------------------------------

/// Every stage runs from the CHECKOUT ROOT: the scripts resolve `.import/`, `target/debug/…` and
/// each other relative to it, and `lyracore` is usable from any subdirectory of a checkout.
fn from_root(project: &ProjectLayout, script: PathBuf) -> CommandSpec {
    CommandSpec::new("bash")
        .arg(script.to_string_lossy().to_string())
        .cwd(project.root.clone())
}

fn pull_command(project: &ProjectLayout) -> CommandSpec {
    from_root(project, project.pull_classic_db_script())
}

/// The world ETL reads its target database and client path out of the environment; the canonical
/// map-0 box and every other default is the script's own and is deliberately not overridden here.
///
/// That includes `INCLUDE_MAPS=36`, which is why the instance-pool pass is this same command with
/// `DB` changed and nothing else: the pool wants exactly the default map set, not a narrower one.
fn world_command(project: &ProjectLayout, client_data: &Path, database: &str) -> CommandSpec {
    from_root(project, project.import_world_script())
        .env("DB", database)
        .env("DBC", client_data.to_string_lossy().to_string())
}

/// The class-spell import takes the client path as its one argument and the database from `DB` —
/// pass BOTH explicitly, because its `DB` default is the fixture database and a silent default is
/// how a shard once had its spells written to a different database entirely.
fn class_spells_command(
    project: &ProjectLayout,
    client_data: &Path,
    database: &str,
) -> CommandSpec {
    from_root(project, project.import_class_spells_script())
        .arg(client_data.to_string_lossy().to_string())
        .env("DB", database)
}

fn stage(number: u8, total: u8, what: &str) {
    println!();
    println!("==> [{number}/{total}] {what}");
}

/// Wrap a stage failure with what to do about it, keeping the child's own diagnosis.
fn stage_failure(what: &str, advice: &str, cause: Error) -> Error {
    Error::Process(format!("{what} failed.\n  {advice}\n  ({cause})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeStack};
    use std::cell::RefCell;
    use tempfile::TempDir;

    /// A prompt with pre-recorded answers, which also records what it was asked.
    struct ScriptedPrompt {
        answers: RefCell<Vec<String>>,
        asked: RefCell<Vec<String>>,
    }

    impl ScriptedPrompt {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: RefCell::new(answers.iter().rev().map(|a| a.to_string()).collect()),
                asked: RefCell::new(Vec::new()),
            }
        }
        fn asked(&self) -> Vec<String> {
            self.asked.borrow().clone()
        }
    }

    impl Prompt for ScriptedPrompt {
        fn ask(&self, question: &str) -> Result<String> {
            self.asked.borrow_mut().push(question.to_string());
            // Trimmed, because the trait says answers are — a fake that skipped that would let
            // `consent` pass a test it fails against a real terminal's trailing newline.
            self.answers
                .borrow_mut()
                .pop()
                .map(|a| a.trim().to_string())
                .ok_or_else(|| Error::Usage("no terminal".to_string()))
        }
    }

    /// A checkout with the four import scripts present.
    fn checkout(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let dir = tmp.path().join(ProjectLayout::IMPORTER_SCRIPTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "pull-classic-db.sh",
            "import-world.sh",
            "import-class-spells.sh",
        ] {
            std::fs::write(dir.join(name), "#!/usr/bin/env bash\n").unwrap();
        }
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    /// A directory that passes for a 1.12.1 client's Data/.
    fn client_data(tmp: &TempDir) -> PathBuf {
        let data = tmp.path().join("wow/Data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["dbc.MPQ", "terrain.MPQ", "model.MPQ", "wmo.MPQ"] {
            std::fs::write(data.join(name), "").unwrap();
        }
        data
    }

    fn accepted(path: &Path) -> ImportOptions {
        ImportOptions {
            accept: true,
            client_data: Some(path.to_string_lossy().to_string()),
        }
    }

    // ---- consent ----

    #[test]
    fn without_yes_or_accept_nothing_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);

        for answer in ["", "no", "n", "y", "sure", "YES please", "1"] {
            let stack = FakeStack::new();
            let prompt = ScriptedPrompt::new(&[answer]);
            let options = ImportOptions {
                accept: false,
                client_data: Some(data.to_string_lossy().to_string()),
            };
            let error = run(&project, &stack.runner(), &prompt, &options).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{answer:?}");
            assert!(
                stack.calls().is_empty(),
                "answering {answer:?} ran something: {:?}",
                stack.calls()
            );
        }
    }

    #[test]
    fn a_typed_yes_is_consent_and_case_does_not_matter() {
        for answer in ["yes", "YES", "Yes", " yes "] {
            let tmp = TempDir::new().unwrap();
            let project = checkout(&tmp);
            let data = client_data(&tmp);
            let stack = FakeStack::new();
            let prompt = ScriptedPrompt::new(&[answer]);
            let options = ImportOptions {
                accept: false,
                client_data: Some(data.to_string_lossy().to_string()),
            };
            run(&project, &stack.runner(), &prompt, &options).unwrap();
            assert_eq!(
                stack.calls().len(),
                expected_calls(Topology::Sharded),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn accept_answers_the_question_so_no_terminal_is_needed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        // No answers at all: this prompt errors if it is asked anything.
        let prompt = ScriptedPrompt::new(&[]);

        run(&project, &stack.runner(), &prompt, &accepted(&data)).unwrap();
        assert!(prompt.asked().is_empty(), "--accept must not prompt");
    }

    fn set_topology(project: &ProjectLayout, topology: Topology) {
        crate::state::RuntimeState {
            topology: topology.as_str().to_string(),
            ..Default::default()
        }
        .save(&project.state_file())
        .unwrap();
    }

    /// How many child processes a complete run spawns: the pull, then an ETL + a class-spell pass
    /// per populated database. Derived, so growing the fixture never turns these into a hunt for
    /// hardcoded 3s. A checkout with no `state.json` reads back as the DEFAULT topology.
    fn expected_calls(topology: Topology) -> usize {
        1 + 2 * populated_databases(topology).len()
    }

    /// Every `(script name, DB=)` pair the run handed out, in order.
    fn imported_databases(stack: &FakeStack) -> Vec<(String, String)> {
        stack
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Stream(spec) | Call::Wait(spec) => {
                    let script = spec.args().first()?.rsplit('/').next()?.to_string();
                    Some((script, spec.env_value("DB")?.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_sharded_fixture_imports_the_instance_pool_too_and_single_does_not() {
        // THE reason this loop exists. The sharded fixture routes map 36 to `lyracore-instances`,
        // and that database — not the world shard — is what spawns a dungeon run's population. Skip
        // it and a Deadmines entry is routed perfectly and comes up EMPTY, with no error on either
        // side and nothing at runtime that reports it. There is no loud failure to fall back on, so
        // the import has to be the thing that covers it.
        for (topology, want) in [
            (Topology::Single, vec![ProjectLayout::DATABASE]),
            (
                Topology::Sharded,
                vec![ProjectLayout::DATABASE, ProjectLayout::INSTANCE_POOL],
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let project = checkout(&tmp);
            let data = client_data(&tmp);
            set_topology(&project, topology);
            let stack = FakeStack::new();

            run(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap();

            let world: Vec<String> = imported_databases(&stack)
                .into_iter()
                .filter(|(script, _)| script == "import-world.sh")
                .map(|(_, db)| db)
                .collect();
            assert_eq!(world, want, "{topology:?}");
            // ...and the class-spell pass follows each ETL onto the SAME database. Its `DB` default
            // is the fixture database, so an unset one writes a shard's spells into `lyracore`
            // silently — the failure that made passing it explicitly a rule in the first place.
            let spells: Vec<String> = imported_databases(&stack)
                .into_iter()
                .filter(|(script, _)| script == "import-class-spells.sh")
                .map(|(_, db)| db)
                .collect();
            assert_eq!(spells, want, "{topology:?}");
        }
    }

    #[test]
    fn the_instance_pool_etl_is_the_default_one_with_only_the_database_changed() {
        // It rides `import-world.sh`'s own defaults — crucially `INCLUDE_MAPS=36`, which is what
        // puts the Deadmines interior on the pool. A narrowed box or map set here would be this CLI
        // second-guessing an ETL whose assertion floors are tuned to those defaults.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let default = world_command(&project, &data, ProjectLayout::DATABASE);
        let pool = world_command(&project, &data, ProjectLayout::INSTANCE_POOL);
        assert_eq!(pool.env_value("DBC"), default.env_value("DBC"));
        assert_eq!(pool.env_value("DB"), Some(ProjectLayout::INSTANCE_POOL));
        assert_eq!(
            pool.render()
                .replace(ProjectLayout::INSTANCE_POOL, ProjectLayout::DATABASE),
            default.render(),
            "the two runs must differ in DB and nothing else"
        );
    }

    #[test]
    fn the_notice_names_the_source_its_licence_and_who_supplies_the_client() {
        for needle in [
            "classic-db",
            "cmangos",
            "GPL-3.0",
            "Blizzard",
            "copyrighted",
            "1.12.1",
            "never distribute",
        ] {
            assert!(CONSENT.contains(needle), "the notice must say {needle:?}");
        }
    }

    #[test]
    fn a_refusal_reports_that_nothing_happened() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["no"]);
        let options = ImportOptions {
            accept: false,
            client_data: Some(data.to_string_lossy().to_string()),
        };
        let error = run(&project, &stack.runner(), &prompt, &options)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Nothing was fetched"), "{error}");
    }

    #[test]
    fn a_missing_terminal_is_a_refusal_not_a_default_yes() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&[]); // asking errors, like a headless run
        let options = ImportOptions {
            accept: false,
            client_data: Some(data.to_string_lossy().to_string()),
        };
        assert!(run(&project, &stack.runner(), &prompt, &options).is_err());
        assert!(stack.calls().is_empty());
    }

    // ---- stage order and argv shapes ----

    fn rendered(stack: &FakeStack) -> Vec<String> {
        stack.rendered()
    }

    #[test]
    fn the_stages_run_in_order_pull_then_world_then_spells_per_database() {
        // Per database and INTERLEAVED: world then spells, world then spells. Not all-ETLs-then-all-
        // spells — a failure part way must leave one COMPLETE database rather than two half ones,
        // and ETL-then-spells is the order these scripts have been run in against real dumps.
        for topology in [Topology::Single, Topology::Sharded] {
            let tmp = TempDir::new().unwrap();
            let project = checkout(&tmp);
            let data = client_data(&tmp);
            set_topology(&project, topology);
            let stack = FakeStack::new();

            run(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap();

            let calls = rendered(&stack);
            assert_eq!(calls.len(), expected_calls(topology), "{calls:?}");
            assert!(calls[0].contains("pull-classic-db.sh"), "{calls:?}");
            for pair in calls[1..].chunks(2) {
                assert!(pair[0].contains("import-world.sh"), "{calls:?}");
                assert!(pair[1].contains("import-class-spells.sh"), "{calls:?}");
            }
            // Every one of them streams: these are long, chatty children whose progress is the point.
            assert!(
                stack.calls().iter().all(|c| matches!(c, Call::Stream(_))),
                "{:?}",
                stack.calls()
            );
        }
    }

    #[test]
    fn every_stage_runs_from_the_checkout_root_whatever_the_working_directory_is() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        for call in stack.calls() {
            let Call::Stream(spec) = call else {
                panic!("expected streams only")
            };
            assert_eq!(
                spec.cwd_value(),
                Some(project.root.as_path()),
                "{}",
                spec.render()
            );
        }
    }

    #[test]
    fn the_scripts_are_addressed_by_their_shipped_paths_and_run_through_bash() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        for (call, expected) in stack.calls().iter().zip([
            ProjectLayout::PULL_CLASSIC_DB_SCRIPT,
            ProjectLayout::IMPORT_WORLD_SCRIPT,
            ProjectLayout::IMPORT_CLASS_SPELLS_SCRIPT,
        ]) {
            let Call::Stream(spec) = call else {
                panic!("expected streams only")
            };
            assert_eq!(spec.program(), "bash");
            assert!(
                spec.args()[0].ends_with(expected),
                "{} should be {expected}",
                spec.args()[0]
            );
        }
    }

    #[test]
    fn the_client_path_and_the_database_reach_both_importing_stages() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let canonical = std::fs::canonicalize(&data).unwrap();
        let canonical = canonical.to_string_lossy().to_string();

        let specs: Vec<CommandSpec> = stack
            .calls()
            .into_iter()
            .map(|c| match c {
                Call::Stream(spec) => spec,
                other => panic!("expected streams only, got {other:?}"),
            })
            .collect();

        // The world ETL takes both out of the environment.
        assert_eq!(specs[1].env_value("DBC"), Some(canonical.as_str()));
        assert_eq!(
            specs[1].env_value("DB"),
            Some(ProjectLayout::DATABASE),
            "an unset DB silently writes to the default database"
        );
        // The class-spell import takes the client path as its argument and the database from env.
        assert_eq!(
            specs[2].args().last().map(String::as_str),
            Some(canonical.as_str())
        );
        assert_eq!(specs[2].env_value("DB"), Some(ProjectLayout::DATABASE));
        // The pull needs neither: it fetches a dump, it does not read the client or the database.
        assert_eq!(specs[0].env_value("DBC"), None);
    }

    #[test]
    fn the_pull_is_not_told_to_skip_its_own_checksum_verification() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        assert!(
            !rendered(&stack)[0].contains("--skip-verify"),
            "the pinned-commit checksum is the drift guard; the façade must not disable it"
        );
    }

    // ---- the client path ----

    #[test]
    fn without_the_flag_the_path_is_asked_for_after_consent_and_after_the_pull() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);

        run(
            &project,
            &stack.runner(),
            &prompt,
            &ImportOptions::default(),
        )
        .unwrap();

        let asked = prompt.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].contains("Proceed"), "{asked:?}");
        assert!(asked[1].contains("Path"), "{asked:?}");
        assert_eq!(rendered(&stack).len(), expected_calls(Topology::Sharded));
    }

    #[test]
    fn a_bad_client_path_is_refused_before_the_licence_notice_is_even_answered() {
        // A typo in --client-data must not cost somebody a half-hour clone first.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();
        let options = ImportOptions {
            accept: true,
            client_data: Some(tmp.path().join("nope").to_string_lossy().to_string()),
        };
        let error = run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &options,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.calls().is_empty());
    }

    #[test]
    fn the_install_directory_one_level_up_is_named_as_the_fix() {
        let tmp = TempDir::new().unwrap();
        let data = client_data(&tmp);
        let install = data.parent().unwrap();

        let error = validate_client_data(install).unwrap_err().to_string();
        assert!(error.contains("INSTALL directory"), "{error}");
        assert!(
            error.contains(&data.to_string_lossy().to_string()),
            "{error}"
        );
    }

    #[test]
    fn a_directory_without_the_archives_says_which_ones_and_what_they_are_for() {
        let tmp = TempDir::new().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = validate_client_data(&empty).unwrap_err().to_string();
        assert!(error.contains("dbc.MPQ"), "{error}");
        assert!(error.contains("terrain.MPQ"), "{error}");
        assert!(error.contains("5875"), "{error}");
    }

    #[test]
    fn a_later_expansions_client_is_rejected_by_name() {
        let tmp = TempDir::new().unwrap();
        let data = client_data(&tmp);
        std::fs::write(data.join("common.MPQ"), "").unwrap();
        let error = validate_client_data(&data).unwrap_err().to_string();
        assert!(error.contains("common.MPQ"), "{error}");
        assert!(error.contains("Burning Crusade"), "{error}");
    }

    #[test]
    fn a_file_is_not_a_data_directory() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("dbc.MPQ");
        std::fs::write(&file, "").unwrap();
        assert!(validate_client_data(&file).is_err());
    }

    #[test]
    fn missing_nav_archives_are_a_note_not_a_refusal() {
        let tmp = TempDir::new().unwrap();
        let data = tmp.path().join("Data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["dbc.MPQ", "terrain.MPQ"] {
            std::fs::write(data.join(name), "").unwrap();
        }
        validate_client_data(&data).unwrap();
    }

    #[test]
    fn an_empty_answer_to_the_path_prompt_is_a_usage_error_naming_the_flag() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["yes", ""]);
        let error = run(
            &project,
            &stack.runner(),
            &prompt,
            &ImportOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--client-data"), "{error}");
    }

    // ---- the config fallback chain ----

    #[test]
    fn a_client_data_flag_wins_over_a_configured_path_and_leaves_it_untouched() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let flagged = client_data(&tmp);
        let configured = tmp.path().join("configured-elsewhere");
        crate::config::Config {
            client_data: Some(configured.to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();

        let stack = FakeStack::new();
        run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&flagged),
        )
        .unwrap();

        // A flag never even reads config.json, let alone overwrites it.
        let config = crate::config::Config::load(&project.config_file()).unwrap();
        assert_eq!(
            config.client_data.as_deref(),
            Some(configured.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_configured_path_is_used_without_prompting_when_no_flag_is_given() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        crate::config::Config {
            client_data: Some(data.to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();

        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["yes"]); // asking for a path would error: no 2nd answer
        run(
            &project,
            &stack.runner(),
            &prompt,
            &ImportOptions::default(),
        )
        .unwrap();

        assert_eq!(
            prompt.asked().len(),
            1,
            "a valid configured path must not be prompted for: {:?}",
            prompt.asked()
        );
    }

    #[test]
    fn a_prompted_path_is_persisted_so_the_next_run_does_not_ask() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);

        run(
            &project,
            &stack.runner(),
            &prompt,
            &ImportOptions::default(),
        )
        .unwrap();

        let canonical = std::fs::canonicalize(&data).unwrap();
        let config = crate::config::Config::load(&project.config_file()).unwrap();
        assert_eq!(
            config.client_data.as_deref(),
            Some(canonical.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn an_invalid_configured_path_reports_why_and_falls_back_to_the_prompt() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        crate::config::Config {
            client_data: Some(tmp.path().join("nope").to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();

        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);
        run(
            &project,
            &stack.runner(),
            &prompt,
            &ImportOptions::default(),
        )
        .unwrap();

        assert_eq!(
            prompt.asked().len(),
            2,
            "an invalid configured path must still fall back to asking: {:?}",
            prompt.asked()
        );
        // ...and the good answer replaces the bad one, so the NEXT run does not hit this again.
        let canonical = std::fs::canonicalize(&data).unwrap();
        let config = crate::config::Config::load(&project.config_file()).unwrap();
        assert_eq!(
            config.client_data.as_deref(),
            Some(canonical.to_string_lossy().as_ref())
        );
    }

    // ---- failure surfacing ----

    #[test]
    fn each_stages_failure_names_the_stage_keeps_the_childs_words_and_stops_the_run() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);

        for (needle, child_says, stage_name, stages_before) in [
            (
                "pull-classic-db.sh",
                "sha256 MISMATCH",
                "pulling classic-db",
                0,
            ),
            (
                "import-world.sh",
                "FAIL terrain chunks",
                "importing the world",
                1,
            ),
            (
                "import-class-spells.sh",
                "requested 31 ids but matched 30",
                "importing class spells",
                2,
            ),
        ] {
            let stack = FakeStack::new().fail_on(needle, child_says);
            let error = run(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap_err()
            .to_string();

            assert!(error.contains(stage_name), "{error}");
            assert!(error.contains(child_says), "{error}");
            assert_eq!(
                stack.calls().len(),
                stages_before + 1,
                "a failed stage must stop the run: {error}"
            );
        }
    }

    #[test]
    fn a_checkout_without_the_import_scripts_says_so_before_consent_is_spent() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        let error = run(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(ProjectLayout::IMPORTER_SCRIPTS_DIR),
            "{error}"
        );
        assert!(stack.calls().is_empty());
    }
}
