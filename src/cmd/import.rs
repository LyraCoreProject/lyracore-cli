//! `lyracore import [world|vmaps]` — build the real world.
//!
//! A fresh checkout publishes a FIXTURE: a handful of demo creatures, one quest, enough to prove
//! the stack is wired up. The Alliance early-game corridors, their loot/vendor/trainer tables,
//! terrain heightmaps and navigation grids are not in this repository and never will be. They are
//! RECONSTRUCTED on your machine, from two sources that are yours to obtain and not ours to
//! redistribute:
//!
//!   1. cmangos' `classic-db` world database (GPL-3.0), pulled from cmangos' own public repository
//!      at a commit this repo pins.
//!   2. YOUR 1.12.1 client's DBC/terrain/model archives, read out of the `Data/` directory of a
//!      client you already own.
//!
//! So `import world` asks for CONSENT before it does anything at all, and the consent text says
//! exactly that. Nothing is fetched, read or written before the operator says yes (or passes
//! `--accept`, for a scripted run).
//!
//! TWO VERBS, ONE BOUNDARY (#104; the operator decision is CLI = orchestration, core = machinery):
//!
//! - `import world` (bare `import` is the same command) is the Rust port of the orchestration that
//!   used to live in `importer/scripts/import-world.sh`: the DUMP/DBC staging, the importer-binary
//!   mode ordering, and the FLOOR_* manifest assertions. Every mode still SHELLS OUT to the pinned
//!   `lyracore-importer` binary — this CLI gains no MPQ/DBC parsing and no module-schema crate
//!   coupling; the queries below are the same textual `spacetime sql` probes the bash ran.
//!   `import-world.sh` itself is NOT retired: it remains the by-hand advanced path (several
//!   shards, a non-default box, a second continent) until this flow has proven parity on a fresh
//!   provision, and this command deliberately does not run it.
//! - `import vmaps` drives the importer's `--vmap` mode (exact model/WMO collision triangles →
//!   `game_vmap_chunk`) per world shard. It reads only YOUR client's archives — nothing is fetched
//!   from the network — so it carries no consent gate.
//!
//! What still shells to bash, on purpose: `pull-classic-db.sh` (the pinned, checksum-verified dump
//! fetch — its lockfile discipline lives with the lock it reads) and `import-class-spells.sh` (the
//! curated overlay whose spell-id lists are core-repo data). `import-manifest.sh` is sourced, not
//! run: it is a data file of floors the core repo tunes per dump, read fresh on every import so a
//! retuned floor never needs a CLI release.
//!
//! HOW MANY STAGES: the pull, the client, the importer build, then the modes + the re-arm + the
//! floors per content destination: six on `dev up --single`, twelve on the sharded fixture. See
//! [`import_destinations`].

use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::{ProjectLayout, Topology};
use crate::state::RuntimeState;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// What `import world` was asked to do.
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

/// Archives only the navigation/collision passes need. Missing means a thinner grid, not a failed
/// import.
const OPTIONAL_ARCHIVES: [&str; 2] = ["model.MPQ", "wmo.MPQ"];

/// Archives that exist ONLY in The Burning Crusade and later. Their presence means the path is a
/// client of the wrong expansion, which would otherwise surface as a DBC schema parse error deep
/// inside the first stage.
const POST_VANILLA_ARCHIVES: [&str; 2] = ["common.MPQ", "expansion.MPQ"];

/// Where `pull-classic-db.sh` assembles the dump — its documented output, and `--dump`'s input.
/// Relative on purpose: every child runs from the checkout root, exactly like the bash flow.
const DUMP_PATH: &str = ".import/classic-db-full.sql";
/// Out-of-box quest givers force-imported by entry — 10 NPCs whose spawns sit outside the box but
/// whose quest chains complete inside it.
const INCLUDE_CREATURES: &str = "344,11406,266,415,1343,6966,5165,6166,6569,5149";
/// The caster-mob `--only` allowlist: spell ids the in-box Elwynn/Westfall casters reference,
/// imported additively so the curated class kit survives. Extending it (the Defias casters) is
/// dump-verification work that lives with the bash flow's EXTEND-HERE block.
const CASTER_SPELL_IDS: &str = "53,133,143,1776,145,744,745,3149,3150,3238,3248,5416,5708,6016,\
                                6268,6524,6660,6730,7159,7357,8014,8260,8646,8873,9080,10101,\
                                10277,12023,12024,12170,12544,13322,13342,13375,13443,14030,\
                                15572,15652,15657,15661,16144,16244,20712,20714,20720,20746,\
                                20793,20808,23114,23260,23504,28265";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorldProfile {
    AllianceEastern,
    AllianceKalimdor,
    AllianceSingle,
    Instances,
}

impl WorldProfile {
    fn name(self) -> &'static str {
        match self {
            Self::AllianceEastern => "alliance-eastern",
            Self::AllianceKalimdor => "alliance-kalimdor",
            Self::AllianceSingle => "alliance-single",
            Self::Instances => "instances",
        }
    }

    fn has_bounded_slices(self) -> bool {
        !matches!(self, Self::Instances)
    }

    fn includes_eastern_corridors(self) -> bool {
        matches!(self, Self::AllianceEastern | Self::AllianceSingle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImportDestination {
    shard: &'static str,
    profile: WorldProfile,
}

// =============================================================================================
//  `import world`
// =============================================================================================

pub fn run_world(
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
        project.import_class_spells_script(),
        project.import_manifest_script(),
    ] {
        if !script.exists() {
            return Err(Error::PrerequisiteMissing(format!(
                "{} is missing from this checkout — `lyracore import` drives the tooling under \
                 {}/",
                script.display(),
                ProjectLayout::IMPORTER_SCRIPTS_DIR
            )));
        }
    }

    // The floors come FIRST, before anything expensive: a manifest that has drifted away from the
    // checks below must fail here, not after the half-hour clone and the tens-of-minutes ETL —
    // and never as an assertion that silently stopped existing.
    let floors = load_floors(project, runner)?;
    floors.require(&CONSUMED_FLOORS)?;
    println!(
        "   floors: {} minimums from {}",
        CONSUMED_FLOORS.len(),
        ProjectLayout::IMPORT_MANIFEST_SCRIPT
    );

    let destinations = import_destinations(RuntimeState::load(&project.state_file())?.topology());
    // Three fixed stages, then the modes + the re-arm + the floors per destination.
    let total = 3 + 3 * destinations.len() as u8;

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
    // The DUMP staging assertion: the pull's documented output is the ETL's input, and "the pull
    // said OK but wrote somewhere else" must fail here, not as an importer error mid-stage-4.
    let dump = project.root.join(DUMP_PATH);
    if !dump.exists() {
        return Err(Error::Process(format!(
            "the classic-db pull reported success but left no dump at {} — that path is \
             pull-classic-db.sh's documented output and the world ETL's input. Re-run the script \
             by hand and see which path it printed.",
            dump.display()
        )));
    }

    // ---- stage 2: the client ------------------------------------------------------------------
    stage(2, total, "locating your 1.12.1 client data");
    let client_data = resolve_client_data(project, prompt, options.client_data.as_deref())?;
    println!("   client data: {}", client_data.display());

    // ---- stage 3: the importer binary ---------------------------------------------------------
    stage(3, total, "building the importer (cargo build --bin lyracore-importer)");
    runner
        .run_and_wait(&build_importer_command(project))
        .map_err(|e| {
            stage_failure(
                "building the importer",
                "The importer builds with the checkout's own pinned toolchain — `lyracore doctor` \
                 checks the prerequisites.",
                e,
            )
        })?;

    // ---- the modes, the re-arm and the floors, per destination, interleaved -------------------
    // Interleaved rather than all-modes-then-all-floors so a failure part way leaves one COMPLETE
    // database rather than two half ones — and because modes-then-floors is the order the bash
    // flow was run in, per database, against real dumps.
    for (i, destination) in destinations.iter().enumerate() {
        let n = 4 + 3 * i as u8;
        let database = destination.shard;
        let profile = destination.profile.name();
        let contents = if destination.profile.has_bounded_slices() {
            "creatures, quests, loot, vendors, terrain, navigation, spells"
        } else {
            "instance creatures, loot, gameobjects, spells"
        };

        // ---- the importer's modes, in the bash flow's order ----------------------------------
        stage(
            n,
            total,
            &format!("importing {profile} into {database} ({contents})"),
        );
        if i == 0 {
            println!(
                "   this is the long one — tens of minutes; each mode prints its own progress."
            );
        } else if destination.profile == WorldProfile::Instances {
            println!("   instance-only profile: open-world terrain and navigation are skipped.");
        }
        for (what, advice, command) in world_etl_commands(project, &client_data, *destination) {
            println!();
            println!("   -> {what}");
            runner.run_streaming(&command).map_err(|e| {
                stage_failure(&format!("{what} for {database} ({profile})"), advice, e)
            })?;
        }

        // ---- re-arm this database --------------------------------------------------------------
        // The bash flow swallowed these two calls' exit status (`>/dev/null 2>&1`, no guard). The
        // port checks them deliberately: an un-rearmed creature tick or an unarmed gather pool is
        // invisible to every floor below and only surfaces in play, which is exactly the
        // silent-success shape this pipeline exists to refuse.
        stage(
            n + 1,
            total,
            &format!("re-arming {database} (creature tick, fixtures, gather pools)"),
        );
        for reducer in ["debug_repair_after_publish", "arm_all_pools"] {
            runner
                .run_and_wait(&call_command(project, database, reducer))
                .map_err(|e| {
                    stage_failure(
                        &format!("{reducer} for {database} ({profile})"),
                        "Both reducers are operator-gated — `lyracore dev up` publishes the \
                         module and claims the operator; is the stack up (`lyracore dev status`)?",
                        e,
                    )
                })?;
        }

        // ---- the FLOOR_* assertions ------------------------------------------------------------
        stage(
            n + 2,
            total,
            &format!("asserting the import floors on {database} (the FLOOR_* manifest)"),
        );
        assert_floors(project, runner, database, destination.profile, &floors).map_err(
            |error| {
                Error::Process(format!(
                    "asserting import floors for {database} ({profile}) failed: {error}"
                ))
            },
        )?;
    }

    println!();
    println!(
        "import complete. The realm now holds the imported world rather than the seed fixture."
    );
    println!("Nothing produced here is redistributable — see the notice this command opened with.");
    Ok(())
}

/// Content destinations, in import order. The importer owns every profile's spatial facts; the CLI
/// owns only the Realm topology assignment.
fn import_destinations(topology: Topology) -> Vec<ImportDestination> {
    match topology {
        Topology::Single => vec![ImportDestination {
            shard: ProjectLayout::DATABASE,
            profile: WorldProfile::AllianceSingle,
        }],
        Topology::Sharded => vec![
            ImportDestination {
                shard: ProjectLayout::DATABASE,
                profile: WorldProfile::AllianceEastern,
            },
            ImportDestination {
                shard: ProjectLayout::KALIMDOR_SHARD,
                profile: WorldProfile::AllianceKalimdor,
            },
            ImportDestination {
                shard: ProjectLayout::INSTANCE_POOL,
                profile: WorldProfile::Instances,
            },
        ],
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

// =============================================================================================
//  `import vmaps`
// =============================================================================================

/// Drive the importer's `--vmap` mode — exact per-cell model/WMO collision triangles →
/// `game_vmap_chunk` — for each bounded World Shard profile in the running topology.
///
/// No consent gate, on purpose: unlike `import world` this fetches nothing from anyone — the only
/// input is the operator's own client archives, named by the same `--client-data`/config/prompt
/// chain, and the only output is the local database. The notice below still says so before
/// anything is read.
///
/// The canonical profile is shared with the dump, terrain and navigation modes, so collision
/// coverage follows the same bounded slices without this caller reproducing their coordinates.
pub fn run_vmaps(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    client_data: Option<&str>,
) -> Result<()> {
    // Same early check as `import world`: a typo in the flag is a usage mistake, refused first.
    if let Some(raw) = client_data {
        validate_client_data(Path::new(raw))?;
    }

    println!("lyracore import vmaps — exact model/WMO collision, from YOUR client");
    println!(
        "Reads the model/WMO archives out of your own 1.12.1 client's Data/ directory and writes"
    );
    println!(
        "per-cell collision triangles into this checkout's local database(s). Nothing is fetched"
    );
    println!("from the network, and nothing read here leaves your machine.");
    println!();

    let data = resolve_client_data(project, prompt, client_data)?;
    println!("   client data: {}", data.display());

    runner
        .run_and_wait(&build_importer_command(project))
        .map_err(|e| {
            stage_failure(
                "building the importer",
                "The importer builds with the checkout's own pinned toolchain — `lyracore doctor` \
                 checks the prerequisites.",
                e,
            )
        })?;

    let destinations = import_destinations(RuntimeState::load(&project.state_file())?.topology());
    let world_shards: Vec<ImportDestination> = destinations
        .into_iter()
        .filter(|destination| destination.profile.has_bounded_slices())
        .collect();
    for destination in &world_shards {
        let shard = destination.shard;
        let profile = destination.profile.name();
        println!();
        println!("==> {shard}: vmap extract + import ({profile})");
        runner
            .run_streaming(&vmap_command(project, *destination, &data))
            .map_err(|e| {
                stage_failure(
                    &format!("importing vmaps for {shard} ({profile})"),
                    "The extract reads model.MPQ/wmo.MPQ out of the client archives; \
                     `no MCNK cells intersected the profile` means its bounded slices matched no \
                     tiles. The --apply half needs the node up and the World Shard published \
                     (`lyracore dev up`).",
                    e,
                )
            })?;
    }

    println!();
    println!("vmaps imported. The exact rays stay gated until you flip the module config:");
    for destination in world_shards {
        println!(
            "  spacetime call --server {} {} debug_set_vmap_enabled true",
            ProjectLayout::stdb_uri(),
            destination.shard
        );
    }
    println!("— flipping it is a runtime decision, not part of this import, so it is not done here.");
    Ok(())
}

// =============================================================================================
//  locating and validating the client data
// =============================================================================================

/// The flag if there is one, otherwise the persisted config, otherwise ask — and save a freshly
/// prompted-for answer so the NEXT run does not have to ask again.
///
/// The chain, in order: `--client-data` wins outright (a typo in it is a usage mistake the caller
/// already refused before anything was spent); the `config.json` value is used if it still
/// validates, and reported-then-abandoned if it does not (a stale config must not turn into a
/// silent prompt-less failure); the interactive prompt is the fallback of last resort, and its
/// answer is written back to `config.json` once it validates.
fn resolve_client_data(
    project: &ProjectLayout,
    prompt: &dyn Prompt,
    flag: Option<&str>,
) -> Result<PathBuf> {
    if let Some(raw) = flag {
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

// ---- the invocations ------------------------------------------------------------------------

/// Every child runs from the CHECKOUT ROOT: the scripts and the importer resolve `.import/`,
/// `target/debug/…` and each other relative to it, and `lyracore` is usable from any subdirectory
/// of a checkout.
fn from_root(project: &ProjectLayout, script: PathBuf) -> CommandSpec {
    CommandSpec::new("bash")
        .arg(script.to_string_lossy().to_string())
        .cwd(project.root.clone())
}

fn pull_command(project: &ProjectLayout) -> CommandSpec {
    from_root(project, project.pull_classic_db_script())
}

fn build_importer_command(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("cargo")
        .arg("build")
        .arg("-q")
        .arg("--bin")
        .arg("lyracore-importer")
        .cwd(project.root.clone())
}

/// A base importer invocation: the pinned binary, destination Shard and loopback endpoint named
/// explicitly. Neither may fall through to ambient CLI state.
fn importer_command(project: &ProjectLayout, database: &str) -> CommandSpec {
    CommandSpec::new(project.importer_bin().to_string_lossy().to_string())
        .arg("--db")
        .arg(database)
        .arg("--server")
        .arg(ProjectLayout::stdb_uri())
        .cwd(project.root.clone())
}

/// The world ETL: the importer's modes, in `import-world.sh`'s order, each with the advice its
/// failure needs. One list so the ordering is a fact in one place — the ordering IS the port.
///
/// Two deliberate differences from the bash flow: nothing is piped through a reporting `grep`
/// (the child's full output streams, and its exit status is checked mode by mode — the bash ran
/// every mode even after one failed and let the assertions catch the fallout), and the curated
/// class-spell overlay therefore runs exactly ONCE (the old façade ran it twice because the bash
/// swallowed its exit status inside that grep).
fn world_etl_commands(
    project: &ProjectLayout,
    client_data: &Path,
    destination: ImportDestination,
) -> Vec<(&'static str, &'static str, CommandSpec)> {
    let data = client_data.to_string_lossy().to_string();
    let database = destination.shard;
    let profile = destination.profile;
    let mut dump = importer_command(project, database)
        .arg("--dump")
        .arg(DUMP_PATH)
        .arg("--dbc")
        .arg(&data)
        .arg("--world-profile")
        .arg(profile.name());
    if profile.includes_eastern_corridors() {
        dump = dump.arg("--include-creatures").arg(INCLUDE_CREATURES);
    }
    dump = dump.arg("--apply");

    let mut commands = vec![(
        "world content (creatures, quests, loot, vendors, gameobjects)",
        "The dump families are clear+reload — a failed run leaves the target partially loaded; \
         re-run after fixing the cause. `no spawns matched the profile` means the canonical \
         profile does not fit the pinned dump; verify its dump-derived scope in the importer.",
        dump,
    )];

    if profile.has_bounded_slices() {
        commands.extend([
            (
            "terrain heightmap (ground-z)",
            "Reads terrain.MPQ. A self-check failure is an axis/interpolation regression, not a \
             missing file — do not re-run past it.",
            importer_command(project, database)
                .arg("--terrain")
                .arg(&data)
                .arg("--world-profile")
                .arg(profile.name())
                .arg("--apply"),
            ),
            (
            "nav grid (walkability / line of sight)",
            "Reads model.MPQ/wmo.MPQ. A handful of M2 parse warnings is expected (decorative \
             props); a calibration failure means transform drift and must not be re-run past.",
            importer_command(project, database)
                .arg("--nav")
                .arg(&data)
                .arg("--world-profile")
                .arg(profile.name())
                .arg("--apply"),
            ),
        ]);
    }

    commands.extend([
        (
            "character-creation + faction DBC tables",
            "Reads dbc.MPQ. Without this pass non-Warrior classes inherit the Warrior loadout — \
             it must not be skipped.",
            importer_command(project, database)
                .arg("--dbc")
                .arg(&data)
                .arg("--apply"),
        ),
        (
            "real talent trees (TalentTab.dbc + Talent.dbc)",
            "Reads dbc.MPQ. Without this the demo talent ids do not match what the 5875 client \
             sends, and talent clicks silently no-op.",
            importer_command(project, database)
                .arg("--dbc")
                .arg(&data)
                .arg("--talents")
                .arg("--apply"),
        ),
        (
            "full Spell.dbc import (every non-zero row)",
            "Reads dbc.MPQ. The long tail lands as scripted no-ops by design; a parse failure \
             here is a client-build problem, not that.",
            importer_command(project, database)
                .arg("--dbc")
                .arg(&data)
                .arg("--spells")
                .arg("--apply"),
        ),
        (
            "curated class spells (all nine classes)",
            "This reads Spell.dbc out of the client archives and writes trainer offerings. A \
             \"requested N ids but matched M\" warning names ids your client build does not have.",
            class_spells_command(project, client_data, database),
        ),
        (
            "caster-mob spells (the Elwynn/Westfall --only allowlist)",
            "Additive --only import. A \"requested N ids but matched M\" warning is a wrong or \
             typo'd id in the allowlist — correct the id rather than shipping a mute caster.",
            importer_command(project, database)
                .arg("--dbc")
                .arg(&data)
                .arg("--spells")
                .arg("--apply")
                .arg("--only")
                .arg(CASTER_SPELL_IDS),
        ),
    ]);
    commands
}

/// The class-spell import takes the client path as its argument, the destination from `DB`, and the
/// node from `SPACETIME_SERVER`. Pass all three explicitly so no Shard or node comes from ambient
/// shell state.
fn class_spells_command(
    project: &ProjectLayout,
    client_data: &Path,
    database: &str,
) -> CommandSpec {
    from_root(project, project.import_class_spells_script())
        .arg(client_data.to_string_lossy().to_string())
        .env("DB", database)
        .env("SPACETIME_SERVER", ProjectLayout::stdb_uri())
}

/// `spacetime call`, pinned to the loopback node — bare `spacetime call` inherits the CLI's
/// AMBIENT default server, which on a fresh machine is maincloud, not the node `dev up` started
/// (the #440 class of failure the bash flow's `SPACETIME_SERVER` chokepoint exists for).
fn call_command(project: &ProjectLayout, database: &str, reducer: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("call")
        .arg("--server")
        .arg(ProjectLayout::stdb_uri())
        .arg(database)
        .arg(reducer)
        .cwd(project.root.clone())
}

/// `spacetime sql`, pinned to the loopback node for the same #440 reason as [`call_command`].
fn sql_command(project: &ProjectLayout, database: &str, query: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("sql")
        .arg("--server")
        .arg(ProjectLayout::stdb_uri())
        .arg(database)
        .arg(query)
        .cwd(project.root.clone())
}

fn vmap_command(
    project: &ProjectLayout,
    destination: ImportDestination,
    client_data: &Path,
) -> CommandSpec {
    importer_command(project, destination.shard)
        .arg("--vmap")
        .arg(client_data.to_string_lossy().to_string())
        .arg("--world-profile")
        .arg(destination.profile.name())
        .arg("--apply")
}

fn stage(number: u8, total: u8, what: &str) {
    println!();
    println!("==> [{number}/{total}] {what}");
}

/// Wrap a stage failure with what to do about it, keeping the child's own diagnosis.
fn stage_failure(what: &str, advice: &str, cause: Error) -> Error {
    Error::Process(format!("{what} failed.\n  {advice}\n  ({cause})"))
}

// =============================================================================================
//  the FLOOR_* manifest assertions — the port of import-world.sh's `chk` block
// =============================================================================================

/// Every FLOOR_* key the assertion pass below consumes — one per `chk`, in assertion order. Kept
/// in lockstep with `importer/scripts/import-manifest.sh` (the same contract the core repo's
/// import-manifest-smoke.sh pins for the bash consumer): `run_world` demands every one of these
/// from the sourced manifest BEFORE the first stage runs, so a floor that goes missing fails the
/// import up front instead of downgrading its check to a silent pass.
const CONSUMED_FLOORS: [&str; 47] = [
    "FLOOR_CLASS_TRAINERS",
    "FLOOR_TRAINER_OFFERINGS_CLASS",
    "FLOOR_TRAINER_OFFERINGS_PROFESSION",
    "FLOOR_GATHER_NODE_GO",
    "FLOOR_TERRAIN_CHUNKS",
    "FLOOR_NAV_CHUNKS",
    "FLOOR_INNKEEPER_FARLEY",
    "FLOOR_CONTINENT_SPAWNS",
    "FLOOR_QUEST_GIVER_RELATIONS",
    "FLOOR_QUESTS_L6_10",
    "FLOOR_QUESTS_L10_20",
    "FLOOR_QUESTS_CHAINED",
    "FLOOR_LIVE_CREATURES",
    "FLOOR_VENDORS",
    "FLOOR_TRAINER_COVERAGE",
    "FLOOR_VENDOR_COVERAGE",
    "FLOOR_QUESTGIVER_COVERAGE",
    "FLOOR_SENTINEL_HILL_VENDOR",
    "FLOOR_START_ITEMS_CLASSES",
    "FLOOR_CASTER_CAST_ROWS",
    "FLOOR_SPELL_TOTAL",
    "FLOOR_SPELL_CHAIN",
    "FLOOR_SPELL_LEARN",
    "FLOOR_GEOMANCER_CAST_SPELL",
    "FLOOR_ROGUE_WIZARD_CAST_SPELL",
    "FLOOR_IMP_FIREBOLT_CAST_ROW",
    "FLOOR_IMP_FIREBOLT_SPELL",
    "FLOOR_DEFIAS_CASTER_CAST_ROW",
    "FLOOR_ROTATION_ROWS",
    "FLOOR_GEOMANCER_ROTATION_NUKE",
    "FLOOR_FROST_ARMOR_ROTATION",
    "FLOOR_AREAS",
    "FLOOR_AREA_TRIGGERS",
    "FLOOR_GRAVEYARDS",
    "FLOOR_GRAVEYARD_ZONE_RESOLVE",
    "FLOOR_AREATRIGGER_TELEPORTS",
    "FLOOR_DEADMINES_PORTAL_ROUNDTRIP",
    "FLOOR_DEADMINES_CREATURE_SPAWNS",
    "FLOOR_DEADMINES_GAMEOBJECTS",
    "FLOOR_DEADMINES_BOSSES",
    "FLOOR_DEADMINES_NAMED_LOOT",
    "FLOOR_PICKPOCKET_LOOT",
    "FLOOR_SKINNING_LOOT",
    "FLOOR_GAMEOBJECT_CHEST_LOOT",
    "FLOOR_FISHING_LOOT",
    "FLOOR_CREATURES_WITH_PICKPOCKET",
    "FLOOR_CREATURES_WITH_SKIN",
];

/// Trainers with NO offerings BY DESIGN — each has a system that is not the class-spell path, so
/// they are annotated in the coverage audit rather than reported as dead.
const KNOWN_EMPTY_TRAINERS: [(u32, &str); 4] = [
    (2485, "portal trainer (teleports not curated yet)"),
    (4732, "riding trainer (189 mounts)"),
    (2879, "pet trainer (188 pet system)"),
    (15351, "PvP-rank reward vendor (no honor system yet)"),
];

/// The FLOOR_* minimums, as sourced from the manifest by [`load_floors`].
struct Floors(BTreeMap<String, i64>);

impl Floors {
    fn get(&self, key: &str) -> Result<i64> {
        self.0.get(key).copied().ok_or_else(|| {
            Error::Process(format!(
                "{} defines no {key} — every assertion in this flow must find its floor there. A \
                 floor that has gone missing would otherwise read as a check that silently \
                 stopped existing, which is the one failure mode this pipeline cannot afford.",
                ProjectLayout::IMPORT_MANIFEST_SCRIPT
            ))
        })
    }

    fn require(&self, keys: &[&str]) -> Result<()> {
        for key in keys {
            self.get(key)?;
        }
        Ok(())
    }
}

/// Dump every `FLOOR_*` the sourced manifest defines, one `KEY=value` per line. Sourced through
/// bash rather than parsed in Rust because the manifest IS shell — its `[ "${SLICE:-0}" = 1 ]`
/// override block re-points floors for non-canonical runs, and a line-parser would read those
/// override values as the canonical ones.
const FLOOR_DUMP_SNIPPET: &str = r#"set -u; source "$1" >/dev/null || exit 1; for v in "${!FLOOR_@}"; do printf '%s=%s\n' "$v" "${!v}"; done"#;

fn floors_command(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("bash")
        .arg("-c")
        .arg(FLOOR_DUMP_SNIPPET)
        .arg("bash")
        .arg(project.import_manifest_script().to_string_lossy().to_string())
        // The CLI runs ONLY the canonical map-0 corridor, so the manifest's SLICE/MAP override
        // block must never fire — pinned here rather than inherited, because a contributor's
        // exported MAP=1 would otherwise silently swap every floor for a presence value.
        .env("MAP", "0")
        .env("SLICE", "0")
        .cwd(project.root.clone())
}

fn load_floors(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<Floors> {
    let output = runner.run_and_wait(&floors_command(project)).map_err(|e| {
        stage_failure(
            "reading the import floors",
            "importer/scripts/import-manifest.sh is a KEY=value data file and must stay \
             `source`able — see its header.",
            e,
        )
    })?;
    let mut floors = BTreeMap::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            Error::Process(format!(
                "unparseable line from {}: {line:?} — expected FLOOR_KEY=integer",
                ProjectLayout::IMPORT_MANIFEST_SCRIPT
            ))
        })?;
        let value: i64 = value.trim().parse().map_err(|_| {
            Error::Process(format!(
                "{}: {key} is not a number ({:?}) — the FLOOR_* section is KEY=value integers \
                 only, per its own header.",
                ProjectLayout::IMPORT_MANIFEST_SCRIPT,
                value.trim()
            ))
        })?;
        floors.insert(key.trim().to_string(), value);
    }
    Ok(Floors(floors))
}

/// Which output lines of a `spacetime sql` count as rows — the ports of the bash `n()` patterns.
#[derive(Clone, Copy)]
enum RowMatch {
    /// `^ *[0-9]` — any row whose first field is numeric (the default).
    Numeric,
    /// `[0-9]` — any line with a digit anywhere.
    AnyDigit,
    /// `[0-9]{6,}` — live-entity guids; a six-plus digit run, so template ids cannot count.
    Guid,
    /// `^ *N *$` — exactly this number on a row of its own (the graveyard resolve probe).
    Exactly(i64),
}

impl RowMatch {
    fn matches(self, line: &str) -> bool {
        match self {
            RowMatch::Numeric => line
                .trim_start()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
            RowMatch::AnyDigit => line.chars().any(|c| c.is_ascii_digit()),
            RowMatch::Guid => {
                let mut run = 0;
                for c in line.chars() {
                    if c.is_ascii_digit() {
                        run += 1;
                        if run >= 6 {
                            return true;
                        }
                    } else {
                        run = 0;
                    }
                }
                false
            }
            RowMatch::Exactly(n) => line.trim() == n.to_string(),
        }
    }
}

/// The lines of a `spacetime sql` answer that are a bare number — the port of the bash `q_list`,
/// backing the checks that filter or dedup raw values instead of counting rows.
fn numeric_values(output: &str) -> Vec<i64> {
    output
        .lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            t.parse().ok()
        })
        .collect()
}

/// A `spacetime sql` answer as rows of cells — data lines start with a space; cells split on `|`.
/// The port of the coverage audit's python `rows()` helper.
fn table_cells(output: &str) -> Vec<Vec<String>> {
    output
        .lines()
        .filter(|line| line.starts_with(' '))
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

/// The assertion pass's working state: every query goes through [`Assertions::sql`], every
/// comparison through [`Assertions::chk`], and failures are COLLECTED — the whole picture prints
/// before the run fails, exactly like the bash `fail=1` bookkeeping.
struct Assertions<'a> {
    project: &'a ProjectLayout,
    runner: &'a dyn ProcessRunner,
    database: &'a str,
    floors: &'a Floors,
    failed: usize,
}

impl Assertions<'_> {
    /// One query, with the #440 discipline: a FAILED query is not the same as zero rows, and
    /// conflating them is how one dead connection turns into dozens of fake "table is empty"
    /// FAILs — so the first failure aborts the whole pass instead of reading as a count of 0.
    fn sql(&self, query: &str) -> Result<String> {
        self.runner
            .run_and_wait(&sql_command(self.project, self.database, query))
            .map_err(|e| {
                Error::Process(format!(
                    "a verification query against '{db}' failed — a failed query is NOT zero \
                     rows, so no further assertion is reported. Check the node is up and '{db}' \
                     is published (`lyracore dev status`), then re-run `lyracore import world`.\n  \
                     ({e})",
                    db = self.database
                ))
            })
    }

    fn count(&self, query: &str, rows: RowMatch) -> Result<i64> {
        Ok(self
            .sql(query)?
            .lines()
            .filter(|line| rows.matches(line))
            .count() as i64)
    }

    fn values(&self, query: &str) -> Result<Vec<i64>> {
        Ok(numeric_values(&self.sql(query)?))
    }

    fn chk(&mut self, key: &'static str, label: &str, actual: i64) -> Result<()> {
        let floor = self.floors.get(key)?;
        if actual < floor {
            println!("  FAIL  {label}: got {actual}, want >= {floor}");
            self.failed += 1;
        } else {
            println!("  ok    {label}: {actual}");
        }
        Ok(())
    }

    fn chk_count(
        &mut self,
        key: &'static str,
        label: &str,
        query: &str,
        rows: RowMatch,
    ) -> Result<()> {
        let actual = self.count(query, rows)?;
        self.chk(key, label, actual)
    }
}

/// One flagged-NPC service audit: which spawned templates advertise `bit` in npc_flags, and how
/// many of those actually appear in the provider table. Prints the bash/python audit's line and
/// returns the covered count for its floor.
fn audit_service(
    label: &str,
    bit: u32,
    spawned: &BTreeSet<u32>,
    templates: &BTreeMap<u32, (u32, String)>,
    providers: &BTreeSet<u32>,
) -> i64 {
    let known_empty: BTreeMap<u32, &str> = KNOWN_EMPTY_TRAINERS.iter().copied().collect();
    let flagged: BTreeSet<u32> = spawned
        .iter()
        .filter(|entry| {
            templates
                .get(entry)
                .is_some_and(|(flags, _)| flags & bit != 0)
        })
        .copied()
        .collect();
    let providing: Vec<u32> = flagged.intersection(providers).copied().collect();
    let silent: Vec<u32> = flagged.difference(providers).copied().collect();
    let dead: Vec<u32> = silent
        .iter()
        .filter(|entry| !known_empty.contains_key(entry))
        .copied()
        .collect();
    let known: Vec<u32> = silent
        .iter()
        .filter(|entry| known_empty.contains_key(entry))
        .copied()
        .collect();

    let mark = if dead.is_empty() { "ok   " } else { "GAP  " };
    let mut line = format!(
        "  {mark} {label}: flagged+spawned={} providing={} dead={}",
        flagged.len(),
        providing.len(),
        dead.len()
    );
    if !known.is_empty() {
        let notes: Vec<&str> = known.iter().map(|entry| known_empty[entry]).collect();
        line.push_str(&format!(
            " known-empty={} ({})",
            known.len(),
            notes.join(", ")
        ));
    }
    if !dead.is_empty() {
        let names: Vec<String> = dead
            .iter()
            .take(12)
            .map(|entry| {
                let name = templates
                    .get(entry)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or("?");
                format!("{name}({entry})")
            })
            .collect();
        line.push_str(&format!(": {}", names.join(", ")));
        if dead.len() > 12 {
            line.push_str(" …");
        }
    }
    println!("{line}");
    providing.len() as i64
}

/// The FLOOR_* assertion pass — `import-world.sh`'s `chk` block, ported check for check, label
/// for label, in the bash order. Everything here runs on the canonical map-0 corridor run, so the
/// bash's `chk0`/`chk36` map fences collapse to always-on (the CLI cannot express another
/// continent — that is the by-hand path).
///
/// Left in the bash on purpose: the one-continent-per-database preflight and post-run re-assert
/// (this flow only ever runs map 0+36 at the fixture database, so the map switch they guard
/// against cannot be expressed here) and every MAP/BOX/CENTER knob. The service-coverage audit is
/// PORTED, natively — the bash needed python3 for it and skipped it when absent; this pass has no
/// skip path.
fn assert_floors(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    database: &str,
    profile: WorldProfile,
    floors: &Floors,
) -> Result<()> {
    let mut a = Assertions {
        project,
        runner,
        database,
        floors,
        failed: 0,
    };
    println!("   assertions ({} → {database}):", profile.name());

    a.chk_count(
        "FLOOR_CLASS_TRAINERS",
        "Goldshire anchor trainers spawned {328,377,906,913,917,927} (NOT total coverage — see the service-coverage audit below)",
        "SELECT guid FROM game_world_entity WHERE owner_guid=0 AND (entry=328 OR entry=377 OR entry=906 OR entry=913 OR entry=917 OR entry=927)",
        RowMatch::Guid,
    )?;
    a.chk_count(
        "FLOOR_TRAINER_OFFERINGS_CLASS",
        "class trainer offerings (learn_skill_line=0)",
        "SELECT spell_id FROM game_trainer_spell WHERE learn_skill_line = 0",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_TRAINER_OFFERINGS_PROFESSION",
        "profession learn offerings (learn_skill_line>0)",
        "SELECT spell_id FROM game_trainer_spell WHERE learn_skill_line > 0",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_GATHER_NODE_GO",
        "gather-node GO spawns",
        "SELECT guid FROM game_gameobject",
        RowMatch::AnyDigit,
    )?;
    a.chk_count(
        "FLOOR_TERRAIN_CHUNKS",
        "terrain chunks (ground-z heightmap)",
        "SELECT key FROM game_terrain_chunk",
        RowMatch::AnyDigit,
    )?;
    a.chk_count(
        "FLOOR_NAV_CHUNKS",
        "nav chunks (walkability/LoS grid)",
        "SELECT key FROM game_nav_chunk",
        RowMatch::AnyDigit,
    )?;
    a.chk_count(
        "FLOOR_INNKEEPER_FARLEY",
        "innkeeper Farley(295) spawned",
        "SELECT guid FROM game_world_entity WHERE entry=295 AND owner_guid=0",
        RowMatch::Guid,
    )?;
    a.chk_count(
        "FLOOR_CONTINENT_SPAWNS",
        "spawn rows on this run's map (0)",
        "SELECT guid FROM game_creature_spawn WHERE map_id = 0",
        RowMatch::AnyDigit,
    )?;
    a.chk_count(
        "FLOOR_QUEST_GIVER_RELATIONS",
        "quest-giver relations",
        "SELECT quest_entry FROM game_creature_quest",
        RowMatch::Numeric,
    )?;

    // The quest-level bands filter raw values shell-side in the bash (a single-column inequality
    // filter in SQL is itself the can-return-0-rows-wrongly trap its comment cites) — same here.
    let quest_levels = a.values("SELECT quest_level FROM game_quest_template")?;
    let l6_10 = quest_levels.iter().filter(|l| (6..=10).contains(*l)).count() as i64;
    a.chk("FLOOR_QUESTS_L6_10", "L6-10 quests (quest_level band)", l6_10)?;
    let l10_20 = quest_levels.iter().filter(|l| (10..=20).contains(*l)).count() as i64;
    a.chk(
        "FLOOR_QUESTS_L10_20",
        "L10-20 quests (Westfall quest_level band)",
        l10_20,
    )?;
    let chained = a
        .values("SELECT next_quest_id FROM game_quest_template")?
        .iter()
        .filter(|v| **v > 0)
        .count() as i64;
    a.chk(
        "FLOOR_QUESTS_CHAINED",
        "chained quests (next_quest_id>0) [V]",
        chained,
    )?;
    // Timed quests are coverage-printed only, no floor — plausibly zero in this slice, per the
    // manifest's own note.
    let timed = a
        .values("SELECT limit_time FROM game_quest_template")?
        .iter()
        .filter(|v| **v > 0)
        .count();
    println!("  ..    timed quests (limit_time>0) [V]: {timed}");

    a.chk_count(
        "FLOOR_LIVE_CREATURES",
        "live creature entities",
        "SELECT guid FROM game_world_entity WHERE entry>0 AND owner_guid=0",
        RowMatch::Guid,
    )?;
    a.chk_count(
        "FLOOR_VENDORS",
        "vendors (npc_vendor)",
        "SELECT item_entry FROM game_npc_vendor",
        RowMatch::Numeric,
    )?;

    // --- Service coverage: an NPC that ADVERTISES a service via npc_flags must actually PROVIDE
    // it. This audit exists because the row-count assertions above once certified an import "OK"
    // while every spawned Northshire class trainer taught NOTHING — it joins spawned+flagged
    // templates against the provider tables HERE (spacetime sql has no JOIN) and names the dead
    // NPCs. Floors guard non-regression on the COVERED counts; the dead lists are the honest gap
    // ledger (fix = give them rows, not silence).
    println!("   service coverage (flagged NPCs that actually provide their service):");
    let mut templates: BTreeMap<u32, (u32, String)> = BTreeMap::new();
    for row in table_cells(&a.sql("SELECT entry, npc_flags, name FROM game_creature_template")?) {
        let (Some(entry), Some(flags)) = (
            row.first().and_then(|c| c.parse().ok()),
            row.get(1).and_then(|c| c.parse().ok()),
        ) else {
            continue;
        };
        let name = row
            .get(2)
            .map(|c| c.trim_matches('"').to_string())
            .unwrap_or_default();
        templates.insert(entry, (flags, name));
    }
    let entry_set = |a: &Assertions, query: &str| -> Result<BTreeSet<u32>> {
        Ok(table_cells(&a.sql(query)?)
            .iter()
            .filter_map(|row| row.first().and_then(|c| c.parse().ok()))
            .collect())
    };
    let spawned = entry_set(&a, "SELECT entry FROM game_world_entity WHERE owner_guid = 0")?;
    let teach = entry_set(&a, "SELECT trainer_entry FROM game_trainer_spell")?;
    let sell = entry_set(&a, "SELECT creature_entry FROM game_npc_vendor")?;
    let give = entry_set(&a, "SELECT creature_entry FROM game_creature_quest")?;
    let trainers = audit_service(
        "trainers (npc_flags&0x10 vs game_trainer_spell)",
        0x10,
        &spawned,
        &templates,
        &teach,
    );
    let vendors = audit_service(
        "vendors (npc_flags&0x4 vs game_npc_vendor)",
        0x4,
        &spawned,
        &templates,
        &sell,
    );
    let questgivers = audit_service(
        "questgivers (npc_flags&0x2 vs game_creature_quest; quiet ones may be off-level content)",
        0x2,
        &spawned,
        &templates,
        &give,
    );
    a.chk(
        "FLOOR_TRAINER_COVERAGE",
        "trainer coverage (spawned trainers that teach)",
        trainers,
    )?;
    a.chk(
        "FLOOR_VENDOR_COVERAGE",
        "vendor coverage (spawned vendors that sell)",
        vendors,
    )?;
    a.chk(
        "FLOOR_QUESTGIVER_COVERAGE",
        "questgiver coverage (spawned givers with quests)",
        questgivers,
    )?;

    a.chk_count(
        "FLOOR_SENTINEL_HILL_VENDOR",
        "Sentinel Hill vendor spawned (Quartermaster Lewis 491)",
        "SELECT guid FROM game_world_entity WHERE owner_guid=0 AND entry=491",
        RowMatch::Guid,
    )?;
    // All 6 Human classes have a creation loadout (race_class = (race<<8)|class). DISTINCT
    // present values — the WHERE caps it at 6, so distinct-count 6 means all six landed.
    let start_items: BTreeSet<i64> = a
        .values(
            "SELECT race_class FROM game_start_item WHERE race_class=257 OR race_class=258 OR race_class=260 OR race_class=261 OR race_class=264 OR race_class=265",
        )?
        .into_iter()
        .collect();
    a.chk(
        "FLOOR_START_ITEMS_CLASSES",
        "Human start-items (all 6 classes)",
        start_items.len() as i64,
    )?;
    a.chk_count(
        "FLOOR_CASTER_CAST_ROWS",
        "caster-mob cast rows",
        "SELECT creature_entry FROM game_creature_cast",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_SPELL_TOTAL",
        "total game_spell rows (full Spell.dbc import)",
        "SELECT spell_id FROM game_spell",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_SPELL_CHAIN",
        "spell chain rows (game_spell_chain) [V]",
        "SELECT spell_id FROM game_spell_chain",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_SPELL_LEARN",
        "spell auto-learn dependents (game_spell_learn) [V]",
        "SELECT id FROM game_spell_learn",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_GEOMANCER_CAST_SPELL",
        "476 Geomancer cast spell present",
        "SELECT spell_id FROM game_spell WHERE spell_id=133",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_ROGUE_WIZARD_CAST_SPELL",
        "474 Rogue Wizard cast spell present",
        "SELECT spell_id FROM game_spell WHERE spell_id=13322",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_IMP_FIREBOLT_CAST_ROW",
        "Imp(416) Firebolt cast row",
        "SELECT spell_id FROM game_creature_cast WHERE creature_entry=416",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_IMP_FIREBOLT_SPELL",
        "Imp Firebolt(3110) in game_spell",
        "SELECT spell_id FROM game_spell WHERE spell_id=3110",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_DEFIAS_CASTER_CAST_ROW",
        "Defias caster cast row (Pillager 589?/Conjurer 449? [V])",
        "SELECT creature_entry FROM game_creature_cast WHERE creature_entry=589 OR creature_entry=449",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_ROTATION_ROWS",
        "rotation rows (game_creature_spell)",
        "SELECT id FROM game_creature_spell",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_GEOMANCER_ROTATION_NUKE",
        "476 Geomancer rotation nuke row",
        "SELECT id FROM game_creature_spell WHERE creature_entry=476 AND condition=0",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_FROST_ARMOR_ROTATION",
        "474/476 Frost Armor rotation row",
        "SELECT id FROM game_creature_spell WHERE spell_id=12544",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_AREAS",
        "areas imported (game_area) [V]",
        "SELECT id FROM game_area",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_AREA_TRIGGERS",
        "area triggers imported (game_area_trigger) [V]",
        "SELECT id FROM game_area_trigger",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_GRAVEYARDS",
        "graveyards imported (game_graveyard) [V]",
        "SELECT id FROM game_graveyard",
        RowMatch::Numeric,
    )?;

    // Zone-12 (Elwynn) graveyard links must resolve to a real game_graveyard row — spacetime sql
    // has no JOIN, so this is done here, link by link, like the bash.
    println!("   zone-12 graveyard_zone→game_graveyard resolve check (client-side, no SQL JOIN):");
    let mut resolved = 0;
    for id in a.values("SELECT safe_loc_id FROM game_graveyard_zone WHERE zone_id = 12")? {
        if a.count(
            &format!("SELECT id FROM game_graveyard WHERE id = {id}"),
            RowMatch::Exactly(id),
        )? > 0
        {
            resolved += 1;
        }
    }
    a.chk(
        "FLOOR_GRAVEYARD_ZONE_RESOLVE",
        "zone-12 graveyard_zone links resolve to a real game_graveyard row",
        resolved,
    )?;

    a.chk_count(
        "FLOOR_AREATRIGGER_TELEPORTS",
        "areatrigger_teleport portals imported (game_areatrigger_teleport) [V]",
        "SELECT trigger_id FROM game_areatrigger_teleport",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_DEADMINES_PORTAL_ROUNDTRIP",
        "Deadmines portal round-trip: entrance(78)+exit(119) both present",
        "SELECT trigger_id FROM game_areatrigger_teleport WHERE trigger_id=78 OR trigger_id=119",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_DEADMINES_CREATURE_SPAWNS",
        "Deadmines creature spawns (map 36) [V]",
        "SELECT guid FROM game_creature_spawn WHERE map_id = 36",
        RowMatch::AnyDigit,
    )?;
    a.chk_count(
        "FLOOR_DEADMINES_GAMEOBJECTS",
        "Deadmines gameobjects (doors/cannon/chests, map 36) [V]",
        "SELECT guid FROM game_gameobject WHERE map_id = 36",
        RowMatch::AnyDigit,
    )?;
    // Placed bosses, DISTINCT entries. Sneed 643 stays in the query but not the floor — he is
    // script-summoned in cmangos-era dumps, so a correct import usually has no placed row.
    let bosses: BTreeSet<i64> = a
        .values(
            "SELECT entry FROM game_creature_spawn WHERE map_id = 36 AND (entry=644 OR entry=643 OR entry=642 OR entry=1763 OR entry=646 OR entry=647 OR entry=639)",
        )?
        .into_iter()
        .collect();
    a.chk(
        "FLOOR_DEADMINES_BOSSES",
        "Deadmines bosses spawned (>=6 distinct placed entries) [V]",
        bosses.len() as i64,
    )?;
    // BOTH named drops, DISTINCT item ids — two rows of one item must not pass vacuously.
    let named_loot: BTreeSet<i64> = a
        .values("SELECT item_entry FROM game_creature_loot WHERE item_entry=5191 OR item_entry=5196")?
        .into_iter()
        .collect();
    a.chk(
        "FLOOR_DEADMINES_NAMED_LOOT",
        "Deadmines named drops (Cruel Barb ~5191 + Smite's Mighty Hammer ~5196) [V]",
        named_loot.len() as i64,
    )?;
    a.chk_count(
        "FLOOR_PICKPOCKET_LOOT",
        "pickpocket loot rows (game_pickpocket_loot) [V]",
        "SELECT id FROM game_pickpocket_loot",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_SKINNING_LOOT",
        "skinning loot rows (game_skinning_loot) [V]",
        "SELECT id FROM game_skinning_loot",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_GAMEOBJECT_CHEST_LOOT",
        "gameobject (chest) loot rows (game_gameobject_loot) [V]",
        "SELECT id FROM game_gameobject_loot",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_FISHING_LOOT",
        "fishing loot rows (game_fishing_loot) [V]",
        "SELECT id FROM game_fishing_loot",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_CREATURES_WITH_PICKPOCKET",
        "creatures with a pickpocket table (creature_template.pickpocket_loot_id>0) [V]",
        "SELECT entry FROM game_creature_template WHERE pickpocket_loot_id > 0",
        RowMatch::Numeric,
    )?;
    a.chk_count(
        "FLOOR_CREATURES_WITH_SKIN",
        "creatures with a skin table (creature_template.skin_loot_id>0) [V]",
        "SELECT entry FROM game_creature_template WHERE skin_loot_id > 0",
        RowMatch::Numeric,
    )?;

    if a.failed > 0 {
        return Err(Error::Process(format!(
            "{} assertion(s) came in under their floor — see the FAIL lines above. A floor that \
             is wrong for YOUR dump is tuned in {}; a family that imported nothing is a real \
             regression and must not be tuned away.",
            a.failed,
            ProjectLayout::IMPORT_MANIFEST_SCRIPT
        )));
    }
    println!("   OK — all 1-20 blockers present");
    Ok(())
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

    /// A checkout with the import tooling present: the two driven scripts, the floors manifest,
    /// and the staged dump the (fake) pull would have assembled.
    fn checkout(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let dir = tmp.path().join(ProjectLayout::IMPORTER_SCRIPTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "pull-classic-db.sh",
            "import-class-spells.sh",
            "import-manifest.sh",
        ] {
            std::fs::write(dir.join(name), "#!/usr/bin/env bash\n").unwrap();
        }
        let import_dir = tmp.path().join(".import");
        std::fs::create_dir_all(&import_dir).unwrap();
        std::fs::write(import_dir.join("classic-db-full.sql"), "-- dump\n").unwrap();
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

    /// Every consumed floor at 1 — the manifest a healthy sourcing yields, in fake form.
    fn floors_of_one() -> String {
        CONSUMED_FLOORS
            .iter()
            .map(|key| format!("{key}=1"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One `spacetime sql` answer that satisfies every floors-of-one assertion shape at once:
    /// counted rows, a 6-digit guid, values inside both quest-level bands, and one
    /// creature-template row whose npc_flags advertise all three audited services (0x10|0x4|0x2).
    const SQL_ROWS: &str = " 7 \n 12 \n 123456 \n 3 \n 7 | 22 | \"Bob\" \n";

    /// A machine on which the whole flow succeeds: the manifest sources, every query answers.
    fn healthy() -> FakeStack {
        FakeStack::new()
            .with_stdout("import-manifest.sh", &floors_of_one())
            .with_stdout("spacetime sql", SQL_ROWS)
    }

    fn rendered(stack: &FakeStack) -> Vec<String> {
        stack.rendered()
    }

    fn pos(calls: &[String], needle: &str) -> usize {
        calls
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("{needle} was never run: {calls:?}"))
    }

    // ---- consent ----

    #[test]
    fn without_yes_or_accept_nothing_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);

        for answer in ["", "no", "n", "y", "sure", "YES please", "1"] {
            let stack = healthy();
            let prompt = ScriptedPrompt::new(&[answer]);
            let options = ImportOptions {
                accept: false,
                client_data: Some(data.to_string_lossy().to_string()),
            };
            let error = run_world(&project, &stack.runner(), &prompt, &options).unwrap_err();
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
            let stack = healthy();
            let prompt = ScriptedPrompt::new(&[answer]);
            let options = ImportOptions {
                accept: false,
                client_data: Some(data.to_string_lossy().to_string()),
            };
            run_world(&project, &stack.runner(), &prompt, &options).unwrap();
            assert!(
                rendered(&stack).iter().any(|c| c.contains("--dump")),
                "{answer:?} should have run the ETL"
            );
        }
    }

    #[test]
    fn accept_answers_the_question_so_no_terminal_is_needed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();
        // No answers at all: this prompt errors if it is asked anything.
        let prompt = ScriptedPrompt::new(&[]);

        run_world(&project, &stack.runner(), &prompt, &accepted(&data)).unwrap();
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

    /// The destination/profile pair of every dump importer invocation, in order.
    fn dump_destinations(stack: &FakeStack) -> Vec<(String, String)> {
        stack
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Stream(spec) | Call::Wait(spec) => {
                    let args = spec.args();
                    args.iter().any(|a| a == "--dump").then(|| {
                        let profile = args
                            .windows(2)
                            .find(|pair| pair[0] == "--world-profile")
                            .map(|pair| pair[1].clone())
                            .expect("dump command has no profile");
                        (args[1].clone(), profile)
                    })
                }
                _ => None,
            })
            .collect()
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
    fn topology_assigns_each_content_destination_its_canonical_profile_once() {
        for (topology, want) in [
            (
                Topology::Single,
                vec![(ProjectLayout::DATABASE, "alliance-single")],
            ),
            (
                Topology::Sharded,
                vec![
                    (ProjectLayout::DATABASE, "alliance-eastern"),
                    (ProjectLayout::KALIMDOR_SHARD, "alliance-kalimdor"),
                    (ProjectLayout::INSTANCE_POOL, "instances"),
                ],
            ),
        ] {
            let want: Vec<(String, String)> = want
                .into_iter()
                .map(|(shard, profile)| (shard.to_string(), profile.to_string()))
                .collect();
            let tmp = TempDir::new().unwrap();
            let project = checkout(&tmp);
            let data = client_data(&tmp);
            set_topology(&project, topology);
            let stack = healthy();

            run_world(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap();

            let world = dump_destinations(&stack);
            assert_eq!(world, want, "{topology:?}");
            let expected_shards: Vec<String> =
                want.iter().map(|(shard, _)| shard.clone()).collect();
            let spells: Vec<String> = imported_databases(&stack)
                .into_iter()
                .filter(|(script, _)| script == "import-class-spells.sh")
                .map(|(_, db)| db)
                .collect();
            assert_eq!(spells, expected_shards, "{topology:?}");
        }
    }

    #[test]
    fn the_instance_pool_skips_open_world_geometry_but_keeps_global_modes() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let pool = world_etl_commands(
            &project,
            &data,
            ImportDestination {
                shard: ProjectLayout::INSTANCE_POOL,
                profile: WorldProfile::Instances,
            },
        );
        let rendered: Vec<String> = pool
            .iter()
            .map(|(_, _, command)| command.render())
            .collect();
        assert!(rendered.iter().any(|command| {
            command.contains("--dump") && command.contains("--world-profile instances")
        }));
        assert!(!rendered.iter().any(|command| command.contains("--terrain")));
        assert!(!rendered.iter().any(|command| command.contains("--nav")));
        for mode in ["--talents", "--spells"] {
            assert!(rendered.iter().any(|command| command.contains(mode)));
        }
        let spells = pool
            .iter()
            .find(|(what, _, _)| what.contains("class spells"))
            .map(|(_, _, c)| c)
            .expect("the pool's mode list lost the class-spell overlay");
        assert_eq!(spells.env_value("DB"), Some(ProjectLayout::INSTANCE_POOL));
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
        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["no"]);
        let options = ImportOptions {
            accept: false,
            client_data: Some(data.to_string_lossy().to_string()),
        };
        let error = run_world(&project, &stack.runner(), &prompt, &options)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Nothing was fetched"), "{error}");
    }

    #[test]
    fn a_missing_terminal_is_a_refusal_not_a_default_yes() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();
        let prompt = ScriptedPrompt::new(&[]); // asking errors, like a headless run
        let options = ImportOptions {
            accept: false,
            client_data: Some(data.to_string_lossy().to_string()),
        };
        assert!(run_world(&project, &stack.runner(), &prompt, &options).is_err());
        assert!(stack.calls().is_empty());
    }

    // ---- stage order and argv shapes ----

    #[test]
    fn the_importer_modes_run_in_the_bash_flows_order_with_its_exact_flags() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        // One destination, so the expected list below stays exact.
        set_topology(&project, Topology::Single);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let importer = project.importer_bin().to_string_lossy().to_string();
        let data = std::fs::canonicalize(&data).unwrap();
        let data = data.to_string_lossy();
        let db = ProjectLayout::DATABASE;
        let server = ProjectLayout::stdb_uri();
        let expected = vec![
            format!(
                "{importer} --db {db} --server {server} --dump {DUMP_PATH} --dbc {data} --world-profile alliance-single --include-creatures {INCLUDE_CREATURES} --apply"
            ),
            format!("{importer} --db {db} --server {server} --terrain {data} --world-profile alliance-single --apply"),
            format!("{importer} --db {db} --server {server} --nav {data} --world-profile alliance-single --apply"),
            format!("{importer} --db {db} --server {server} --dbc {data} --apply"),
            format!("{importer} --db {db} --server {server} --dbc {data} --talents --apply"),
            format!("{importer} --db {db} --server {server} --dbc {data} --spells --apply"),
            format!("{importer} --db {db} --server {server} --dbc {data} --spells --apply --only {CASTER_SPELL_IDS}"),
        ];
        let got: Vec<String> = rendered(&stack)
            .into_iter()
            .filter(|c| c.starts_with(&importer))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn the_flow_orders_floors_pull_build_etl_spells_repair_assertions() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let calls = rendered(&stack);
        // The floors are read FIRST — a drifted manifest must fail before the half-hour clone.
        let order = [
            "import-manifest.sh",
            "pull-classic-db.sh",
            "cargo build",
            "--dump",
            "--terrain",
            "--nav",
            "--talents",
            "import-class-spells.sh",
            "--only",
            "debug_repair_after_publish",
            "arm_all_pools",
            "spacetime sql",
        ];
        for pair in order.windows(2) {
            assert!(
                pos(&calls, pair[0]) < pos(&calls, pair[1]),
                "{} must run before {}: {calls:?}",
                pair[0],
                pair[1]
            );
        }
        // ...and the curated overlay sits between the full Spell.dbc import and the caster
        // allowlist, exactly like the bash flow.
        assert!(pos(&calls, "--spells --apply") < pos(&calls, "import-class-spells.sh"));
        // The long children stream; the bookkeeping ones are captured.
        for call in stack.calls() {
            match call {
                Call::Stream(spec) | Call::Wait(spec) => {
                    assert_eq!(spec.cwd_value(), Some(project.root.as_path()), "{}", spec.render());
                }
                other => panic!("unexpected call kind: {other:?}"),
            }
        }
    }

    #[test]
    fn the_databases_import_one_at_a_time_each_completed_before_the_next() {
        // Per database and INTERLEAVED: modes, re-arm, floors — then the next database. Not
        // all-modes-then-all-floors: a failure part way must leave one COMPLETE database rather
        // than two half ones, the order the bash flow was run in per database.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        set_topology(&project, Topology::Sharded);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let calls = rendered(&stack);
        // The default shard's floors assert BEFORE the instance pool's first mode runs. The
        // trailing space matters: "lyracore " must not match "lyracore-instances".
        let sql_prefix = format!(
            "spacetime sql --server {} {} ",
            ProjectLayout::stdb_uri(),
            ProjectLayout::DATABASE
        );
        let world_floors = calls
            .iter()
            .position(|c| c.starts_with(&sql_prefix))
            .expect("the default shard's floors never ran");
        let pool_first_mode = calls
            .iter()
            .position(|c| c.contains("--dump") && c.contains(ProjectLayout::INSTANCE_POOL))
            .expect("the instance pool's ETL never ran");
        assert!(world_floors < pool_first_mode, "{calls:?}");
    }

    #[test]
    fn import_world_never_runs_the_retired_bash_orchestrator() {
        // The headline of #104: the ORCHESTRATION is native now. import-world.sh stays in the
        // checkout as the by-hand advanced path, and this flow must not touch it.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        assert!(
            !rendered(&stack).iter().any(|c| c.contains("import-world.sh")),
            "{:?}",
            rendered(&stack)
        );
    }

    #[test]
    fn the_scripts_are_addressed_by_their_shipped_paths_and_run_through_bash() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let specs: Vec<CommandSpec> = stack
            .calls()
            .into_iter()
            .map(|c| match c {
                Call::Stream(spec) | Call::Wait(spec) => spec,
                other => panic!("unexpected call kind: {other:?}"),
            })
            .collect();
        for (needle, shipped) in [
            ("pull-classic-db.sh", ProjectLayout::PULL_CLASSIC_DB_SCRIPT),
            (
                "import-class-spells.sh",
                ProjectLayout::IMPORT_CLASS_SPELLS_SCRIPT,
            ),
        ] {
            let spec = specs
                .iter()
                .find(|s| s.render().contains(needle))
                .unwrap_or_else(|| panic!("{needle} never ran"));
            assert_eq!(spec.program(), "bash");
            assert!(spec.args()[0].ends_with(shipped), "{}", spec.render());
        }
        // The manifest is SOURCED (bash -c), never executed as a script of its own.
        let floors = specs
            .iter()
            .find(|s| s.render().contains("import-manifest.sh"))
            .expect("the floors were never read");
        assert_eq!(floors.program(), "bash");
        assert_eq!(floors.args()[0], "-c");
        // ...with the canonical-run floor selection pinned, not inherited.
        assert_eq!(floors.env_value("MAP"), Some("0"));
        assert_eq!(floors.env_value("SLICE"), Some("0"));
    }

    #[test]
    fn the_client_path_and_the_database_reach_every_mode_that_needs_them() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        // One database: the per-call assertions below name the fixture database explicitly; the
        // per-database delivery is `a_sharded_fixture_imports_the_instance_pool_too...`'s job.
        set_topology(&project, Topology::Single);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let canonical = std::fs::canonicalize(&data).unwrap();
        let canonical = canonical.to_string_lossy().to_string();
        let importer = project.importer_bin().to_string_lossy().to_string();

        for call in stack.calls() {
            let (Call::Stream(spec) | Call::Wait(spec)) = call else {
                panic!("unexpected call kind")
            };
            let render = spec.render();
            // Every importer mode names the target database explicitly — an unset --db silently
            // writes to the importer's own default.
            if render.starts_with(&importer) {
                assert_eq!(&spec.args()[..2], &["--db", ProjectLayout::DATABASE]);
                assert!(render.contains(&canonical), "{render}");
            }
            // Every spacetime query/call is pinned to the loopback node (#440) and the fixture
            // database.
            if render.starts_with("spacetime") {
                assert!(
                    render.contains(&format!("--server {}", ProjectLayout::stdb_uri())),
                    "{render}"
                );
                assert!(render.contains(ProjectLayout::DATABASE), "{render}");
            }
            // The class-spell overlay takes the client path as its argument and the database from
            // env — both explicit, because its DB default is how spells once landed elsewhere.
            if render.contains("import-class-spells.sh") {
                assert_eq!(spec.args().last().map(String::as_str), Some(canonical.as_str()));
                assert_eq!(spec.env_value("DB"), Some(ProjectLayout::DATABASE));
            }
        }
    }

    #[test]
    fn every_destination_child_names_its_shard_and_loopback_endpoint() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        set_topology(&project, Topology::Sharded);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let importer = project.importer_bin().to_string_lossy().to_string();
        let server = ProjectLayout::stdb_uri();
        let shards = [
            ProjectLayout::DATABASE,
            ProjectLayout::KALIMDOR_SHARD,
            ProjectLayout::INSTANCE_POOL,
        ];
        for call in stack.calls() {
            let (Call::Stream(spec) | Call::Wait(spec)) = call else {
                panic!("unexpected call kind")
            };
            let render = spec.render();
            if render.starts_with(&importer) {
                assert!(shards.contains(&spec.args()[1].as_str()), "{render}");
                assert_eq!(&spec.args()[2..4], &["--server", server.as_str()]);
            }
            if render.contains("import-class-spells.sh") {
                assert!(shards.contains(&spec.env_value("DB").unwrap_or("")));
                assert_eq!(spec.env_value("SPACETIME_SERVER"), Some(server.as_str()));
            }
            if render.starts_with("spacetime") {
                assert!(render.contains(&format!("--server {server}")), "{render}");
                assert!(
                    shards.contains(&spec.args().get(3).map(String::as_str).unwrap_or_default()),
                    "{render}"
                );
            }
        }
        let commands = rendered(&stack);
        for shard in shards {
            for reducer in ["debug_repair_after_publish", "arm_all_pools"] {
                assert_eq!(
                    commands
                        .iter()
                        .filter(|command| {
                            command.as_str()
                                == format!("spacetime call --server {server} {shard} {reducer}")
                        })
                        .count(),
                    1,
                    "{reducer} must run once on {shard}"
                );
            }
            let shard_imports: Vec<&String> = commands
                .iter()
                .filter(|command| {
                    command.starts_with(&importer)
                        && command.contains(&format!("--db {shard} --server"))
                })
                .collect();
            assert!(
                shard_imports
                    .iter()
                    .any(|command| command.contains("--talents")),
                "{shard}"
            );
            assert!(
                shard_imports
                    .iter()
                    .any(|command| command.contains("--spells")),
                "{shard}"
            );
        }
    }

    #[test]
    fn the_pull_is_not_told_to_skip_its_own_checksum_verification() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();

        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();

        let calls = rendered(&stack);
        assert!(
            !calls[pos(&calls, "pull-classic-db.sh")].contains("--skip-verify"),
            "the pinned-commit checksum is the drift guard; the flow must not disable it"
        );
    }

    #[test]
    fn a_pull_that_stages_no_dump_fails_before_the_etl_names_the_path() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::remove_file(project.root.join(DUMP_PATH)).unwrap();
        let data = client_data(&tmp);
        let stack = healthy();

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(DUMP_PATH), "{error}");
        assert!(
            !rendered(&stack).iter().any(|c| c.contains("--dump")),
            "the ETL must not start without its staged input: {:?}",
            rendered(&stack)
        );
    }

    // ---- the client path ----

    #[test]
    fn without_the_flag_the_path_is_asked_for_after_consent_and_after_the_pull() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);

        run_world(&project, &stack.runner(), &prompt, &ImportOptions::default()).unwrap();

        let asked = prompt.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].contains("Proceed"), "{asked:?}");
        assert!(asked[1].contains("Path"), "{asked:?}");
    }

    #[test]
    fn a_bad_client_path_is_refused_before_the_licence_notice_is_even_answered() {
        // A typo in --client-data must not cost somebody a half-hour clone first.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = healthy();
        let options = ImportOptions {
            accept: true,
            client_data: Some(tmp.path().join("nope").to_string_lossy().to_string()),
        };
        let error = run_world(
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
        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["yes", ""]);
        let error = run_world(&project, &stack.runner(), &prompt, &ImportOptions::default())
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

        let stack = healthy();
        run_world(
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

        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["yes"]); // asking for a path would error: no 2nd answer
        run_world(&project, &stack.runner(), &prompt, &ImportOptions::default()).unwrap();

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
        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);

        run_world(&project, &stack.runner(), &prompt, &ImportOptions::default()).unwrap();

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

        let stack = healthy();
        let prompt = ScriptedPrompt::new(&["yes", &data.to_string_lossy()]);
        run_world(&project, &stack.runner(), &prompt, &ImportOptions::default()).unwrap();

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
    fn each_modes_failure_names_the_mode_keeps_the_childs_words_and_stops_the_run() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);

        // (what fails, its own words, what the error must name, a mode that must never start)
        for (needle, child_says, names, never_started) in [
            (
                "pull-classic-db.sh",
                "sha256 MISMATCH",
                "pulling classic-db",
                "--dump",
            ),
            ("--terrain", "self-check bail", "terrain", "--nav"),
            (
                "import-class-spells.sh",
                "requested 31 ids but matched 30",
                "class spells",
                "--only",
            ),
        ] {
            let stack = healthy().fail_on(needle, child_says);
            let error = run_world(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap_err()
            .to_string();

            assert!(error.contains(names), "{error}");
            assert!(error.contains(child_says), "{error}");
            assert!(
                !rendered(&stack).iter().any(|c| c.contains(never_started)),
                "a failed mode must stop the run before {never_started}: {:?}",
                rendered(&stack)
            );
        }
    }

    #[test]
    fn a_destination_failure_names_its_profile_and_stops_before_later_destinations() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        set_topology(&project, Topology::Sharded);
        let stack = healthy().fail_on(
            "--world-profile alliance-kalimdor",
            "profile input did not match",
        );

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains(ProjectLayout::KALIMDOR_SHARD), "{error}");
        assert!(error.contains("alliance-kalimdor"), "{error}");
        assert!(error.contains("world content"), "{error}");
        assert!(error.contains("profile input did not match"), "{error}");
        assert!(
            !rendered(&stack)
                .iter()
                .any(|command| command.contains(ProjectLayout::INSTANCE_POOL)),
            "later destinations must not start after a failure"
        );
    }

    #[test]
    fn a_failed_repair_call_is_loud_not_swallowed_like_the_bash() {
        // The bash flow piped both re-arm calls to /dev/null and ignored their exit status; an
        // unarmed creature tick then only surfaces in play. The port checks them on purpose.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy().fail_on("debug_repair_after_publish", "operator only");

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("debug_repair_after_publish"), "{error}");
        assert!(error.contains("operator only"), "{error}");
    }

    #[test]
    fn a_checkout_without_the_import_tooling_says_so_before_consent_is_spent() {
        for missing in [
            "pull-classic-db.sh",
            "import-class-spells.sh",
            "import-manifest.sh",
        ] {
            let tmp = TempDir::new().unwrap();
            let project = checkout(&tmp);
            std::fs::remove_file(
                project
                    .root
                    .join(ProjectLayout::IMPORTER_SCRIPTS_DIR)
                    .join(missing),
            )
            .unwrap();
            let data = client_data(&tmp);
            let stack = healthy();

            let error = run_world(
                &project,
                &stack.runner(),
                &ScriptedPrompt::new(&[]),
                &accepted(&data),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(missing), "{error}");
            assert!(stack.calls().is_empty());
        }
    }

    // ---- the floors ----

    #[test]
    fn a_manifest_missing_a_consumed_floor_fails_before_anything_expensive_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let partial: String = floors_of_one()
            .lines()
            .filter(|l| !l.starts_with("FLOOR_NAV_CHUNKS"))
            .collect::<Vec<_>>()
            .join("\n");
        let stack = FakeStack::new()
            .with_stdout("import-manifest.sh", &partial)
            .with_stdout("spacetime sql", SQL_ROWS);

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("FLOOR_NAV_CHUNKS"), "{error}");
        assert!(error.contains("import-manifest.sh"), "{error}");
        // Only the sourcing ran: no pull, no ETL, no consented half-hour wasted first.
        assert_eq!(stack.calls().len(), 1, "{:?}", rendered(&stack));
    }

    #[test]
    fn floors_under_their_minimums_fail_the_import_after_reporting_every_check() {
        // The silent-success failure mode: an ETL that wrote nothing while every stage exited 0.
        // Every count reads 0 here, so every floor of 1 must FAIL — loudly, and only after the
        // whole picture printed (the bash `fail=1` bookkeeping, not a first-FAIL abort).
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new().with_stdout("import-manifest.sh", &floors_of_one());

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        let error = error.to_string();
        assert!(error.contains("under their floor"), "{error}");
        // Every assertion ran — a first-FAIL abort would hide the real regression's shape.
        let queries = rendered(&stack)
            .iter()
            .filter(|c| c.starts_with("spacetime sql"))
            .count();
        assert!(queries > 40, "only {queries} queries ran");
    }

    #[test]
    fn a_failed_verification_query_aborts_rather_than_reading_as_zero_rows() {
        // The #440 discipline: one dead connection must not become dozens of fake "table is
        // empty" FAILs.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy().fail_on("spacetime sql", "Unable to connect");

        let error = run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("NOT zero rows"), "{error}");
        assert!(error.contains("Unable to connect"), "{error}");
        assert!(!error.contains("under their floor"), "{error}");
    }

    #[test]
    fn every_consumed_floor_is_asserted_on_a_healthy_run() {
        // The healthy fixture defines EXACTLY the consumed floors, so this run passing proves the
        // assertion pass consumes no key outside CONSUMED_FLOORS — the same "every consumed key
        // is defined" contract the core repo's import-manifest-smoke.sh pins for the bash.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = healthy();
        run_world(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            &accepted(&data),
        )
        .unwrap();
    }

    // ---- row matching, the n()/q_list ports ----

    #[test]
    fn row_patterns_match_like_the_bash_greps() {
        // `^ *[0-9]` — first field numeric.
        assert!(RowMatch::Numeric.matches(" 42 "));
        assert!(!RowMatch::Numeric.matches(" guid "));
        assert!(!RowMatch::Numeric.matches("------"));
        // `[0-9]{6,}` — a six-digit run somewhere; five is not enough, template ids must not count.
        assert!(RowMatch::Guid.matches(" 123456 "));
        assert!(!RowMatch::Guid.matches(" 12345 "));
        assert!(!RowMatch::Guid.matches(" 123a45 "));
        // `^ *N *$` — the whole row is that number.
        assert!(RowMatch::Exactly(7).matches("  7  "));
        assert!(!RowMatch::Exactly(7).matches(" 77 "));
        // q_list keeps only pure-numeric rows.
        assert_eq!(numeric_values(" 7 \n x \n 12 | 3 \n 9 "), vec![7, 9]);
    }

    // ---- `import vmaps` ----

    #[test]
    fn vmaps_drive_each_world_shards_matching_profile() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run_vmaps(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            Some(&data.to_string_lossy()),
        )
        .unwrap();

        let importer = project.importer_bin().to_string_lossy().to_string();
        let canonical = std::fs::canonicalize(&data).unwrap();
        let vmap_runs: Vec<String> = rendered(&stack)
            .into_iter()
            .filter(|c| c.contains("--vmap"))
            .collect();
        assert_eq!(
            vmap_runs,
            vec![
                format!(
                    "{importer} --db {} --server {} --vmap {} --world-profile alliance-eastern --apply",
                    ProjectLayout::DATABASE,
                    ProjectLayout::stdb_uri(),
                    canonical.to_string_lossy()
                ),
                format!(
                    "{importer} --db {} --server {} --vmap {} --world-profile alliance-kalimdor --apply",
                    ProjectLayout::KALIMDOR_SHARD,
                    ProjectLayout::stdb_uri(),
                    canonical.to_string_lossy()
                ),
            ]
        );
        // The importer is built first, and everything runs from the checkout root.
        let calls = rendered(&stack);
        assert!(pos(&calls, "cargo build") < pos(&calls, "--vmap"));
        for call in stack.calls() {
            let (Call::Stream(spec) | Call::Wait(spec)) = call else {
                panic!("unexpected call kind")
            };
            assert_eq!(spec.cwd_value(), Some(project.root.as_path()));
        }
    }

    #[test]
    fn vmaps_skip_the_instance_pool() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();

        run_vmaps(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            Some(&data.to_string_lossy()),
        )
        .unwrap();

        assert!(
            !rendered(&stack)
                .iter()
                .filter(|command| command.contains("--vmap"))
                .any(|command| command.contains(ProjectLayout::INSTANCE_POOL)),
            "{:?}",
            rendered(&stack)
        );
    }

    #[test]
    fn vmaps_respects_a_recorded_single_topology() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        RuntimeState {
            topology: "single".to_string(),
            ..Default::default()
        }
        .save(&project.state_file())
        .unwrap();
        let stack = FakeStack::new();

        run_vmaps(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            Some(&data.to_string_lossy()),
        )
        .unwrap();

        let vmap_runs = rendered(&stack)
            .iter()
            .filter(|c| c.contains("--vmap"))
            .count();
        assert_eq!(vmap_runs, 1);
    }

    #[test]
    fn vmaps_has_no_consent_gate_because_nothing_is_fetched() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&[]); // any question at all would error

        run_vmaps(
            &project,
            &stack.runner(),
            &prompt,
            Some(&data.to_string_lossy()),
        )
        .unwrap();
        assert!(prompt.asked().is_empty());
    }

    #[test]
    fn vmaps_shares_the_config_fallback_chain_with_world() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        crate::config::Config {
            client_data: Some(data.to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();
        let stack = FakeStack::new();
        let prompt = ScriptedPrompt::new(&[]);

        run_vmaps(&project, &stack.runner(), &prompt, None).unwrap();
        assert!(prompt.asked().is_empty(), "a configured path must not prompt");
        assert!(rendered(&stack).iter().any(|c| c.contains("--vmap")));
    }

    #[test]
    fn vmaps_refuses_a_bad_client_path_before_running_anything() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new();

        let error = run_vmaps(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            Some(&tmp.path().join("nope").to_string_lossy()),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.calls().is_empty());
    }

    #[test]
    fn a_failed_vmap_run_keeps_the_childs_words_and_says_what_to_check() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        let stack = FakeStack::new().fail_on("--vmap", "no MCNK cells intersected the slice");

        let error = run_vmaps(
            &project,
            &stack.runner(),
            &ScriptedPrompt::new(&[]),
            Some(&data.to_string_lossy()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no MCNK cells"), "{error}");
        assert!(error.contains("model.MPQ"), "{error}");
        assert!(error.contains(ProjectLayout::DATABASE), "{error}");
        assert!(error.contains("alliance-eastern"), "{error}");
    }
}
