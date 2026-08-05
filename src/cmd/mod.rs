pub mod account;
pub mod dev;
pub mod doctor;
pub mod import;
pub mod preflight;
pub mod publish;

use crate::project::{ClientBind, Component, Topology};
use crate::{Error, Result};
use account::PasswordSource;
use import::ImportOptions;

pub const USAGE: &str = "\
lyracore — local LyraCore development

USAGE:
  lyracore doctor                              check prerequisites for `dev up`
  lyracore preflight                           the offline deploy gate: build, schema, filters
  lyracore publish [DATABASE ...]              preflight, then publish the module (default:
                                               the fixture database). Takes database NAMES;
                                               every flag is refused, including the -c wipe
  lyracore publish --skip-preflight [DB ...]   publish without the gate (say why in the PR)
  lyracore dev up                              start (or reuse) the local realm: four databases
                                               with a live shard seam across Elwynn
  lyracore dev up --single                     the one-database fixture instead — no seam, no
                                               realm-core, every topology variable unset
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
                                               own 1.12.1 client's Data/ archives, run the
                                               world ETL and the class-spell import. Asks
                                               for consent first, every time
  lyracore import --accept                     the same, with the consent answered in
                                               advance (scripted runs)

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
    Import(ImportOptions),
    Help,
}

impl Command {
    pub fn parse(args: &[String]) -> Result<Self> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Command::Help),
            ["-h"] | ["--help"] | ["help"] => Ok(Command::Help),

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

            // `import` takes two options and no positional arguments. A bare path is the shape
            // somebody will type first (`lyracore import /games/wow/Data`), so it is refused by
            // NAME rather than as "unknown command".
            ["import", rest @ ..] => parse_import(rest),

            [other, ..] => Err(Error::Usage(format!("unknown command '{other}'"))),
        }
    }
}

/// `dev up [--single] [--lan <IP>]`, in either order.
///
/// The two options are orthogonal — `--single` chooses how many databases, `--lan` chooses who can
/// reach the two client-facing ports — so refusing to combine them would only mean a contributor
/// debugging a seam on a LAN client had to go back to hand-rolling the launch.
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

fn parse_import(args: &[&str]) -> Result<Command> {
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
                    "`import` takes no positional arguments (got '{other}'). Name the client \
                     directory with --client-data {other}"
                )))
            }
        }
    }
    Ok(Command::Import(options))
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
        assert!(USAGE.contains("--single"), "{USAGE}");
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
            Command::Import(import::ImportOptions::default())
        );
        assert_eq!(
            parse("import --accept").unwrap(),
            Command::Import(import::ImportOptions {
                accept: true,
                client_data: None,
            })
        );
        assert_eq!(
            parse("import --client-data /games/wow/Data --accept").unwrap(),
            Command::Import(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
            })
        );
        assert_eq!(
            parse("import --accept --client-data /games/wow/Data").unwrap(),
            Command::Import(import::ImportOptions {
                accept: true,
                client_data: Some("/games/wow/Data".to_string()),
            })
        );
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
        assert!(USAGE.contains("lyracore import"), "{USAGE}");
        assert!(USAGE.contains("--client-data"), "{USAGE}");
        assert!(USAGE.contains("--accept"), "{USAGE}");
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
}
