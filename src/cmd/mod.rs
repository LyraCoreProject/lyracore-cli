pub mod account;
pub mod character;
pub mod config;
pub mod dev;
pub mod doctor;
pub mod import;
pub mod preflight;
pub mod publish;
pub mod update;

use crate::project::{ClientBind, Component, Topology};
use crate::{Error, Result};
use account::PasswordSource;
use import::ImportOptions;

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
                           world — quests, creatures, items — pulled from public game
                           data and your own 1.12.1 client

Run `lyracore help --all` to see every command, including the ones for working on
LyraCore itself.";

/// The full command surface, including the contributor-facing corners `USAGE` leaves out.
pub const USAGE_ALL: &str = "\
lyracore — local LyraCore development

USAGE:
  lyracore doctor                              check whether your machine is ready for `dev up`
  lyracore preflight                           the offline deploy gate: build, schema, filters
  lyracore publish [DATABASE ...]              preflight, then publish the module (default:
                                               the fixture database). Takes database NAMES
                                               only — SpacetimeDB's data-wiping -c flag is
                                               deliberately not exposed here
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
                                               fixture: pull cmangos' classic-db, read your
                                               own 1.12.1 client's Data/ archives, drive the
                                               importer's modes in order and assert the
                                               FLOOR_* import floors — on every database the
                                               fixture populates, which includes the
                                               instance pool. Asks for consent first,
                                               every time
  lyracore import world --accept               the same command by its full name (`import`
                                               is its alias), with the consent answered in
                                               advance (scripted runs)
  lyracore import vmaps [--client-data PATH]   exact model/WMO collision triangles for each
                                               world shard, read from your own client's
                                               archives — nothing is fetched, so there is
                                               no consent gate
  lyracore config                              show the persisted client-data path (or \"(unset)\")
  lyracore config set client-data PATH         validate and remember your 1.12.1 client's Data/
                                               directory, so `import` and `doctor` stop asking
  lyracore character gm NAME true|false        grant (true) or revoke (false) GM level for a
                                               character — tries every world shard in turn
  lyracore update                              pull the latest checkout in place and restart the
                                               local dev stack (refuses over a dirty working tree)

The password is read from stdin with --password-stdin, otherwise from a hidden terminal
prompt. It is never passed as a command-line argument.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Doctor,
    Preflight,
    Publish {
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
        client_data: Option<String>,
    },
    ConfigShow,
    ConfigSetClientData {
        path: String,
    },
    CharacterGm {
        name: String,
        enabled: bool,
    },
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
            // `publish::databases` is what refuses anything flag-shaped, including `-c`.
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

            ["update"] => Ok(Command::Update),
            ["update", other, ..] => Err(Error::Usage(format!(
                "`update` takes no arguments (got '{other}')"
            ))),

            [other, ..] => Err(Error::Usage(format!("unknown command '{other}'"))),
        }
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
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `import` option '{other}' — the only ones are --accept and \
                     --client-data PATH"
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
    let mut client_data = None;
    let mut rest = args;
    while let Some((head, tail)) = rest.split_first() {
        match *head {
            "--client-data" => match tail.split_first() {
                Some((path, after)) if !path.starts_with('-') => {
                    client_data = Some((*path).to_string());
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
            other if other.starts_with('-') => {
                return Err(Error::Usage(format!(
                    "unknown `import vmaps` option '{other}' — the only one is --client-data PATH \
                     (there is no consent to --accept: nothing is fetched)"
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
    Ok(Command::ImportVmaps { client_data })
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
    fn publish_defaults_to_the_fixture_database_and_takes_names() {
        assert_eq!(
            parse("publish").unwrap(),
            Command::Publish {
                databases: vec![crate::project::ProjectLayout::DATABASE.to_string()],
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
                databases: vec![crate::project::ProjectLayout::DATABASE.to_string()],
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
            })
        );
        assert_eq!(
            parse("import --client-data /games/wow/Data --accept").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
            })
        );
        assert_eq!(
            parse("import --accept --client-data /games/wow/Data").unwrap(),
            Command::ImportWorld(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
            })
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
    fn import_vmaps_takes_only_the_client_data_option() {
        assert_eq!(
            parse("import vmaps").unwrap(),
            Command::ImportVmaps { client_data: None }
        );
        assert_eq!(
            parse("import vmaps --client-data /games/wow/Data").unwrap(),
            Command::ImportVmaps {
                client_data: Some("/games/wow/Data".to_string()),
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
            "lyracore character gm",
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
