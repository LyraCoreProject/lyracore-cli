//! See docs/commands.md for publish targets, credentials and migration checks.

use crate::cmd::import;
use crate::cmd::preflight;
use crate::proc::{CommandSpec, ProcessInspector, ProcessRunner};
use crate::project::{Component, ProjectLayout, Topology};
use crate::state::RuntimeState;
use crate::{Error, Result};

/// Flags that would destroy data. They are refused by the general flag guard below; naming them
/// buys a message that says WHY rather than "that is not a database name".
const DESTRUCTIVE: [&str; 4] = ["-c", "--clear-database", "--delete-data", "--clear"];

/// The verb name this module's refusals quote back at the operator.
const VERB: &str = "publish";

const LOOT_RECEIPT_TABLE: &str = "game_loot_roll_promotion_receipt";
const LOOT_DRAIN_QUERIES: [(&str, &str); 3] = [
    (
        "active or staging Loot Rolls",
        "SELECT COUNT(*) AS n FROM game_loot_roll WHERE id >= 0",
    ),
    (
        "Loot Roll votes",
        "SELECT COUNT(*) AS n FROM game_loot_roll_vote WHERE id >= 0",
    ),
    (
        "withheld corpse loot",
        "SELECT COUNT(*) AS n FROM game_corpse_loot WHERE withheld = true",
    ),
];

/// Reject anything that is not a plain database name, on behalf of `verb`.
///
/// This is the whole safety contract of the commands that take a Shard list, so it lives in one
/// function that both argument parsing and command rendering go through — a database name cannot
/// reach a rendered subprocess without passing here. `verb` is the command doing the refusing, so
/// the operator is sent to the surface they actually typed.
pub fn validate_database(verb: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Usage(format!(
            "`lyracore {verb}` takes database NAMES; got an empty one. Refusing."
        )));
    }
    if DESTRUCTIVE.contains(&name) {
        return Err(Error::Usage(format!(
            "`lyracore {verb}` takes database NAMES, not flags (got '{name}'). Refusing.\n  \
             That flag is the DESTRUCTIVE wipe: it deletes every row and breaks login until the \
             realm is re-provisioned. This CLI has no path to it, by design."
        )));
    }
    if name.starts_with('-') {
        return Err(Error::Usage(format!(
            "`lyracore {verb}` takes database NAMES, not flags (got '{name}'). Refusing.\n  \
             Forwarding an unexamined flag to a subprocess is the one unrecoverable mistake \
             available here, so no flag is forwarded at all."
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Usage(format!(
            "'{name}' is not a database name (expected letters, digits, `-` or `_`). Refusing."
        )));
    }
    Ok(())
}

/// Empty names select the runtime defaults in `run`; see docs/commands.md.
pub fn databases(args: &[String]) -> Result<Vec<String>> {
    for name in args {
        validate_database(VERB, name)?;
    }
    Ok(args.to_vec())
}

pub fn validate_native_server(server: &str) -> Result<()> {
    let valid = server
        .strip_prefix("http://")
        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
        .is_some_and(|address| address.ip().is_loopback() && address.port() != 0);
    if !valid {
        return Err(Error::Usage(
            "native publishing requires an explicit loopback HTTP address and nonzero port".into(),
        ));
    }
    Ok(())
}

// See docs/commands.md for default publish targets during local runtime changes.
fn recorded_databases(project: &ProjectLayout) -> Result<Vec<String>> {
    if project.has_native_host() {
        return Ok(vec![ProjectLayout::DATABASE.to_string()]);
    }
    let state = RuntimeState::load(&project.state_file())?;
    // `dev down` deliberately keeps diagnostic state such as the last topology, but without a
    // recorded gateway there is no active fixture for that topology to describe. In that case the
    // safe publish target is what a fresh `dev up` would provision: the default sharded topology.
    let topology = if state.record(Component::Gateway).is_some() {
        state.topology()
    } else {
        Topology::Sharded
    };
    Ok(topology
        .databases()
        .into_iter()
        .map(str::to_string)
        .collect())
}

fn database_list(databases: &[String]) -> String {
    if databases.is_empty() {
        "(none)".to_string()
    } else {
        databases.join(" ")
    }
}

pub fn publish_command(project: &ProjectLayout, database: &str) -> Result<CommandSpec> {
    let server = if project.has_native_host() {
        ProjectLayout::stdb_uri()
    } else {
        ProjectLayout::STDB_SERVER.into()
    };
    publish_command_to(project, database, &server)
}

fn publish_command_to(
    project: &ProjectLayout,
    database: &str,
    server: &str,
) -> Result<CommandSpec> {
    validate_database(VERB, database)?;
    let mut command = preflight::module_command(project, "spacetime");
    if project.has_native_host() {
        require_native_configuration(project, server)?;
        command = command
            .arg("--config-path")
            .arg(project.native_cli_config().to_string_lossy());
    }
    Ok(command
        .arg("publish")
        .arg("-s")
        .arg(server)
        .arg("-p")
        .arg(project.module_dir().to_string_lossy().to_string())
        .arg(format!(
            "--build-options={}",
            ProjectLayout::DEPLOY_FEATURES
        ))
        .arg("--yes")
        .arg(database))
}

fn require_native_configuration(project: &ProjectLayout, server: &str) -> Result<()> {
    validate_native_server(server)?;
    if !project.native_cli_config().is_file() {
        return Err(Error::Usage("native publishing requires its private publisher configuration; run `lyracore dev up` to prepare it".into()));
    }
    let state = RuntimeState::load(&project.state_file())?;
    let port = server
        .strip_prefix("http://")
        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
        .map(|address| address.port());
    if port == Some(ProjectLayout::STDB_PORT)
        && (state.gateway.is_some() || (!state.native_runtime && state.spacetime.is_some()))
    {
        return Err(Error::Usage("native publishing cannot target the recorded legacy runtime; stop it with `lyracore dev down` and start a native Realm first".into()));
    }
    if port == Some(ProjectLayout::STDB_PORT)
        && (!state.native_runtime || state.spacetime.is_none())
    {
        return Err(Error::Usage("native publishing on port 3000 requires a recorded Native Host; run `lyracore dev up` first".into()));
    }
    Ok(())
}

pub(crate) fn require_native_host(
    project: &ProjectLayout,
    inspector: &dyn ProcessInspector,
    target: &str,
) -> Result<()> {
    if project.has_native_host() {
        require_native_configuration(project, target)?;
        let address: std::net::SocketAddr = target
            .strip_prefix("http://")
            .and_then(|address| address.parse().ok())
            .ok_or_else(|| Error::Usage("invalid native publish address".into()))?;
        if address.port() == ProjectLayout::STDB_PORT {
            let state = RuntimeState::load(&project.state_file())?;
            let live = state.spacetime.as_ref().is_some_and(|record| {
                inspector.identity(record.pid).as_deref() == Some(record.identity.as_str())
            });
            if !live || !inspector.serving(&address.ip().to_string(), address.port()) {
                return Err(Error::Usage("native publishing on port 3000 requires the recorded Native Host to be running; run `lyracore dev up` first".into()));
            }
        }
    }
    Ok(())
}

fn describe_command(database: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("describe")
        .arg("--json")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg(database)
}

fn checkout_has_loot_receipt(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<bool> {
    let prepared = crate::cmd::bindings::ensure(project, runner)?;
    let (schema, errors) = crate::rls::generated_schema(&prepared.directory);
    if !errors.is_empty() {
        return Err(Error::Process(format!(
            "could not read the prepared Module schema: {}. Nothing was published.",
            errors.join("; ")
        )));
    }
    Ok(schema.contains_key(LOOT_RECEIPT_TABLE))
}

fn deployed_has_loot_receipt(database: &str, output: &str) -> Result<bool> {
    let schema: serde_json::Value = serde_json::from_str(output).map_err(|error| {
        Error::Process(format!(
            "could not read the deployed schema for '{database}': invalid JSON ({error}). Nothing \
             was published."
        ))
    })?;
    let tables = schema
        .as_object()
        .and_then(|root| root.get("tables"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            Error::Process(format!(
                "could not read the deployed schema for '{database}': expected a top-level \
                 `tables` list. Nothing was published."
            ))
        })?;

    let mut receipt_tables = 0usize;
    for table in tables {
        let Some(name) = table
            .as_object()
            .and_then(|table| table.get("name"))
            .and_then(serde_json::Value::as_str)
        else {
            return Err(Error::Process(format!(
                "could not read the deployed schema for '{database}': a table has no string \
                 `name`. Nothing was published."
            )));
        };
        if name == LOOT_RECEIPT_TABLE {
            receipt_tables += 1;
        }
    }
    match receipt_tables {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::Process(format!(
            "could not read the deployed schema for '{database}': `{LOOT_RECEIPT_TABLE}` appears \
             more than once. Nothing was published."
        ))),
    }
}

fn parse_count(database: &str, state: &str, output: &str) -> Result<u64> {
    let rows = import::table_cells(output);
    let [header, value] = rows.as_slice() else {
        return Err(Error::Process(format!(
            "could not read {state} on '{database}': expected one `n` count. Nothing was published."
        )));
    };
    if header.as_slice() != ["n"] || value.len() != 1 {
        return Err(Error::Process(format!(
            "could not read {state} on '{database}': unexpected SQL output. Nothing was published."
        )));
    }
    value[0].parse().map_err(|_| {
        Error::Process(format!(
            "could not read {state} on '{database}': count is not an unsigned integer. Nothing was \
             published."
        ))
    })
}

pub(crate) fn require_loot_roll_upgrade_ready(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    databases: &[String],
) -> Result<()> {
    if !checkout_has_loot_receipt(project, runner)? {
        return Ok(());
    }

    for database in databases {
        let schema = runner
            .run_and_wait(&describe_command(database))
            .map_err(|error| {
                Error::Process(format!(
                    "could not read the deployed schema for '{database}': {error}. Nothing was \
                     published."
                ))
            })?;
        if deployed_has_loot_receipt(database, &schema)? {
            continue;
        }

        for (state, query) in LOOT_DRAIN_QUERIES {
            let output = runner
                .run_and_wait(&import::sql_command(project, database, query))
                .map_err(|error| {
                    Error::Process(format!(
                        "could not read {state} on '{database}': {error}. Nothing was published."
                    ))
                })?;
            let count = parse_count(database, state, &output)?;
            if count != 0 {
                return Err(Error::Process(format!(
                    "loot-roll receipt upgrade refused: '{database}' still has {count} {state}. \
                     Stop new Loot Roll producers, keep the old Gateways running until settlement \
                     clears every row, then stop the old Gateways and re-run `lyracore publish`. \
                     Nothing was published."
                )));
            }
        }
    }
    Ok(())
}

/// Stops at the first failed publish and reports completed and unattempted targets.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    inspector: &dyn ProcessInspector,
    databases: &[String],
    skip_preflight: bool,
    server: Option<&str>,
) -> Result<()> {
    if let Some(server) = server {
        validate_native_server(server)?;
        if !project.has_native_host() {
            return Err(Error::Usage(
                "publish --server requires a native checkout".into(),
            ));
        }
    }
    let databases = if databases.is_empty() {
        recorded_databases(project)?
    } else {
        databases.to_vec()
    };
    for database in &databases {
        validate_database(VERB, database)?;
    }

    let commands = databases
        .iter()
        .map(|database| match server {
            Some(server) => publish_command_to(project, database, server),
            None => publish_command(project, database),
        })
        .collect::<Result<Vec<_>>>()?;

    let target = server
        .map(str::to_owned)
        .unwrap_or_else(ProjectLayout::stdb_uri);
    require_native_host(project, inspector, &target)?;

    if skip_preflight {
        println!("· skipping preflight (--skip-preflight)");
    } else {
        preflight::run(project, runner)?;
    }

    if !project.has_native_host() {
        require_loot_roll_upgrade_ready(project, runner, &databases)?;
    }

    for (index, (database, command)) in databases.iter().zip(&commands).enumerate() {
        println!();
        println!("==> publishing to {database}");
        if let Err(error) = runner.run_streaming(command) {
            return Err(Error::Process(format!(
                "{error}\n  publish stopped at: {database}\n  completed: {}\n  not attempted: {}",
                database_list(&databases[..index]),
                database_list(&databases[index + 1..])
            )));
        }
    }

    println!();
    println!("published to: {}", databases.join(" "));
    if !project.has_native_host() && databases == [ProjectLayout::DATABASE] {
        println!(
            "NOTE: only {}. A SCHEMA change must be published to every shard of a sharded realm, \
             or the gateway",
            ProjectLayout::DATABASE
        );
        println!("      refuses logons on the ones left behind. See docs/danger-zones.md §3.");
    }
    Ok(())
}

/// Call after preflight and applicable migration checks; publish and repair precede activation.
pub(crate) fn publish_and_repair(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    inspector: &dyn ProcessInspector,
    database: &str,
) -> Result<()> {
    require_native_host(project, inspector, &ProjectLayout::stdb_uri())?;
    runner
        .run_streaming(&publish_command(project, database)?)
        .map_err(|error| {
            Error::Process(format!("Module publish failed at '{database}': {error}"))
        })?;
    let repair = import::call_command(project, database, "debug_repair_after_publish");
    runner.run_and_wait(&repair).map_err(|error| {
        let command = std::iter::once(repair.program())
            .chain(repair.args().iter().map(String::as_str))
            .map(|arg| super::packages::shell_quote(std::path::Path::new(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        Error::Process(format!(
            "'{database}' was published, but schedule repair failed: {error}\n  \
             Repair schedules with: {command}"
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    #[test]
    fn native_repair_advice_preserves_shell_metacharacters_as_one_argument() {
        let tmp = tempfile::Builder::new()
            .prefix("native user's $(printf bad) ")
            .tempdir()
            .unwrap();
        let project = healthy(&tmp);
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        std::fs::create_dir_all(&project.state_dir).unwrap();
        std::fs::write(project.native_cli_config(), "spacetimedb_token = 'fixture'").unwrap();
        RuntimeState {
            native_runtime: true,
            spacetime: Some(crate::state::ProcessRecord {
                pid: 42,
                identity: "native host".into(),
            }),
            ..Default::default()
        }
        .save(&project.state_file())
        .unwrap();
        let stack = FakeStack::new()
            .with_process(42, "native host")
            .with_port(3000)
            .fail_on("debug_repair_after_publish", "operator only");
        let error = publish_and_repair(&project, &stack.runner(), &stack.inspector(), "lyracore")
            .unwrap_err()
            .to_string();
        let command = error.split_once("Repair schedules with: ").unwrap().1;
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {command}; printf '%s\\0' \"$@\""))
            .output()
            .unwrap();
        assert!(output.status.success());
        let arguments = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        assert_eq!(
            arguments,
            vec![
                b"spacetime".as_slice(),
                b"--config-path",
                project.native_cli_config().to_str().unwrap().as_bytes(),
                b"call",
                b"--server",
                b"http://127.0.0.1:3000",
                b"lyracore",
                b"debug_repair_after_publish",
                b"",
            ]
        );
    }

    #[test]
    fn native_publish_uses_the_selected_node_and_private_publisher() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        std::fs::create_dir_all(&project.state_dir).unwrap();
        std::fs::write(project.native_cli_config(), "spacetimedb_token = 'fixture'").unwrap();
        let stack = FakeStack::new();
        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            Some("http://127.0.0.1:13000"),
        )
        .unwrap();
        let commands = stack.rendered();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(
            commands[0].contains("publish -s http://127.0.0.1:13000"),
            "{commands:?}"
        );
        assert!(commands[0].contains(&format!(
            "--config-path {}",
            project.native_cli_config().display()
        )));
        assert!(commands[0].ends_with("--yes lyracore"));
    }

    #[test]
    fn native_default_and_explicit_publish_refuse_missing_credentials_and_legacy_state() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        let stack = FakeStack::new();
        let mut state = RuntimeState {
            spacetime: Some(crate::state::ProcessRecord {
                pid: 42,
                identity: "legacy".into(),
            }),
            topology: "sharded".into(),
            ..Default::default()
        };
        state.save(&project.state_file()).unwrap();
        for target in [
            None,
            Some("http://127.0.0.1:3000"),
            Some("http://[::1]:3000"),
            Some("http://127.0.0.1:13000"),
        ] {
            let error = run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &[],
                false,
                target,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("private publisher configuration"),
                "{error}"
            );
        }
        assert!(publish_command(&project, "lyracore").is_err());
        std::fs::write(project.native_cli_config(), "spacetimedb_token = 'fixture'").unwrap();
        for target in [
            None,
            Some("http://127.0.0.1:3000"),
            Some("http://[::1]:3000"),
        ] {
            let error = run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &[],
                false,
                target,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("recorded legacy runtime"),
                "{error}"
            );
        }
        assert!(publish_command(&project, "lyracore").is_err());
        assert!(stack.rendered().is_empty());
        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            Some("http://127.0.0.1:13000"),
        )
        .unwrap();
        state.native_runtime = true;
        state.spacetime.as_mut().unwrap().identity = "native host".into();
        let stack = stack.with_process(42, "native host").with_port(3000);
        state.save(&project.state_file()).unwrap();
        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            None,
        )
        .unwrap();
        let commands = stack.rendered();
        assert_eq!(commands.len(), 2);
        assert!(commands[1].contains("publish -s http://127.0.0.1:3000"));
        assert!(commands
            .iter()
            .all(|command| command.contains("--config-path")));
    }

    #[test]
    fn native_default_target_requires_its_live_record_even_when_another_node_answers() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        std::fs::create_dir_all(&project.state_dir).unwrap();
        std::fs::write(project.native_cli_config(), "spacetimedb_token = 'fixture'").unwrap();
        let stack = FakeStack::new()
            .with_port(3000)
            .with_process(99, "other native host");
        for record in [
            None,
            Some(crate::state::ProcessRecord {
                pid: 42,
                identity: "exited native host".into(),
            }),
            Some(crate::state::ProcessRecord {
                pid: 99,
                identity: "reused pid".into(),
            }),
        ] {
            RuntimeState {
                native_runtime: true,
                spacetime: record,
                ..Default::default()
            }
            .save(&project.state_file())
            .unwrap();
            for server in [None, Some("http://127.0.0.1:3000")] {
                let error = run(
                    &project,
                    &stack.runner(),
                    &stack.inspector(),
                    &[],
                    false,
                    server,
                )
                .unwrap_err();
                assert!(error.to_string().contains("requires"), "{error}");
                assert!(stack.rendered().is_empty());
            }
        }
    }

    #[test]
    fn native_target_requires_a_native_checkout_and_private_credentials() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let stack = FakeStack::new();
        assert!(run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            Some("http://127.0.0.1:13000")
        )
        .is_err());
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        assert!(run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            Some("http://127.0.0.1:13000")
        )
        .is_err());
        assert!(stack.rendered().is_empty());
    }

    #[test]
    fn native_target_refuses_non_loopback_and_ambiguous_addresses() {
        for target in [
            "local",
            "http://localhost:13000",
            "https://127.0.0.1:13000",
            "http://127.0.0.1:0",
            "http://159.69.88.70:13000",
            "http://user:secret@127.0.0.1:13000",
            "http://127.0.0.1:13000/path",
        ] {
            assert!(validate_native_server(target).is_err(), "{target}");
        }
        assert!(validate_native_server("http://127.0.0.1:13000").is_ok());
        assert!(validate_native_server("http://[::1]:13000").is_ok());
    }

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn names(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    // ---- the guard contract, case for case with the shell script's CI job ----

    #[test]
    fn a_bare_dash_c_is_refused_rather_than_forwarded() {
        // `spacetime publish -c` is the destructive wipe. This is the single reason the wrapper
        // exists, so it is asserted FUNCTIONALLY: the refusal happens before any process starts.
        let error = databases(&names(&["-c"])).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("Refusing"), "{error}");
        assert!(error.to_string().contains("DESTRUCTIVE"), "{error}");
    }

    #[test]
    fn every_other_flag_shaped_argument_is_refused_the_same_way() {
        for flag in [
            "--clear",
            "--delete-data",
            "--clear-database",
            "-y",
            "--anything",
            "-",
        ] {
            let error = databases(&names(&[flag])).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "{flag} must be a usage error"
            );
            assert!(error.to_string().contains("Refusing"), "{flag}: {error}");
        }
    }

    #[test]
    fn a_flag_hidden_after_a_valid_database_is_still_refused() {
        // The dangerous shape: `lyracore publish lyracore -c` — one good argument in front of a
        // wipe. Nothing may be published before the whole list has been checked.
        let error = databases(&names(&["lyracore", "-c"])).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(error.to_string().contains("Refusing"), "{error}");
    }

    #[test]
    fn a_flag_cannot_be_smuggled_past_argument_parsing_into_a_rendered_command() {
        // Defence in depth: even a caller constructing the command directly is refused.
        let tmp = TempDir::new().unwrap();
        assert!(publish_command(&project(&tmp), "-c").is_err());
        assert!(publish_command(&project(&tmp), "--delete-data").is_err());
        assert!(publish_command(&project(&tmp), "a database").is_err());
    }

    #[test]
    fn plain_database_names_are_accepted() {
        assert_eq!(
            databases(&names(&["lyracore-world-1", "realm_core"])).unwrap(),
            names(&["lyracore-world-1", "realm_core"])
        );
    }

    #[test]
    fn no_arguments_validate_to_an_empty_list_rather_than_a_guessed_default() {
        // `databases` runs at parse time, before a `ProjectLayout` exists to read the recorded
        // topology from — so it can only reflect what was actually typed. `run` (below) is what
        // turns an empty list into the recorded topology's databases.
        assert_eq!(databases(&[]).unwrap(), Vec::<String>::new());
    }

    // ---- what the rendered command must always contain ----

    #[test]
    fn the_two_flags_the_wrapper_exists_for_are_always_present_and_the_wipe_never_is() {
        let tmp = TempDir::new().unwrap();
        let rendered = publish_command(&project(&tmp), "lyracore")
            .unwrap()
            .render();
        assert!(
            rendered.contains("--build-options=--features=debug_reducers"),
            "{rendered}"
        );
        assert!(rendered.contains("--yes"), "{rendered}");
        let args: Vec<&str> = rendered.split_whitespace().collect();
        for wipe in ["-c", "--clear-database", "--delete-data", "--clear"] {
            assert!(
                !args.contains(&wipe),
                "{wipe} must never be baked in: {rendered}"
            );
        }
        // The server alias is pinned and never re-selected.
        assert!(rendered.contains("-s local"), "{rendered}");
        assert!(!rendered.contains("server set-default"), "{rendered}");
    }

    #[test]
    fn the_module_path_is_absolute_so_the_cwd_cannot_decide_what_is_published() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let rendered = publish_command(&project, "lyracore").unwrap().render();
        assert!(
            rendered.contains(&project.module_dir().to_string_lossy().to_string()),
            "{rendered}"
        );
    }

    // ---- the run ----

    /// A checkout `preflight` passes, so these tests exercise `publish`'s own behaviour.
    fn healthy(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!(
                "[toolchain]\nchannel = \"{}\"\n",
                crate::proc::fake::FAKE_RUST_VERSION
            ),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!(
                "spacetimedb = {{ version = \"={}\" }}\n",
                crate::proc::fake::FAKE_SPACETIME_VERSION
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        ProjectLayout::from_root(root).unwrap()
    }

    fn loot_receipt_upgrade(tmp: &TempDir) -> ProjectLayout {
        let project = healthy(tmp);
        let stack = FakeStack::new().with_generated_binding(
            "game_loot_roll_promotion_receipt_table.rs",
            "use super::character_type::Character;\n/// Table handle for the table `game_loot_roll_promotion_receipt`.\nimpl __sdk::Table for ReceiptTableHandle<'_> {\n    type Row = Character;\n}\n",
        );
        crate::cmd::bindings::ensure(&project, &stack.runner()).unwrap();
        project
    }

    fn schema(tables: &[&str]) -> String {
        serde_json::json!({
            "typespace": { "types": [] },
            "tables": tables
                .iter()
                .map(|name| serde_json::json!({ "name": name }))
                .collect::<Vec<_>>(),
            "reducers": [],
        })
        .to_string()
    }

    fn clean_old_schema_stack() -> FakeStack {
        FakeStack::new()
            .with_stdout("spacetime describe", &schema(&["game_loot_roll"]))
            .with_stdout("FROM game_loot_roll WHERE", " n\n 0\n")
            .with_stdout("FROM game_loot_roll_vote WHERE", " n\n 0\n")
            .with_stdout("FROM game_corpse_loot WHERE", " n\n 0\n")
    }

    #[test]
    fn the_receipt_upgrade_checks_every_destination_before_any_publish() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let stack = clean_old_schema_stack();
        let wanted = names(&["lyracore", "lyracore-world-1", "realm-core"]);

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &wanted,
            true,
            None,
        )
        .unwrap();

        let rendered = stack.rendered();
        let first_publish = rendered
            .iter()
            .position(|command| command.starts_with("spacetime publish"))
            .unwrap();
        assert_eq!(
            rendered[..first_publish]
                .iter()
                .filter(|command| command.starts_with("spacetime describe"))
                .count(),
            wanted.len()
        );
        assert_eq!(
            rendered[..first_publish]
                .iter()
                .filter(|command| command.starts_with("spacetime sql"))
                .count(),
            wanted.len() * LOOT_DRAIN_QUERIES.len()
        );
    }

    #[test]
    fn direct_run_validates_every_destination_before_any_live_read() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let stack = FakeStack::new();

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore", "--delete-data"]),
            true,
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn every_legacy_loot_state_blocks_the_whole_publish() {
        for (blocked_query, state) in [
            ("FROM game_loot_roll WHERE", "active or staging Loot Rolls"),
            ("FROM game_loot_roll_vote WHERE", "Loot Roll votes"),
            ("FROM game_corpse_loot WHERE", "withheld corpse loot"),
        ] {
            let tmp = TempDir::new().unwrap();
            let project = loot_receipt_upgrade(&tmp);
            let stack = clean_old_schema_stack().with_stdout(blocked_query, " n\n 1\n");

            let error = run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &names(&["lyracore", "realm-core"]),
                true,
                None,
            )
            .unwrap_err();

            assert!(error.to_string().contains(state), "{state}: {error}");
            assert!(
                !stack
                    .rendered()
                    .iter()
                    .any(|command| command.starts_with("spacetime publish")),
                "{state}: {:?}",
                stack.rendered()
            );
        }
    }

    #[test]
    fn an_unreadable_or_malformed_live_state_refuses_before_publish() {
        let cases = [
            FakeStack::new().fail_on("spacetime describe", "database is missing"),
            FakeStack::new().with_stdout("spacetime describe", "not json"),
            FakeStack::new().with_stdout("spacetime describe", r#"{"tables":"unknown"}"#),
            FakeStack::new()
                .with_stdout("spacetime describe", &schema(&["game_loot_roll"]))
                .with_stdout("FROM game_loot_roll WHERE", "not a count"),
        ];
        for stack in cases {
            let tmp = TempDir::new().unwrap();
            let project = loot_receipt_upgrade(&tmp);

            let error = run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &names(&["lyracore"]),
                true,
                None,
            )
            .unwrap_err();

            assert!(
                error.to_string().contains("Nothing was published"),
                "{error}"
            );
            assert!(
                !stack
                    .rendered()
                    .iter()
                    .any(|command| command.starts_with("spacetime publish")),
                "{:?}",
                stack.rendered()
            );
        }
    }

    #[test]
    fn only_an_actual_receipt_table_marks_a_destination_upgraded() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let decoy = serde_json::json!({
            "tables": [{ "name": "game_loot_roll" }],
            "reducers": [{ "description": LOOT_RECEIPT_TABLE }],
        })
        .to_string();
        let stack = clean_old_schema_stack()
            .with_stdout("spacetime describe", &decoy)
            .with_stdout("FROM game_loot_roll WHERE", " n\n 1\n");

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("active or staging Loot Rolls"),
            "{error}"
        );
        assert!(stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime sql")));
    }

    #[test]
    fn an_ambiguous_receipt_table_schema_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let stack = FakeStack::new().with_stdout(
            "spacetime describe",
            &schema(&[LOOT_RECEIPT_TABLE, LOOT_RECEIPT_TABLE]),
        );

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("more than once"), "{error}");
        assert!(!stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime publish")));
    }

    #[test]
    fn an_already_upgraded_destination_needs_no_legacy_drain_query() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let stack =
            FakeStack::new().with_stdout("spacetime describe", &schema(&[LOOT_RECEIPT_TABLE]));

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap();

        assert!(!stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime sql")));
        assert!(stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime publish")));
    }

    #[test]
    fn a_mixed_batch_checks_legacy_state_only_on_destinations_still_needing_the_upgrade() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);
        let stack = FakeStack::new()
            .with_stdout(
                "spacetime describe --json -s local lyracore",
                &schema(&[LOOT_RECEIPT_TABLE]),
            )
            .with_stdout(
                "spacetime describe --json -s local realm-core",
                &schema(&["game_loot_roll"]),
            )
            .with_stdout("FROM game_loot_roll WHERE", " n\n 0\n")
            .with_stdout("FROM game_loot_roll_vote WHERE", " n\n 0\n")
            .with_stdout("FROM game_corpse_loot WHERE", " n\n 0\n");

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore", "realm-core"]),
            true,
            None,
        )
        .unwrap();

        let rendered = stack.rendered();
        assert_eq!(
            rendered
                .iter()
                .filter(|command| command.starts_with("spacetime describe"))
                .count(),
            2
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|command| command.starts_with("spacetime sql"))
                .count(),
            LOOT_DRAIN_QUERIES.len()
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|command| command.starts_with("spacetime publish"))
                .count(),
            2
        );
    }

    #[test]
    fn skip_preflight_cannot_skip_the_live_loot_upgrade_check() {
        let tmp = TempDir::new().unwrap();
        let project = loot_receipt_upgrade(&tmp);

        let stack = clean_old_schema_stack()
            .fail_on("cargo check", "preflight must be skipped")
            .with_stdout("FROM game_loot_roll WHERE", " n\n 1\n");

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("active or staging Loot Rolls"),
            "{error}"
        );
        assert!(!stack
            .rendered()
            .iter()
            .any(|command| command.contains("cargo check")));
        assert!(!stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime publish")));
    }

    #[test]
    fn ordinary_publishes_without_the_receipt_table_keep_the_existing_path() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let stack = FakeStack::new().fail_on("spacetime describe", "must not inspect live state");

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap();

        assert_eq!(
            stack
                .rendered()
                .iter()
                .filter(|command| command.starts_with("spacetime publish"))
                .count(),
            1
        );
    }

    #[test]
    fn failed_schema_preparation_stops_before_live_reads_or_publish() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let stack = FakeStack::new().fail_on("spacetime generate", "bad Module");
        assert!(run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None
        )
        .is_err());
        assert!(!stack
            .rendered()
            .iter()
            .any(|command| command.starts_with("spacetime describe")
                || command.starts_with("spacetime publish")));
    }

    // ---- bare `publish` resolves against the recorded topology, not a hardcoded default ----

    fn record_active_topology(project: &ProjectLayout, topology: Topology) {
        let state = crate::state::RuntimeState {
            gateway: Some(crate::state::ProcessRecord {
                pid: 42,
                identity: "recorded fixture gateway".to_string(),
            }),
            topology: topology.as_str().to_string(),
            ..Default::default()
        };
        state.save(&project.state_file()).unwrap();
    }

    #[test]
    fn bare_publish_targets_every_database_of_an_active_sharded_fixture() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        record_active_topology(&project, Topology::Sharded);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(
            published.len(),
            crate::project::Topology::Sharded.databases().len(),
            "{published:?}"
        );
        for database in crate::project::Topology::Sharded.databases() {
            assert!(
                published.iter().any(|r| r.ends_with(database)),
                "{database} missing from {published:?}"
            );
        }
    }

    #[test]
    fn bare_publish_targets_the_one_database_of_an_active_single_fixture() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        record_active_topology(&project, Topology::Single);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(published.len(), 1, "{published:?}");
        assert!(
            published[0].ends_with(ProjectLayout::DATABASE),
            "{published:?}"
        );
    }

    #[test]
    fn bare_publish_with_no_recorded_state_uses_the_default_sharded_topology() {
        // A fresh clone that has never run `dev up` has no `state.json` at all. It must publish
        // what its first `dev up` will build, not the single legacy fixture.
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(
            published.len(),
            crate::project::Topology::Sharded.databases().len(),
            "{published:?}"
        );
    }

    #[test]
    fn bare_publish_ignores_the_stale_topology_of_a_stopped_fixture() {
        // `dev down` clears the gateway record but retains the last topology for diagnostics. That
        // stale value must not turn "no active fixture" back into the unsafe one-database default.
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let state = crate::state::RuntimeState {
            topology: Topology::Single.as_str().to_string(),
            ..Default::default()
        };
        state.save(&project.state_file()).unwrap();
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &[],
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(published.len(), Topology::Sharded.databases().len());
    }

    #[test]
    fn explicit_names_are_never_overridden_by_the_recorded_topology() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        record_active_topology(&project, Topology::Sharded);
        let stack = FakeStack::new();

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["realm-core"]),
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(published.len(), 1, "{published:?}");
        assert!(published[0].ends_with("realm-core"), "{published:?}");
    }

    #[test]
    fn several_databases_publish_in_order() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        let wanted = names(&["lyracore", "lyracore-world-1", "realm-core"]);
        run(
            &healthy(&tmp),
            &stack.runner(),
            &stack.inspector(),
            &wanted,
            true,
            None,
        )
        .unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(published.len(), 3, "{published:?}");
        for (rendered, database) in published.iter().zip(&wanted) {
            assert!(rendered.ends_with(database), "{rendered}");
            assert!(rendered.contains("--yes"), "{rendered}");
            assert!(
                rendered.contains("--features=debug_reducers"),
                "every shard gets the deploy features, not just the first: {rendered}"
            );
        }
    }

    #[test]
    fn a_failed_shard_stops_the_run_instead_of_leaving_a_half_published_realm() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new().fail_on("lyracore-world-1", "migration rejected");
        let wanted = names(&["lyracore", "lyracore-world-1", "realm-core"]);

        let error = run(
            &healthy(&tmp),
            &stack.runner(),
            &stack.inspector(),
            &wanted,
            true,
            None,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        let message = error.to_string();
        assert!(
            message.contains("publish stopped at: lyracore-world-1"),
            "{message}"
        );
        assert!(message.contains("completed: lyracore\n"), "{message}");
        assert!(message.contains("not attempted: realm-core"), "{message}");
        assert!(
            !stack.rendered().iter().any(|r| r.ends_with("realm-core")),
            "the run must stop at the failure: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn preflight_runs_first_and_a_failing_one_publishes_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        std::fs::write(project.rust_toolchain_file(), "channel = \"1.0.0\"\n").unwrap();

        let stack = FakeStack::new();

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            false,
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime publish")),
            "a failing preflight must publish nothing: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn publish_refuses_a_stale_package_delta_artifact() {
        // LyraCore#316: preflight's identity gate refuses a generated artifact with no Build
        // Identity sidecar (it predates identity tracking, so it is stale by definition) — and
        // `publish` runs preflight first, so nothing reaches a Shard.
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        let generated = project.packages_dir().join("fire_nova/data/.generated");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(
            generated.join("spell.json"),
            format!(
                r#"{{"version":1,"package":"fire_nova","source_hash":"{}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#,
                "a".repeat(64)
            ),
        )
        .unwrap();
        let stack = FakeStack::new();

        let error = run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            false,
            None,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime publish")),
            "a stale artifact must publish nothing: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn preflight_precedes_the_publish_rather_than_following_it() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        run(
            &healthy(&tmp),
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            false,
            None,
        )
        .unwrap();

        let rendered = stack.rendered();
        let first_publish = rendered
            .iter()
            .position(|r| r.contains("spacetime publish"))
            .expect("published");
        let last_check = rendered
            .iter()
            .rposition(|r| r.contains("cargo check") || r.contains("spacetime generate"))
            .expect("preflighted");
        assert!(last_check < first_publish, "{rendered:?}");
    }

    #[test]
    fn skip_preflight_is_the_only_way_past_the_gate() {
        let tmp = TempDir::new().unwrap();
        let project = healthy(&tmp);
        // A Module build preflight would refuse.
        let stack = FakeStack::new().fail_on("cargo check", "invalid native build");
        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &names(&["lyracore"]),
            true,
            None,
        )
        .unwrap();
        assert!(
            stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime publish")),
            "{:?}",
            stack.rendered()
        );
        assert!(
            !stack.rendered().iter().any(|r| r.contains("cargo check")),
            "--skip-preflight must actually skip it: {:?}",
            stack.rendered()
        );
    }
}
