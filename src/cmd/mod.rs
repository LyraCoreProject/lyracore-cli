pub mod account;
pub mod character;
pub mod client;
pub mod config;
pub mod dev;
pub mod doctor;
mod gateway_log;
pub mod import;
pub mod packages;
pub mod preflight;
pub mod production;
pub mod publish;
pub mod service;
pub mod update;

use crate::project::{ClientBind, Component, Topology};
use crate::{Error, Result};
use account::PasswordSource;
use import::{ImportOptions, VmapOptions};

/// The default help: just enough to get a newcomer from a fresh checkout to a running server.
/// Everything else — the commands you only need once you're developing LyraCore itself — lives in
/// [`USAGE_ALL`], reached with `lyracore help --all`.
pub const USAGE: &str = "\
lyracore — run a local LyraCore vanilla WoW server

USAGE:
  lyracore doctor          check whether your machine has what `dev up` needs
  lyracore dev up          start the local server (or reconnect to one already running)
  lyracore account create USER
                           create a login account so you can connect a 1.12.1 client
  lyracore dev status      show whether the server is running
  lyracore dev down        stop the server
  lyracore import          replace the placeholder starter data with the real game
                           world — every Alliance early-game corridor — from public
                           game data and your own 1.12.1 client

Run `lyracore help --all` to see every command, including the ones for working on
LyraCore itself.";

/// The full command surface, including the contributor-facing corners `USAGE` leaves out.
pub const USAGE_ALL: &str = "\
lyracore — local LyraCore development

USAGE:
  lyracore doctor                              check whether your machine is ready for `dev up`
  lyracore preflight                           the offline deploy gate: build, schema, filters
  lyracore publish [DATABASE ...]              preflight, then publish the module. With no
                                               names, publishes every database of the active
                                               fixture topology (the default sharded topology
                                               if none is recorded); named databases are
                                               published exactly as given. Takes database
                                               NAMES only — SpacetimeDB's data-wiping -c flag
                                               is deliberately not exposed here
  lyracore publish --skip-preflight [DB ...]   publish without running the offline checks
                                               (preflight) first
  lyracore dev up                              start (or reuse) the local realm: four databases
                                               — Eastern Kingdoms, Kalimdor, the instance pool,
                                               and realm-core
  lyracore dev up --single                     a one-database setup instead, for a quicker
                                               local test — skips the multi-database sharded
                                               configuration
  lyracore dev up --lan <IP>                   also serve clients on this machine's private-LAN
                                               address (SpacetimeDB stays on loopback)
  lyracore dev status                          report each component's state, and each
                                               database's own published/connected verdict
  lyracore dev logs [spacetime|gateway]        show a component's log file
  lyracore dev smoke                           run the pinned wire harness's login smoke
  lyracore dev down [--forget]                 stop the processes this CLI started
  lyracore account create USER [--password-stdin]
                                               provision an account's SRP6 credentials
  lyracore import [--client-data PATH]         build the REAL world in place of the seed
                                               fixture: every Alliance early-game corridor
                                               in Eastern Kingdoms and Kalimdor, plus the
                                               instance maps. Pulls cmangos' classic-db,
                                               reads your own 1.12.1 client's Data/ archives,
                                               drives the importer and asserts the FLOOR_*
                                               import floors on every destination. Asks for
                                               consent first, every time
  lyracore import world --profile-shard PROFILE=SHARD
                                               explicitly assign all three sharded World Import
                                               Profiles on an external realm; repeat for
                                               alliance-eastern, alliance-kalimdor, and instances
  lyracore import world --accept               the same command by its full name (`import`
                                               is its alias), with the consent answered in
                                               advance (scripted runs)
  lyracore import vmaps [--client-data PATH]   exact model/WMO collision triangles for each
                                               populated World Shard, including Kalimdor,
                                               read from your own client's archives — nothing
                                               is fetched, so there is no consent gate
  lyracore config                              show the persisted client-data path (or \"(unset)\")
  lyracore config set client-data PATH         validate and remember your 1.12.1 client's Data/
                                               directory, so `import` and `doctor` stop asking
  lyracore client sync                         pack patch-3.MPQ and every enabled Package's
                                               addons, then install them into the configured
                                               client-data path. Warns (best-effort) about an
                                               addon a disabled or removed Package left behind
  lyracore character gm NAME true|false        grant (true) or revoke (false) GM level for a
                                               character — tries every world shard in turn
  lyracore packages add FOLDER|GIT-URL|NAME [--yes]
                                               install a Package from a folder on this machine,
                                               from a repository whose root is one Package, or by
                                               bare NAME from the Official Package Collection:
                                               copies it into packages/, records where it came from
                                               (and the exact commit, for a Git Package Source or
                                               the collection), prints a deterministic review of
                                               the tables, reducers, hooks, addons and client
                                               overrides it registers, asks before copying, then
                                               runs preflight. It never publishes — it prints the
                                               steps left
  lyracore packages update [NAME] [--yes]      advance a Git-backed Package to the repository's
                                               current commit, or every Git-backed Package when no
                                               name is given. Refuses a folder that has drifted
                                               from its recorded content identity, reviews and asks
                                               before replacing it, and keeps the previous revision
                                               until the new one preflights
  lyracore packages enable NAME                move a disabled Package back into packages/, where
                                               the build compiles it again. Its provenance stamp
                                               travels with the folder
  lyracore packages disable NAME               move an enabled Package into .lyracore/packages-
                                               disabled/, out of the build's sight but still on
                                               disk. Reports the Module tables it registers first,
                                               because publishing without them is a schema change
  lyracore packages remove NAME [--yes]        delete a DISABLED Package from this checkout. Asks
                                               first, and refuses a folder that no longer matches
                                               its recorded content identity, because those local
                                               changes exist nowhere else
  lyracore packages replay [DATABASE ...] [--check] [--yes] [--force-all]
                                               reapply every enabled Package's Delta to each named
                                               Shard: reimport Spell.dbc, then replay the claims
                                               over it. Preflights every artifact and every target
                                               before the first write, applies Shard by Shard, and
                                               stops at the first failure naming what completed,
                                               what failed and what was never touched. A Shard
                                               whose recorded provenance already matches this
                                               checkout is skipped, so re-running resumes. --check
                                               prints the plan and writes nothing; --force-all
                                               replays even the Shards that match
  lyracore packages config NAME [KEY [VALUE]] [--new]
                                               read and write an installed Package's Package Config
                                               — the durable key-values it reads at runtime. With no
                                               KEY it lists every key; with a KEY it prints that
                                               value; with a VALUE it writes the key to EVERY Shard
                                               of the fixture topology, because the rows are
                                               per-Shard state. A read reports a key the Shards
                                               disagree about rather than picking one answer. --new
                                               creates a key the Package never seeded; without it
                                               the Module refuses an unknown key and names the ones
                                               it does have
  lyracore packages build                      regenerate the Module schema typings into
                                               datascripts/generated/, install the pinned Bun
                                               dependencies from datascripts/bun.lock, then
                                               typecheck every Datascript against them. Author-side
                                               only: applying a prebuilt Package needs no Bun
  lyracore packages check                      verify every enabled Package's generated artifact
                                               against its recorded Build Identity: the Datascript
                                               source, generated typings, Base Snapshot, authoring
                                               library and toolchain pins it was built from.
                                               Regenerates the typings fresh, so a Module schema
                                               change is caught even on a clean checkout. Refuses
                                               naming the specific input that changed and the
                                               rebuild command; a missing Base Snapshot is reported
                                               unverifiable rather than failing. `preflight` (and so
                                               `publish`) runs the same check
  lyracore packages list                       every installed Package: enabled or disabled, its
                                               Package Source, its recorded content identity,
                                               whether the installed copy has drifted from it,
                                               and what it registers
  lyracore packages new NAME                   scaffold from the maintained reference Package in
                                               this checkout (packages/example/), without fetching
                                               a template. Refuses enabled/disabled collisions,
                                               then runs ordinary preflight (whose Cargo checks may
                                               use its configured cache/network). No client content;
                                               the printed next steps explain how to add it
  lyracore production status --server URI --gateway-log PATH --realm-core DB DATABASE ...
                                               read-only production topology, schema, connection,
                                               realm-core and listener verdicts
  lyracore service reconcile                   make a PRODUCTION host's spacetimedb-standalone
                                               service match the unit tracked in the checkout.
                                               Root only: it updates the checkout to origin/main
                                               (refusing over a dirty working tree, as `update`
                                               does), checks the host prerequisites the unit
                                               names, refuses when another active service already
                                               owns the same data directory or listen address,
                                               installs the unit into /etc/systemd/system, reloads
                                               systemd, enables and restarts the node, then
                                               verifies its ActiveState, LimitNOFILE and stderr
                                               destination. It never creates, moves or deletes the
                                               node's data directory
  lyracore update                              pull the latest checkout in place and restart the
                                               local dev stack (refuses over a dirty working tree)

The password is read from stdin with --password-stdin, otherwise from a hidden terminal
prompt. It is never passed as a command-line argument.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Doctor,
    Preflight,
    Publish {
        /// The database names typed on the command line. Empty means none were — `publish::run`
        /// resolves that against the recorded topology once it has discovered the project.
        databases: Vec<String>,
        skip_preflight: bool,
    },
    DevUp {
        bind: ClientBind,
        topology: Topology,
    },
    DevStatus,
    DevLogs(Option<Component>),
    DevSmoke,
    DevDown {
        forget: bool,
    },
    AccountCreate {
        user: String,
        source: PasswordSource,
    },
    ImportWorld(ImportOptions),
    ImportVmaps {
        options: VmapOptions,
    },
    ConfigShow,
    ConfigSetClientData {
        path: String,
    },
    ClientSync,
    CharacterGm {
        name: String,
        enabled: bool,
    },
    PackagesAdd {
        /// The Package Source to install, as typed: a folder on this machine, a Git URL, or a bare
        /// name to resolve against the Official Package Collection. `packages::add` decides which
        /// and resolves it there, the earliest point the checkout it is being installed into is
        /// known.
        source: String,
        /// `--yes`: the install confirmation was answered in advance (scripted runs).
        yes: bool,
    },
    PackagesUpdate {
        /// The Package to advance. `None` means every Git-backed one.
        name: Option<String>,
        /// `--yes`: the update confirmation was answered in advance (scripted runs).
        yes: bool,
    },
    PackagesList,
    PackagesNew {
        name: String,
    },
    PackagesBuild,
    PackagesCheck,
    PackagesEnable {
        name: String,
    },
    PackagesDisable {
        name: String,
    },
    PackagesRemove {
        name: String,
        /// `--yes`: the deletion confirmation was answered in advance (scripted runs).
        yes: bool,
    },
    PackagesReplay(packages::replay::ReplayOptions),
    PackagesConfig(packages::config::ConfigOptions),
    ProductionStatus(production::StatusOptions),
    /// The one root-only, system-state verb on this surface, deliberately its own: `update` stays
    /// a contributor's git-pull replacement and owns no service.
    ServiceReconcile,
    Update,
    Help,
    HelpAll,
}

impl Command {
    pub fn parse(args: &[String]) -> Result<Self> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Command::Help),
            ["-h"] | ["--help"] | ["help"] => Ok(Command::Help),
            ["help", "--all"] => Ok(Command::HelpAll),
            ["help", other, ..] => Err(Error::Usage(format!(
                "`lyracore help` takes no arguments other than --all (got '{other}')"
            ))),

            ["doctor"] => Ok(Command::Doctor),
            ["preflight"] => Ok(Command::Preflight),
            ["preflight", other, ..] => Err(Error::Usage(format!(
                "`preflight` takes no arguments (got '{other}')"
            ))),

            // Everything that is not the one recognised option is a database NAME — and
            // `publish::databases` is what refuses anything flag-shaped, including `-c`. An empty
            // result is not yet a default: `publish::run` resolves it against the recorded
            // topology once it has discovered the project (parsing happens before that).
            ["publish", rest @ ..] => {
                let skip_preflight = rest.contains(&"--skip-preflight");
                let names: Vec<String> = rest
                    .iter()
                    .filter(|arg| **arg != "--skip-preflight")
                    .map(|arg| (*arg).to_string())
                    .collect();
                publish::databases(&names).map(|databases| Command::Publish {
                    databases,
                    skip_preflight,
                })
            }

            ["dev", "up", rest @ ..] => parse_dev_up(rest),
            ["dev", "status"] => Ok(Command::DevStatus),
            ["dev", "smoke"] => Ok(Command::DevSmoke),
            ["dev", "down"] => Ok(Command::DevDown { forget: false }),
            ["dev", "down", "--forget"] => Ok(Command::DevDown { forget: true }),
            ["dev", "logs"] => Ok(Command::DevLogs(None)),
            ["dev", "logs", name] => Component::parse(name)
                .map(|c| Command::DevLogs(Some(c)))
                .ok_or_else(|| {
                    Error::Usage(format!(
                        "unknown component '{name}' — expected one of: spacetime, gateway"
                    ))
                }),
            ["dev"] => Err(Error::Usage(
                "`dev` needs a subcommand: up, status, logs, smoke, down".to_string(),
            )),
            ["dev", other, ..] => Err(Error::Usage(format!(
                "unknown `dev` subcommand '{other}' — expected up, status, logs, smoke, or down"
            ))),

            ["account", "create", user] => Ok(Command::AccountCreate {
                user: (*user).to_string(),
                source: PasswordSource::Tty,
            }),
            ["account", "create", user, "--password-stdin"] => Ok(Command::AccountCreate {
                user: (*user).to_string(),
                source: PasswordSource::Stdin,
            }),
            ["account", "create"] => Err(Error::Usage(
                "`account create` needs a username".to_string(),
            )),
            ["account", ..] => Err(Error::Usage(
                "`account` supports: create USER [--password-stdin]".to_string(),
            )),

            // Two verbs since #104: `world` (the full import) and `vmaps` (collision only). Bare
            // `import` is an ALIAS of `import world` rather than a separate arm: this parser is
            // literal slice-matching, so the alias is one line, the two spellings cannot drift,
            // and the command bare `import` has meant since it existed keeps meaning that.
            ["import", "world", rest @ ..] => parse_import_world(rest),
            ["import", "vmaps", rest @ ..] => parse_import_vmaps(rest),
            // ...and `import` still takes no positional arguments. A bare path is the shape
            // somebody will type first (`lyracore import /games/wow/Data`), so it is refused by
            // NAME rather than as "unknown command".
            ["import", rest @ ..] => parse_import_world(rest),

            ["config"] => Ok(Command::ConfigShow),
            ["config", "set", "client-data", path] => Ok(Command::ConfigSetClientData {
                path: (*path).to_string(),
            }),
            ["config", "set", "client-data"] => Err(Error::Usage(
                "`config set client-data` needs a path".to_string(),
            )),
            ["config", ..] => Err(Error::Usage(
                "`config` supports: (bare, to show) or set client-data PATH".to_string(),
            )),

            ["client", "sync"] => Ok(Command::ClientSync),
            ["client", "sync", other, ..] => Err(Error::Usage(format!(
                "`client sync` takes no arguments (got '{other}')"
            ))),
            ["client"] => Err(Error::Usage(
                "`client` needs a subcommand: sync".to_string(),
            )),
            ["client", other, ..] => Err(Error::Usage(format!(
                "unknown `client` subcommand '{other}' — expected sync"
            ))),

            // Only `gm` today, but the catch-all below names it as ONE arm among future
            // `character` verbs rather than "unknown command" — the shape this whole surface is
            // meant to grow into.
            ["character", "gm", name, "true"] => Ok(Command::CharacterGm {
                name: (*name).to_string(),
                enabled: true,
            }),
            ["character", "gm", name, "false"] => Ok(Command::CharacterGm {
                name: (*name).to_string(),
                enabled: false,
            }),
            ["character", "gm", _name, other] => Err(Error::Usage(format!(
                "`character gm` takes true or false (got '{other}')"
            ))),
            ["character", ..] => Err(Error::Usage(
                "`character` supports: gm NAME true|false".to_string(),
            )),

            // `add` takes exactly one positional: the Package Source. `--yes` may come on either
            // side of it, like every other flag on this surface.
            ["packages", "add", rest @ ..] => parse_packages_add(rest),
            ["packages", "update", rest @ ..] => parse_packages_update(rest),
            ["packages", "list"] => Ok(Command::PackagesList),
            ["packages", "list", other, ..] => Err(Error::Usage(format!(
                "`packages list` takes no arguments (got '{other}')"
            ))),
            ["packages", "new", name] => Ok(Command::PackagesNew {
                name: (*name).to_string(),
            }),
            ["packages", "new"] => Err(Error::Usage(
                "`packages new` needs a name, e.g. `packages new my-package`".to_string(),
            )),
            ["packages", "new", _name, other, ..] => Err(Error::Usage(format!(
                "`packages new` takes one name (got a second argument: '{other}')"
            ))),
            ["packages", "replay", rest @ ..] => parse_packages_replay(rest),
            ["packages", "config", rest @ ..] => parse_packages_config(rest),
            ["packages", "build"] => Ok(Command::PackagesBuild),
            ["packages", "build", other, ..] => Err(Error::Usage(format!(
                "`packages build` takes no arguments (got '{other}')"
            ))),
            ["packages", "check"] => Ok(Command::PackagesCheck),
            ["packages", "check", other, ..] => Err(Error::Usage(format!(
                "`packages check` takes no arguments (got '{other}')"
            ))),
            ["packages", "enable", rest @ ..] => Ok(Command::PackagesEnable {
                name: parse_packages_move("enable", rest)?,
            }),
            ["packages", "disable", rest @ ..] => Ok(Command::PackagesDisable {
                name: parse_packages_move("disable", rest)?,
            }),
            ["packages", "remove", rest @ ..] => parse_packages_remove(rest),
            ["packages", ..] => Err(Error::Usage(
                "`packages` supports: add FOLDER|GIT-URL [--yes], build, check, config NAME [KEY \
                 [VALUE]] [--new], disable NAME, enable NAME, list, new NAME, remove NAME [--yes], \
                 replay [DATABASE ...] [--check] [--yes] [--force-all], update [NAME] [--yes]"
                    .to_string(),
            )),

            ["production", "status", rest @ ..] => parse_production_status(rest),
            ["production", ..] => Err(Error::Usage(
                "`production` supports: status --server URI --gateway-log PATH --realm-core DB \
                 DATABASE ..."
                    .to_string(),
            )),

            // One subverb today. The catch-alls below name it, the way `client` and `character`
            // do, rather than reporting "unknown command" for a group that exists.
            ["service", "reconcile"] => Ok(Command::ServiceReconcile),
            ["service", "reconcile", other, ..] => Err(Error::Usage(format!(
                "`service reconcile` takes no arguments (got '{other}')"
            ))),
            ["service"] => Err(Error::Usage(
                "`service` needs a subcommand: reconcile".to_string(),
            )),
            ["service", other, ..] => Err(Error::Usage(format!(
                "unknown `service` subcommand '{other}' — expected reconcile"
            ))),

            ["update"] => Ok(Command::Update),
            ["update", other, ..] => Err(Error::Usage(format!(
                "`update` takes no arguments (got '{other}')"
            ))),

            [other, ..] => Err(Error::Usage(format!("unknown command '{other}'"))),
        }
    }
}

fn parse_production_status(args: &[&str]) -> Result<Command> {
    let mut server = None;
    let mut gateway_log = None;
    let mut realm_core = None;
    let mut names = Vec::new();
    let mut rest = args;
    while let Some((head, tail)) = rest.split_first() {
        match *head {
            "--server" => match tail.split_first() {
                Some((value, after)) if !value.starts_with('-') => {
                    if server.is_some() {
                        return Err(Error::Usage(
                            "`production status --server` may be supplied only once".into(),
                        ));
                    }
                    server = Some((*value).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`production status --server` needs a nickname, host, or URL".into(),
                    ))
                }
            },
            "--gateway-log" => match tail.split_first() {
                Some((path, after)) if !path.starts_with('-') => {
                    if gateway_log.is_some() {
                        return Err(Error::Usage(
                            "`production status --gateway-log` may be supplied only once".into(),
                        ));
                    }
                    gateway_log = Some((*path).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`production status --gateway-log` needs a path".into(),
                    ))
                }
            },
            "--realm-core" => match tail.split_first() {
                Some((database, after)) if !database.starts_with('-') => {
                    if realm_core.is_some() {
                        return Err(Error::Usage(
                            "`production status --realm-core` may be supplied only once".into(),
                        ));
                    }
                    realm_core = Some((*database).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`production status --realm-core` needs a database name".into(),
                    ))
                }
            },
            option if option.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `production status` option '{option}'"
                )))
            }
            database => {
                names.push(database.to_string());
                rest = tail;
            }
        }
    }

    let server = server.ok_or_else(|| {
        Error::Usage("`production status` needs --server NICKNAME|HOST|URL".to_string())
    })?;
    let gateway_log = gateway_log
        .ok_or_else(|| Error::Usage("`production status` needs --gateway-log PATH".to_string()))?;
    let realm_core = realm_core.ok_or_else(|| {
        Error::Usage("`production status` needs --realm-core DATABASE".to_string())
    })?;
    if names.is_empty() {
        return Err(Error::Usage(
            "`production status` needs the complete production database list".to_string(),
        ));
    }
    let databases = publish::databases(&names)?;
    if databases
        .iter()
        .any(|name| databases.iter().filter(|other| *other == name).count() > 1)
    {
        return Err(Error::Usage(
            "`production status` database names must be unique".to_string(),
        ));
    }
    if !databases.contains(&realm_core) {
        return Err(Error::Usage(format!(
            "realm-core '{realm_core}' is not in the production database list"
        )));
    }
    Ok(Command::ProductionStatus(production::StatusOptions {
        server,
        gateway_log: gateway_log.into(),
        realm_core,
        databases,
    }))
}

/// `packages add FOLDER|GIT-URL|NAME [--yes]`.
///
/// `--yes` answers the install confirmation in advance, the same way `import --accept` answers the
/// consent question: a scripted install must be possible, and a prompt read from `/dev/tty` cannot
/// be answered by a pipeline. It is a separate flag from the Package Source so a path that happens
/// to start with `-` is still refused rather than read as an option.
fn parse_packages_add(args: &[&str]) -> Result<Command> {
    let mut source: Option<String> = None;
    let mut yes = false;
    for arg in args {
        match *arg {
            "--yes" => yes = true,
            option if option.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `packages add` option '{option}' — the only one is --yes"
                )))
            }
            candidate if source.is_none() => source = Some(candidate.to_string()),
            extra => {
                return Err(Error::Usage(format!(
                    "`packages add` installs one Package at a time (got a second: '{extra}')"
                )))
            }
        }
    }
    match source {
        Some(source) => Ok(Command::PackagesAdd { source, yes }),
        None => Err(Error::Usage(
            "`packages add` needs the folder, repository URL, or Official Package Collection \
             name to install, e.g. `packages add ~/src/my-package`"
                .to_string(),
        )),
    }
}

/// `packages update [NAME] [--yes]`, in either order.
///
/// The name is optional because "every Git-backed Package" is the ordinary intent: an operator who
/// installed three Packages from repositories wants all three current, and naming them one at a
/// time is the exception.
fn parse_packages_update(args: &[&str]) -> Result<Command> {
    let mut name: Option<String> = None;
    let mut yes = false;
    for arg in args {
        match *arg {
            "--yes" => yes = true,
            option if option.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `packages update` option '{option}' — the only one is --yes"
                )))
            }
            candidate if name.is_none() => name = Some(candidate.to_string()),
            extra => {
                return Err(Error::Usage(format!(
                    "`packages update` takes at most one Package name (got a second: '{extra}'). \
                     With no name it updates every Git-backed Package."
                )))
            }
        }
    }
    Ok(Command::PackagesUpdate { name, yes })
}

/// `packages replay [DATABASE ...] [--check] [--yes] [--force-all] [--client-data PATH]`.
///
/// Flags and Shard names interleave freely, but a Shard name is validated the moment it is taken:
/// anything flag-shaped that is not one of this verb's own options is REFUSED rather than passed on
/// to a subprocess, exactly as `publish` refuses it. An empty list is not a default target list — it
/// means "none named", which `replay` resolves against the recorded development topology once it can
/// read one.
fn parse_packages_replay(args: &[&str]) -> Result<Command> {
    let mut options = packages::replay::ReplayOptions::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--check" => options.check = true,
            "--yes" => options.yes = true,
            "--force-all" => options.force_all = true,
            "--client-data" => {
                options.client_data =
                    Some(rest.next().map(|path| (*path).to_string()).ok_or_else(|| {
                        Error::Usage(
                            "`--client-data` needs a path, e.g. `--client-data \
                                 /games/WoW-1.12.1/Data`"
                                .to_string(),
                        )
                    })?);
            }
            option if option.starts_with("--") => {
                return Err(Error::Usage(format!(
                    "unknown `packages replay` option '{option}' — the options are --check, --yes, \
                     --force-all and --client-data PATH"
                )))
            }
            name => {
                publish::validate_database(packages::replay::VERB, name)?;
                options.databases.push(name.to_string());
            }
        }
    }
    Ok(Command::PackagesReplay(options))
}

/// `packages config NAME [KEY [VALUE]] [--new]`.
///
/// How many positional arguments there are IS the action: a name lists, a name and a key gets, a
/// name, a key and a value sets. `--new` is consent for the set, so it is refused on the read forms
/// rather than accepted and ignored.
///
/// A VALUE is taken verbatim, dashes and all. An argument that starts with `-` is only read as an
/// option while the value slot is still empty — a config value of `-5` is an ordinary thing to
/// write, and refusing it as an unknown option would be a rule nobody could work around.
fn parse_packages_config(args: &[&str]) -> Result<Command> {
    let mut allow_new = false;
    let mut positional: Vec<String> = Vec::new();
    for arg in args {
        match *arg {
            "--new" => allow_new = true,
            option if option.starts_with('-') && positional.len() < 2 => {
                return Err(Error::Usage(format!(
                    "unknown `packages config` option '{option}' — the only one is --new"
                )))
            }
            other => positional.push((*other).to_string()),
        }
    }

    let (package, rest) = positional.split_first().ok_or_else(|| {
        Error::Usage(
            "`packages config` needs a Package name, e.g. `packages config my-package` to list \
             its keys"
                .to_string(),
        )
    })?;
    let action = match rest {
        [] => packages::config::ConfigAction::List,
        [key] => packages::config::ConfigAction::Get { key: key.clone() },
        [key, value] => packages::config::ConfigAction::Set {
            key: key.clone(),
            value: value.clone(),
            allow_new,
        },
        [_, _, extra, ..] => {
            return Err(Error::Usage(format!(
                "`packages config` sets one key at a time (got a fourth argument: '{extra}'). A \
                 value with spaces has to be quoted."
            )))
        }
    };
    if allow_new && !matches!(action, packages::config::ConfigAction::Set { .. }) {
        return Err(Error::Usage(
            "`--new` is the consent to create a key the Package never seeded, so it only applies \
             to a write: `packages config NAME KEY VALUE --new`."
                .to_string(),
        ));
    }
    Ok(Command::PackagesConfig(packages::config::ConfigOptions {
        package: package.clone(),
        action,
    }))
}

/// `packages enable NAME` and `packages disable NAME`: one Package name, no options.
///
/// Neither destroys anything, and each is the other's undo, so neither has a confirmation to
/// answer in advance. `--yes` on one of them would be a flag that did nothing.
fn parse_packages_move(verb: &str, args: &[&str]) -> Result<String> {
    match args {
        [name] if !name.starts_with('-') => Ok((*name).to_string()),
        [] => Err(Error::Usage(format!(
            "`packages {verb}` needs a name, e.g. `packages {verb} my-package`"
        ))),
        [_name, other, ..] => Err(Error::Usage(format!(
            "`packages {verb}` takes one name (got a second argument: '{other}')"
        ))),
        [option] => Err(Error::Usage(format!(
            "`packages {verb}` takes a Package name, not an option ('{option}')"
        ))),
    }
}

/// `packages remove NAME [--yes]`, in either order.
///
/// `--yes` answers the deletion question in advance, like `packages add`'s. A name is a Package
/// folder name, so it can never start with `-`; an option-looking argument is refused rather than
/// taken as one.
fn parse_packages_remove(args: &[&str]) -> Result<Command> {
    let mut name: Option<String> = None;
    let mut yes = false;
    for arg in args {
        match *arg {
            "--yes" => yes = true,
            option if option.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `packages remove` option '{option}' — the only one is --yes"
                )))
            }
            candidate if name.is_none() => name = Some(candidate.to_string()),
            extra => {
                return Err(Error::Usage(format!(
                    "`packages remove` deletes one Package at a time (got a second: '{extra}')"
                )))
            }
        }
    }
    match name {
        Some(name) => Ok(Command::PackagesRemove { name, yes }),
        None => Err(Error::Usage(
            "`packages remove` needs the name of a disabled Package, e.g. `packages remove \
             my-package`"
                .to_string(),
        )),
    }
}

/// `dev up [--single] [--lan <IP>]`, in either order.
///
/// The two options are orthogonal — `--single` chooses how many databases, `--lan` chooses who can
/// reach the two client-facing ports — so refusing to combine them would only mean a contributor
/// debugging a sharded realm from a LAN client had to go back to hand-rolling the launch.
fn parse_dev_up(args: &[&str]) -> Result<Command> {
    let mut bind = ClientBind::Loopback;
    let mut topology = Topology::Sharded;
    let mut rest = args;
    while let Some((head, tail)) = rest.split_first() {
        match *head {
            "--single" => {
                topology = Topology::Single;
                rest = tail;
            }
            "--lan" => match tail.split_first() {
                Some((ip, after)) if !ip.starts_with('-') => {
                    bind = ClientBind::parse_lan(ip)?;
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`dev up --lan` needs this machine's private-LAN IPv4 address, e.g. \
                         `dev up --lan 192.168.1.50`"
                            .to_string(),
                    ))
                }
            },
            other => {
                return Err(Error::Usage(format!(
                    "unknown `dev up` option '{other}' — the only ones are --single and --lan <IP>"
                )))
            }
        }
    }
    Ok(Command::DevUp { bind, topology })
}

fn parse_import_world(args: &[&str]) -> Result<Command> {
    let mut options = ImportOptions::default();
    let mut rest = args;
    while let Some((head, tail)) = rest.split_first() {
        match *head {
            "--accept" => {
                options.accept = true;
                rest = tail;
            }
            "--client-data" => match tail.split_first() {
                Some((path, after)) if !path.starts_with('-') => {
                    options.client_data = Some((*path).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`import --client-data` needs the path to your 1.12.1 client's Data/ \
                         directory, e.g. `--client-data /games/WoW-1.12.1/Data`"
                            .to_string(),
                    ))
                }
            },
            "--profile-shard" => match tail.split_first() {
                Some((assignment, after)) if !assignment.starts_with('-') => {
                    options.profile_shards.push((*assignment).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`import --profile-shard` needs PROFILE=SHARD, e.g. \
                         `--profile-shard alliance-kalimdor=lyracore-world-1`"
                            .to_string(),
                    ))
                }
            },
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `import` option '{other}' — the options are --accept, --client-data \
                     PATH, and --profile-shard PROFILE=SHARD"
                )))
            }
            other => {
                return Err(Error::Usage(format!(
                    "`import` takes no positional arguments other than the verbs world and vmaps \
                     (got '{other}'). Name the client directory with --client-data {other}"
                )))
            }
        }
    }
    Ok(Command::ImportWorld(options))
}

/// `import vmaps [--client-data PATH]`. No `--accept`, deliberately: there is no consent question
/// to answer in advance — the vmaps import fetches nothing and reads only the operator's own
/// client, so a flag that implied otherwise would misdescribe the command.
fn parse_import_vmaps(args: &[&str]) -> Result<Command> {
    let mut options = VmapOptions::default();
    let mut rest = args;
    while let Some((head, tail)) = rest.split_first() {
        match *head {
            "--client-data" => match tail.split_first() {
                Some((path, after)) if !path.starts_with('-') => {
                    options.client_data = Some((*path).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`import vmaps --client-data` needs the path to your 1.12.1 client's \
                         Data/ directory, e.g. `--client-data /games/WoW-1.12.1/Data`"
                            .to_string(),
                    ))
                }
            },
            "--profile-shard" => match tail.split_first() {
                Some((assignment, after)) if !assignment.starts_with('-') => {
                    options.profile_shards.push((*assignment).to_string());
                    rest = after;
                }
                _ => {
                    return Err(Error::Usage(
                        "`import vmaps --profile-shard` needs PROFILE=SHARD, e.g. \
                         `--profile-shard alliance-kalimdor=lyracore-world-1`"
                            .to_string(),
                    ))
                }
            },
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `import vmaps` option '{other}' — the options are --client-data PATH \
                     and --profile-shard PROFILE=SHARD (there is no consent to --accept: nothing \
                     is fetched)"
                )))
            }
            other => {
                return Err(Error::Usage(format!(
                    "`import vmaps` takes no positional arguments (got '{other}'). Name the \
                     client directory with --client-data {other}"
                )))
            }
        }
    }
    Ok(Command::ImportVmaps { options })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        Command::parse(&args)
    }

    #[test]
    fn the_documented_surface_parses() {
        assert_eq!(parse("doctor").unwrap(), Command::Doctor);
        assert_eq!(
            parse("dev up").unwrap(),
            Command::DevUp {
                bind: ClientBind::Loopback,
                topology: Topology::Sharded,
            }
        );
        assert_eq!(parse("dev status").unwrap(), Command::DevStatus);
        assert_eq!(parse("dev smoke").unwrap(), Command::DevSmoke);
        assert_eq!(parse("dev logs").unwrap(), Command::DevLogs(None));
        assert_eq!(
            parse("dev down").unwrap(),
            Command::DevDown { forget: false }
        );
        assert_eq!(
            parse("account create TEST --password-stdin").unwrap(),
            Command::AccountCreate {
                user: "TEST".to_string(),
                source: PasswordSource::Stdin,
            }
        );
    }

    #[test]
    fn account_create_without_the_flag_prompts_instead() {
        assert_eq!(
            parse("account create TEST").unwrap(),
            Command::AccountCreate {
                user: "TEST".to_string(),
                source: PasswordSource::Tty,
            }
        );
    }

    #[test]
    fn logs_can_name_a_component() {
        assert_eq!(
            parse("dev logs gateway").unwrap(),
            Command::DevLogs(Some(Component::Gateway))
        );
        assert!(parse("dev logs realm-core").is_err());
    }

    #[test]
    fn down_takes_forget() {
        assert_eq!(
            parse("dev down --forget").unwrap(),
            Command::DevDown { forget: true }
        );
    }

    #[test]
    fn no_arguments_shows_help_rather_than_failing() {
        assert_eq!(parse("").unwrap(), Command::Help);
    }

    #[test]
    fn lan_needs_a_private_address_and_no_other_option_is_invented() {
        assert_eq!(
            parse("dev up --lan 192.168.1.50").unwrap(),
            Command::DevUp {
                bind: ClientBind::parse_lan("192.168.1.50").unwrap(),
                topology: Topology::Sharded,
            }
        );
        for line in [
            "dev up --lan",         // no address
            "dev up --lan 8.8.8.8", // public
            "dev up --lan 0.0.0.0", // every interface
            "dev up --public",      // not a flag this CLI has
            "dev up --sharded",     // sharded is the default, not a flag
            "dev up --lan 10.0.0.1 extra",
            "dev up --lan --single", // an option is not an address
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "`{line}` should be a usage error"
            );
        }
    }

    #[test]
    fn single_selects_the_one_database_fixture_and_composes_with_lan() {
        assert_eq!(
            parse("dev up --single").unwrap(),
            Command::DevUp {
                bind: ClientBind::Loopback,
                topology: Topology::Single,
            }
        );
        // Orthogonal options: how many databases, and who can reach the client-facing ports.
        for line in [
            "dev up --single --lan 10.0.0.7",
            "dev up --lan 10.0.0.7 --single",
        ] {
            assert_eq!(
                parse(line).unwrap(),
                Command::DevUp {
                    bind: ClientBind::parse_lan("10.0.0.7").unwrap(),
                    topology: Topology::Single,
                },
                "`{line}`"
            );
        }
    }

    #[test]
    fn the_help_text_documents_the_topology_flag() {
        assert!(USAGE_ALL.contains("--single"), "{USAGE_ALL}");
    }

    #[test]
    fn publish_with_no_names_parses_to_an_empty_list_and_takes_names_verbatim() {
        // Bare `publish` is resolved against the recorded topology inside `publish::run`, which
        // is the earliest point a `ProjectLayout` exists to read it from — parsing itself only
        // reflects what was typed.
        assert_eq!(
            parse("publish").unwrap(),
            Command::Publish {
                databases: vec![],
                skip_preflight: false,
            }
        );
        assert_eq!(
            parse("publish lyracore lyracore-world-1").unwrap(),
            Command::Publish {
                databases: vec!["lyracore".to_string(), "lyracore-world-1".to_string()],
                skip_preflight: false,
            }
        );
    }

    #[test]
    fn skip_preflight_is_the_only_flag_publish_accepts() {
        assert_eq!(
            parse("publish --skip-preflight").unwrap(),
            Command::Publish {
                databases: vec![],
                skip_preflight: true,
            }
        );
        assert_eq!(
            parse("publish --skip-preflight realm-core").unwrap(),
            Command::Publish {
                databases: vec!["realm-core".to_string()],
                skip_preflight: true,
            }
        );
    }

    #[test]
    fn publish_refuses_the_destructive_flag_at_the_command_line() {
        // The one unrecoverable mistake available here. Exit 2, before anything runs.
        for line in [
            "publish -c",
            "publish --delete-data",
            "publish lyracore -c",
            "publish --clear",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "`{line}` should be refused"
            );
            assert!(error.to_string().contains("Refusing"), "`{line}`: {error}");
        }
    }

    #[test]
    fn preflight_takes_no_arguments() {
        assert_eq!(parse("preflight").unwrap(), Command::Preflight);
        assert!(parse("preflight lyracore").is_err());
    }

    #[test]
    fn import_parses_its_two_options_in_any_order() {
        assert_eq!(
            parse("import").unwrap(),
            Command::ImportWorld(import::ImportOptions::default())
        );
        assert_eq!(
            parse("import --accept").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: true,
                client_data: None,
                profile_shards: vec![],
            })
        );
        assert_eq!(
            parse("import --client-data /games/wow/Data --accept").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
                profile_shards: vec![],
            })
        );
        assert_eq!(
            parse("import --accept --client-data /games/wow/Data").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
                profile_shards: vec![],
            })
        );
    }

    #[test]
    fn import_parses_profile_shards_for_world_and_vmaps() {
        let profile_shards = vec![
            "alliance-eastern=lyracore".to_string(),
            "alliance-kalimdor=lyracore-world-1".to_string(),
            "instances=lyracore-instances".to_string(),
        ];
        assert_eq!(
            parse("import world --profile-shard alliance-eastern=lyracore --profile-shard alliance-kalimdor=lyracore-world-1 --profile-shard instances=lyracore-instances").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: false,
                client_data: None,
                profile_shards: profile_shards.clone(),
            })
        );
        assert_eq!(
            parse("import vmaps --profile-shard alliance-eastern=lyracore --profile-shard alliance-kalimdor=lyracore-world-1 --profile-shard instances=lyracore-instances").unwrap(),
            Command::ImportVmaps {
                options: import::VmapOptions {
                    client_data: None,
                    profile_shards,
                },
            }
        );
    }

    #[test]
    fn bare_import_is_an_alias_of_import_world() {
        // One command, two spellings — the parser routes both through the same arm, so the two
        // cannot drift apart.
        for line in ["", " --accept", " --accept --client-data /games/wow/Data"] {
            assert_eq!(
                parse(&format!("import{line}")).unwrap(),
                parse(&format!("import world{line}")).unwrap(),
                "`import{line}`"
            );
        }
    }

    #[test]
    fn import_vmaps_takes_client_data_and_profile_shards() {
        assert_eq!(
            parse("import vmaps").unwrap(),
            Command::ImportVmaps {
                options: import::VmapOptions::default()
            }
        );
        assert_eq!(
            parse("import vmaps --client-data /games/wow/Data").unwrap(),
            Command::ImportVmaps {
                options: import::VmapOptions {
                    client_data: Some("/games/wow/Data".to_string()),
                    profile_shards: vec![],
                },
            }
        );
    }

    #[test]
    fn import_vmaps_refuses_accept_because_it_has_no_consent_question() {
        let error = parse("import vmaps --accept").unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("--client-data"), "{error}");
    }

    #[test]
    fn import_vmaps_refuses_a_bare_path_by_naming_the_flag_it_wanted() {
        for line in ["import vmaps /games/wow/Data", "import vmaps --client-data"] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn import_refuses_a_bare_path_by_naming_the_flag_it_wanted() {
        let error = parse("import /games/wow/Data").unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("--client-data"), "{error}");
    }

    #[test]
    fn import_rejects_an_empty_or_flag_shaped_client_data_value() {
        for line in [
            "import --client-data",
            "import --client-data --accept",
            "import --nonsense",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "`{line}` should be a usage error"
            );
        }
    }

    #[test]
    fn the_help_text_documents_import() {
        // `import` is one of the first-five-minutes commands, so the short help names it too —
        // but its options and verbs are contributor-depth detail and only show up in the full
        // listing.
        assert!(USAGE.contains("lyracore import"), "{USAGE}");
        assert!(USAGE_ALL.contains("lyracore import"), "{USAGE_ALL}");
        assert!(USAGE_ALL.contains("lyracore import world"), "{USAGE_ALL}");
        assert!(USAGE_ALL.contains("lyracore import vmaps"), "{USAGE_ALL}");
        assert!(USAGE_ALL.contains("--client-data"), "{USAGE_ALL}");
        assert!(USAGE_ALL.contains("--accept"), "{USAGE_ALL}");
        assert!(
            USAGE.contains("every Alliance early-game corridor"),
            "{USAGE}"
        );
        assert!(
            USAGE_ALL.contains("Eastern Kingdoms and Kalimdor"),
            "{USAGE_ALL}"
        );
        assert!(USAGE_ALL.contains("populated World Shard"), "{USAGE_ALL}");
    }

    #[test]
    fn unknown_commands_are_usage_errors() {
        for line in [
            "nonsense",
            "dev",
            "dev sideways",
            "account",
            "account create",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "`{line}` should be a usage error"
            );
        }
    }

    #[test]
    fn production_status_requires_an_explicit_topology_and_log() {
        assert_eq!(
            parse(
                "production status --server http://127.0.0.1:3000 --gateway-log /tmp/gw.log \
                 --realm-core lyracore-realm \
                 lyracore lyracore-world-1 lyracore-instances lyracore-realm"
            )
            .unwrap(),
            Command::ProductionStatus(production::StatusOptions {
                server: "http://127.0.0.1:3000".into(),
                gateway_log: "/tmp/gw.log".into(),
                realm_core: "lyracore-realm".into(),
                databases: vec![
                    "lyracore".into(),
                    "lyracore-world-1".into(),
                    "lyracore-instances".into(),
                    "lyracore-realm".into(),
                ],
            })
        );
    }

    #[test]
    fn production_status_refuses_implicit_or_ambiguous_topology() {
        for line in [
            "production status --server local --realm-core lyracore-realm lyracore lyracore-realm",
            "production status --server local --gateway-log /tmp/gw.log lyracore lyracore-realm",
            "production status --server local --gateway-log /tmp/gw.log --realm-core lyracore-realm lyracore",
            "production status --server local --gateway-log /tmp/gw.log --realm-core lyracore-realm lyracore-realm lyracore-realm",
            "production status --server local --gateway-log /tmp/one.log --gateway-log /tmp/two.log --realm-core lyracore-realm lyracore lyracore-realm",
            "production status --server local --gateway-log /tmp/gw.log --realm-core one --realm-core two lyracore one two",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn production_status_requires_one_explicit_spacetime_server() {
        assert!(parse(
            "production status --server http://127.0.0.1:3000 --gateway-log /tmp/gw.log \
             --realm-core lyracore-realm lyracore lyracore-realm"
        )
        .is_ok());
        assert!(parse(
            "production status --gateway-log /tmp/gw.log --realm-core lyracore-realm \
             lyracore lyracore-realm"
        )
        .is_err());
        assert!(parse(
            "production status --server local --server http://127.0.0.1:3000 \
             --gateway-log /tmp/gw.log --realm-core lyracore-realm lyracore lyracore-realm"
        )
        .is_err());
    }

    #[test]
    fn a_trailing_password_argument_is_refused() {
        // The old `gateway provision USER PASSWORD` shape must not be silently accepted here,
        // or the password would land in argv again.
        assert!(parse("account create TEST hunter2").is_err());
    }

    // ---- config ----

    #[test]
    fn bare_config_shows_and_set_client_data_takes_a_path() {
        assert_eq!(parse("config").unwrap(), Command::ConfigShow);
        assert_eq!(
            parse("config set client-data /games/wow/Data").unwrap(),
            Command::ConfigSetClientData {
                path: "/games/wow/Data".to_string(),
            }
        );
    }

    #[test]
    fn config_set_client_data_without_a_path_is_a_usage_error() {
        let error = parse("config set client-data").unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
    }

    #[test]
    fn an_unknown_config_key_names_client_data_as_the_only_one() {
        for line in ["config set nonsense value", "config wipe"] {
            let error = parse(line).unwrap_err().to_string();
            assert!(error.contains("client-data"), "{line}: {error}");
        }
    }

    #[test]
    fn the_help_text_documents_config() {
        assert!(USAGE_ALL.contains("lyracore config"), "{USAGE_ALL}");
        assert!(USAGE_ALL.contains("client-data"), "{USAGE_ALL}");
    }

    // ---- client sync ----

    #[test]
    fn client_sync_takes_no_arguments() {
        assert_eq!(parse("client sync").unwrap(), Command::ClientSync);
        assert!(parse("client sync extra").is_err());
    }

    #[test]
    fn client_without_a_recognised_verb_names_sync_as_the_one_that_exists() {
        for line in ["client", "client pack", "client install"] {
            let error = parse(line).unwrap_err().to_string();
            assert!(error.contains("sync"), "{line}: {error}");
        }
    }

    #[test]
    fn the_help_text_documents_client_sync() {
        assert!(USAGE_ALL.contains("lyracore client sync"), "{USAGE_ALL}");
    }

    // ---- character gm ----

    #[test]
    fn character_gm_parses_true_and_false() {
        assert_eq!(
            parse("character gm Ginger true").unwrap(),
            Command::CharacterGm {
                name: "Ginger".to_string(),
                enabled: true,
            }
        );
        assert_eq!(
            parse("character gm Ginger false").unwrap(),
            Command::CharacterGm {
                name: "Ginger".to_string(),
                enabled: false,
            }
        );
    }

    #[test]
    fn character_gm_refuses_anything_but_true_or_false() {
        for line in ["character gm Ginger 1", "character gm Ginger yes"] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn character_without_a_recognised_verb_names_gm_as_the_one_that_exists() {
        for line in [
            "character",
            "character gm",
            "character gm Ginger",
            "character sideways",
        ] {
            let error = parse(line).unwrap_err().to_string();
            assert!(error.contains("gm"), "{line}: {error}");
        }
    }

    #[test]
    fn the_help_text_documents_character_gm() {
        assert!(USAGE_ALL.contains("character gm"), "{USAGE_ALL}");
    }

    // ---- packages ----

    #[test]
    fn packages_add_takes_one_package_source_and_the_confirmation_flag_in_either_order() {
        assert_eq!(
            parse("packages add /src/greeter").unwrap(),
            Command::PackagesAdd {
                source: "/src/greeter".to_string(),
                yes: false,
            }
        );
        for line in [
            "packages add /src/greeter --yes",
            "packages add --yes /src/greeter",
        ] {
            assert_eq!(
                parse(line).unwrap(),
                Command::PackagesAdd {
                    source: "/src/greeter".to_string(),
                    yes: true,
                },
                "`{line}`"
            );
        }
    }

    #[test]
    fn a_git_url_reaches_add_verbatim() {
        // Parsing never decides what a Package Source IS: `packages::add` does, against the
        // filesystem. A URL mangled here (a stripped suffix, a normalised host) would be a URL
        // nobody typed being cloned.
        for url in [
            "https://example.invalid/greeter.git",
            "ssh://git@example.invalid/greeter.git",
            "git://example.invalid/greeter",
            "git@example.invalid:team/greeter.git",
        ] {
            assert_eq!(
                parse(&format!("packages add {url}")).unwrap(),
                Command::PackagesAdd {
                    source: url.to_string(),
                    yes: false,
                },
                "{url}"
            );
        }
    }

    #[test]
    fn packages_add_refuses_no_source_two_sources_and_invented_options() {
        for line in [
            "packages add",
            "packages add --yes",
            "packages add /src/one /src/two",
            "packages add /src/greeter --force",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn packages_update_takes_an_optional_name_and_the_confirmation_flag() {
        assert_eq!(
            parse("packages update").unwrap(),
            Command::PackagesUpdate {
                name: None,
                yes: false
            }
        );
        assert_eq!(
            parse("packages update --yes").unwrap(),
            Command::PackagesUpdate {
                name: None,
                yes: true
            }
        );
        for line in [
            "packages update greeter --yes",
            "packages update --yes greeter",
        ] {
            assert_eq!(
                parse(line).unwrap(),
                Command::PackagesUpdate {
                    name: Some("greeter".to_string()),
                    yes: true
                },
                "{line}"
            );
        }
        for line in ["packages update one two", "packages update greeter --force"] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn packages_list_takes_no_arguments() {
        assert_eq!(parse("packages list").unwrap(), Command::PackagesList);
        assert!(parse("packages list --all").is_err());
    }

    #[test]
    fn packages_new_takes_one_name() {
        assert_eq!(
            parse("packages new greeter").unwrap(),
            Command::PackagesNew {
                name: "greeter".to_string(),
            }
        );
    }

    #[test]
    fn packages_new_refuses_no_name_and_a_second_argument() {
        for line in ["packages new", "packages new greeter extra"] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn packages_build_takes_no_arguments() {
        assert_eq!(parse("packages build").unwrap(), Command::PackagesBuild);
        for line in ["packages build --watch", "packages build my-package"] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn packages_check_takes_no_arguments() {
        assert_eq!(parse("packages check").unwrap(), Command::PackagesCheck);
        for line in ["packages check --watch", "packages check my-package"] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn packages_replay_takes_shard_names_and_its_own_options_in_any_order() {
        assert_eq!(
            parse("packages replay").unwrap(),
            Command::PackagesReplay(packages::replay::ReplayOptions::default())
        );
        assert_eq!(
            parse("packages replay lyracore --check lyracore-kalimdor --force-all --yes").unwrap(),
            Command::PackagesReplay(packages::replay::ReplayOptions {
                databases: vec!["lyracore".to_string(), "lyracore-kalimdor".to_string()],
                client_data: None,
                check: true,
                yes: true,
                force_all: true,
            })
        );
        assert_eq!(
            parse("packages replay --client-data /games/Data lyracore").unwrap(),
            Command::PackagesReplay(packages::replay::ReplayOptions {
                databases: vec!["lyracore".to_string()],
                client_data: Some("/games/Data".to_string()),
                ..Default::default()
            })
        );
    }

    /// The same guard `publish` has: a Shard list is names only, and a flag hidden among them is
    /// refused at parse time rather than forwarded to the importer.
    #[test]
    fn packages_replay_refuses_a_flag_shaped_shard_name() {
        for line in [
            "packages replay -c",
            "packages replay lyracore --delete-data",
            "packages replay --unknown",
            "packages replay --client-data",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    /// How many positional arguments there are IS the action.
    #[test]
    fn packages_config_reads_its_action_from_the_number_of_arguments() {
        use packages::config::{ConfigAction, ConfigOptions};

        assert_eq!(
            parse("packages config greeter").unwrap(),
            Command::PackagesConfig(ConfigOptions {
                package: "greeter".to_string(),
                action: ConfigAction::List,
            })
        );
        assert_eq!(
            parse("packages config greeter greeting").unwrap(),
            Command::PackagesConfig(ConfigOptions {
                package: "greeter".to_string(),
                action: ConfigAction::Get {
                    key: "greeting".to_string()
                },
            })
        );
        assert_eq!(
            parse("packages config greeter greeting Hello").unwrap(),
            Command::PackagesConfig(ConfigOptions {
                package: "greeter".to_string(),
                action: ConfigAction::Set {
                    key: "greeting".to_string(),
                    value: "Hello".to_string(),
                    allow_new: false,
                },
            })
        );
    }

    #[test]
    fn packages_config_takes_the_new_consent_in_any_position_of_a_write() {
        use packages::config::{ConfigAction, ConfigOptions};

        let expected = Command::PackagesConfig(ConfigOptions {
            package: "greeter".to_string(),
            action: ConfigAction::Set {
                key: "greeting".to_string(),
                value: "Hello".to_string(),
                allow_new: true,
            },
        });
        for line in [
            "packages config greeter greeting Hello --new",
            "packages config --new greeter greeting Hello",
            "packages config greeter --new greeting Hello",
        ] {
            assert_eq!(parse(line).unwrap(), expected, "{line}");
        }
    }

    /// `--new` is consent for a write. On a read it would be a flag that did nothing, so it is
    /// refused rather than accepted and dropped.
    #[test]
    fn packages_config_refuses_the_new_consent_on_a_read() {
        for line in [
            "packages config greeter --new",
            "packages config greeter greeting --new",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
            assert!(
                error.to_string().contains("only applies to a write"),
                "{line}: {error}"
            );
        }
    }

    /// A config value is arbitrary Operator text. `-5` is an ordinary thing to write, and reading it
    /// as an unknown option would be a rule with no way around it.
    #[test]
    fn packages_config_takes_a_dash_leading_value_verbatim() {
        use packages::config::{ConfigAction, ConfigOptions};

        assert_eq!(
            parse("packages config greeter offset -5").unwrap(),
            Command::PackagesConfig(ConfigOptions {
                package: "greeter".to_string(),
                action: ConfigAction::Set {
                    key: "offset".to_string(),
                    value: "-5".to_string(),
                    allow_new: false,
                },
            })
        );
    }

    #[test]
    fn packages_config_refuses_no_name_a_fourth_argument_and_invented_options() {
        for line in [
            "packages config",
            "packages config --new",
            "packages config greeter greeting Hello there",
            "packages config greeter --force",
            "packages config --force greeter",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn packages_enable_and_disable_each_take_one_name() {
        assert_eq!(
            parse("packages enable greeter").unwrap(),
            Command::PackagesEnable {
                name: "greeter".to_string()
            }
        );
        assert_eq!(
            parse("packages disable greeter").unwrap(),
            Command::PackagesDisable {
                name: "greeter".to_string()
            }
        );
        // Neither destroys anything, so neither has a confirmation for --yes to answer.
        for line in [
            "packages enable",
            "packages enable greeter extra",
            "packages enable --yes",
            "packages disable",
            "packages disable greeter extra",
            "packages disable --all",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn packages_remove_takes_one_name_and_the_confirmation_flag_in_either_order() {
        assert_eq!(
            parse("packages remove greeter").unwrap(),
            Command::PackagesRemove {
                name: "greeter".to_string(),
                yes: false
            }
        );
        for line in [
            "packages remove greeter --yes",
            "packages remove --yes greeter",
        ] {
            assert_eq!(
                parse(line).unwrap(),
                Command::PackagesRemove {
                    name: "greeter".to_string(),
                    yes: true
                },
                "{line}"
            );
        }
        for line in [
            "packages remove",
            "packages remove --yes",
            "packages remove one two",
            "packages remove greeter --force",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn packages_without_a_recognised_verb_names_the_nine_that_exist() {
        for line in ["packages", "packages install greeter", "packages upgrade"] {
            let error = parse(line).unwrap_err().to_string();
            for verb in PACKAGES_VERBS {
                assert!(error.contains(verb), "{line}: {error}");
            }
        }
    }

    const PACKAGES_VERBS: [&str; 9] = [
        "add", "build", "check", "disable", "enable", "list", "new", "remove", "update",
    ];

    #[test]
    fn the_help_text_documents_packages() {
        for verb in PACKAGES_VERBS {
            assert!(
                USAGE_ALL.contains(&format!("lyracore packages {verb}")),
                "{USAGE_ALL}"
            );
        }
    }

    // ---- service ----

    #[test]
    fn service_reconcile_takes_no_arguments() {
        assert_eq!(
            parse("service reconcile").unwrap(),
            Command::ServiceReconcile
        );
        for line in [
            "service reconcile --force",
            "service reconcile spacetimedb-standalone.service",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn service_without_a_recognised_verb_names_reconcile_as_the_one_that_exists() {
        for line in [
            "service",
            "service install",
            "service restart",
            "service --bogus",
        ] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
            assert!(error.to_string().contains("reconcile"), "{line}: {error}");
        }
    }

    /// The verb mutates a production host, so the help has to say what it does to one: which
    /// service, that systemd is reloaded and restarted, what is verified, and that it needs root.
    #[test]
    fn the_help_text_documents_what_service_reconcile_changes() {
        for phrase in [
            "lyracore service reconcile",
            "spacetimedb-standalone",
            "/etc/systemd/system",
            "LimitNOFILE",
            "Root only",
        ] {
            assert!(USAGE_ALL.contains(phrase), "{phrase}: {USAGE_ALL}");
        }
    }

    // ---- update ----

    #[test]
    fn update_takes_no_arguments() {
        assert_eq!(parse("update").unwrap(), Command::Update);
        assert!(parse("update origin").is_err());
    }

    #[test]
    fn the_help_text_documents_update() {
        assert!(USAGE_ALL.contains("lyracore update"), "{USAGE_ALL}");
    }

    /// `update` is a contributor's git-pull replacement. Reconciling a host is `service
    /// reconcile`'s job, so `update`'s whole help entry must not grow a word about services, and
    /// must not grow at all: it is the two lines it has always been.
    #[test]
    fn the_update_help_entry_says_nothing_about_services_and_has_not_grown() {
        let lines: Vec<&str> = USAGE_ALL.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains("lyracore update"))
            .expect("update is in the full help");
        let entry: Vec<&str> = lines[start..]
            .iter()
            .take_while(|line| !line.trim().is_empty())
            .copied()
            .collect();
        assert_eq!(entry.len(), 2, "{entry:?}");
        let entry = entry.join(" ");
        for absent in ["service", "systemd", "reconcile", "install", "root", "sudo"] {
            assert!(!entry.contains(absent), "{absent}: {entry}");
        }
    }

    // ---- help ----

    #[test]
    fn help_all_parses_and_bare_help_still_shows_the_short_form() {
        assert_eq!(parse("help").unwrap(), Command::Help);
        assert_eq!(parse("-h").unwrap(), Command::Help);
        assert_eq!(parse("--help").unwrap(), Command::Help);
        assert_eq!(parse("help --all").unwrap(), Command::HelpAll);
    }

    #[test]
    fn help_rejects_anything_else() {
        for line in ["help --bogus", "help --all extra", "help me"] {
            let error = parse(line).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{line}");
        }
    }

    #[test]
    fn the_short_help_covers_only_the_first_five_minutes() {
        for command in [
            "doctor",
            "dev up",
            "account create",
            "dev status",
            "dev down",
            "lyracore import",
        ] {
            assert!(USAGE.contains(command), "{USAGE}");
        }
        assert!(USAGE.contains("lyracore help --all"), "{USAGE}");
        // account create is required to log in at all — it's step 2 of the documented
        // quickstart, not a contributor-only command — so it belongs in the short form even
        // though `--password-stdin` (its scripted-run variant) does not.
        assert!(!USAGE.contains("--password-stdin"), "{USAGE}");
        // The contributor-facing surface must not leak into the short form.
        for absent in [
            "preflight",
            "lyracore publish",
            "lyracore config",
            "character gm",
            "lyracore service",
            "lyracore update",
            "dev logs",
            "dev smoke",
            "--single",
        ] {
            assert!(!USAGE.contains(absent), "{USAGE}");
        }
        assert!(USAGE.lines().count() <= 15, "{USAGE}");
    }

    #[test]
    fn the_full_help_still_covers_everything() {
        for command in [
            "lyracore doctor",
            "lyracore preflight",
            "lyracore publish",
            "lyracore dev up",
            "lyracore dev status",
            "lyracore dev logs",
            "lyracore dev smoke",
            "lyracore dev down",
            "lyracore account create",
            "lyracore import",
            "lyracore config",
            "lyracore client sync",
            "lyracore packages add",
            "lyracore packages build",
            "lyracore packages check",
            "lyracore packages disable",
            "lyracore packages enable",
            "lyracore packages list",
            "lyracore packages new",
            "lyracore packages remove",
            "lyracore character gm",
            "lyracore service reconcile",
            "lyracore update",
            "--password-stdin",
        ] {
            assert!(USAGE_ALL.contains(command), "{USAGE_ALL}");
        }
    }

    #[test]
    fn the_full_help_no_longer_carries_the_insider_asides() {
        // These phrases assumed the reader is working ON this project (danger-zone doc,
        // PR etiquette, shard-seam jargon) rather than someone running a server with it.
        for gone in ["say why in the PR", "no seam, no realm-core"] {
            assert!(!USAGE_ALL.contains(gone), "{USAGE_ALL}");
        }
    }
}
