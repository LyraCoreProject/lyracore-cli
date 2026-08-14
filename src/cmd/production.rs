//! Read-only status for an explicitly named production topology.

use crate::cmd::gateway_log::{is_database_name, GatewayEvidence};
use crate::proc::{CommandSpec, ProcessRunner};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusOptions {
    pub server: String,
    pub gateway_log: PathBuf,
    pub realm_core: String,
    pub databases: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Warn,
    Fail,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Warn => "WARN",
            Outcome::Fail => "FAIL",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Check {
    pub label: String,
    pub outcome: Outcome,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatusReport {
    pub checks: Vec<Check>,
}

impl StatusReport {
    fn check(&mut self, label: impl Into<String>, outcome: Outcome, detail: impl Into<String>) {
        self.checks.push(Check {
            label: label.into(),
            outcome,
            detail: detail.into(),
        });
    }

    pub fn blocking(&self) -> bool {
        self.outcome() == Outcome::Fail
    }

    pub fn outcome(&self) -> Outcome {
        if self
            .checks
            .iter()
            .any(|check| check.outcome == Outcome::Fail)
        {
            Outcome::Fail
        } else if self
            .checks
            .iter()
            .any(|check| check.outcome == Outcome::Warn)
        {
            Outcome::Warn
        } else {
            Outcome::Pass
        }
    }
}

fn describe_command(server: &str, database: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("describe")
        .arg("--json")
        .arg("-s")
        .arg(server)
        .arg(database)
}

fn inventory_command(server: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("list")
        .arg("-s")
        .arg(server)
}

fn inventory_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_once('|').map(|(names, _)| names))
        .flat_map(|names| names.split(','))
        .map(str::trim)
        .filter(|name| is_database_name(name))
        .map(str::to_string)
        .collect()
}

fn same_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|name| right.contains(name))
}

pub fn inspect(options: &StatusOptions, runner: &dyn ProcessRunner) -> StatusReport {
    let mut report = StatusReport::default();
    match runner.run_capturing_stderr(&CommandSpec::new("spacetime").arg("--version")) {
        Ok(version) => report.check("SpacetimeDB CLI", Outcome::Pass, version.trim()),
        Err(error) => report.check("SpacetimeDB CLI", Outcome::Fail, error.to_string()),
    }

    match runner.run_and_wait(&inventory_command(&options.server)) {
        Ok(output) => {
            let inventory = inventory_names(&output);
            for database in &options.databases {
                report.check(
                    format!("inventory {database}"),
                    if inventory.contains(database) {
                        Outcome::Pass
                    } else {
                        Outcome::Fail
                    },
                    if inventory.contains(database) {
                        format!("present in `spacetime list -s {}`", options.server)
                    } else {
                        format!("missing from `spacetime list -s {}`", options.server)
                    },
                );
            }
        }
        Err(error) => report.check("database inventory", Outcome::Fail, error.to_string()),
    }

    for database in &options.databases {
        match runner.run_and_wait(&describe_command(&options.server, database)) {
            Ok(_) => report.check(
                format!("reachability {database}"),
                Outcome::Pass,
                "published schema is reachable",
            ),
            Err(error) => report.check(
                format!("reachability {database}"),
                Outcome::Fail,
                error.to_string(),
            ),
        }
    }

    let log = match std::fs::read_to_string(&options.gateway_log) {
        Ok(log) => log,
        Err(error) => {
            report.check(
                "gateway log",
                Outcome::Fail,
                format!("{}: {error}", options.gateway_log.display()),
            );
            return report;
        }
    };
    let evidence = GatewayEvidence::parse(&log);
    report.check(
        "gateway start",
        if evidence.saw_start {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        if evidence.saw_start {
            "latest gateway-start segment found"
        } else {
            "no `gateway starting:` marker"
        },
    );

    match &evidence.configured {
        Some(configured) if same_set(configured, &options.databases) => {
            report.check("configured topology", Outcome::Pass, configured.join(", "))
        }
        Some(configured) => report.check(
            "configured topology",
            Outcome::Fail,
            format!(
                "expected [{}], gateway logged [{}]",
                options.databases.join(", "),
                configured.join(", ")
            ),
        ),
        None => report.check(
            "configured topology",
            Outcome::Fail,
            "no `shard map active` database list in the latest start",
        ),
    }

    for database in &options.databases {
        let connected = evidence.connected.contains(database);
        report.check(
            format!("connection {database}"),
            if connected {
                Outcome::Pass
            } else {
                Outcome::Fail
            },
            if connected {
                "coordinator connected"
            } else {
                "no coordinator connection marker"
            },
        );
    }

    report.check(
        "realm-core",
        if evidence.realm_core.as_deref() == Some(options.realm_core.as_str()) {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        evidence.realm_core.as_deref().map_or_else(
            || format!("expected {}, no active marker", options.realm_core),
            |found| format!("active on {found}"),
        ),
    );
    report.check(
        "logon listener",
        if evidence.logon_listener.is_some() {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        evidence.logon_listener.as_deref().unwrap_or("absent"),
    );
    report.check(
        "world listener",
        if evidence.world_listener.is_some() {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        evidence.world_listener.as_deref().unwrap_or("absent"),
    );
    report.check(
        "startup errors",
        if evidence.startup_errors == 0 {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        evidence.startup_errors.to_string(),
    );
    report.check(
        "coordinator credential",
        if evidence.coordinator_token_warning {
            Outcome::Fail
        } else {
            Outcome::Pass
        },
        if evidence.coordinator_token_warning {
            "LYRACORE_COORDINATOR_TOKEN is unset; private account and session tables are unavailable"
        } else {
            "no missing-token warning in the latest start"
        },
    );
    report.check(
        "realm address",
        if evidence.realm_address_warning {
            Outcome::Warn
        } else {
            Outcome::Pass
        },
        if evidence.realm_address_warning {
            "public listener plus loopback realm address will bounce remote clients to realm select; run `set_realm_address` on every database"
        } else {
            "no mismatch warning in the latest start"
        },
    );
    report.check(
        "writer occupancy",
        if evidence.metrics_warning {
            Outcome::Warn
        } else {
            Outcome::Pass
        },
        if evidence.metrics_warning {
            "writer occupancy is unmeasured; configure LYRACORE_METRICS_DB_IDS for every shard"
        } else {
            "no missing-metrics warning in the latest start"
        },
    );
    report
}

pub fn report(status: &StatusReport) {
    println!("production status");
    for check in &status.checks {
        println!(
            "  {:<4} {:<24} {}",
            check.outcome.label(),
            check.label,
            check.detail
        );
    }
    println!(
        "production status: {}",
        match status.outcome() {
            Outcome::Pass => "HEALTHY",
            Outcome::Warn => "WARNINGS",
            Outcome::Fail => "FAILED",
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    fn options(tmp: &TempDir, log: &str) -> StatusOptions {
        let gateway_log = tmp.path().join("gateway.log");
        std::fs::write(&gateway_log, log).unwrap();
        StatusOptions {
            server: "http://127.0.0.1:3000".into(),
            gateway_log,
            realm_core: "lyracore-realm".into(),
            databases: vec![
                "lyracore".into(),
                "lyracore-world-1".into(),
                "lyracore-instances".into(),
                "lyracore-realm".into(),
            ],
        }
    }

    fn healthy_log() -> &'static str {
        "gateway starting: current\n\
         coordinator connected to shard lyracore\n\
         coordinator connected to shard lyracore-world-1\n\
         coordinator connected to shard lyracore-instances\n\
         coordinator connected to shard lyracore-realm\n\
         realm-core active: accounts live on lyracore-realm\n\
         shard map active: 4 databases [\"lyracore\", \"lyracore-instances\", \"lyracore-world-1\", \"lyracore-realm\"]\n\
         logon listening on 0.0.0.0:3724\n\
         world listening on 0.0.0.0:8085\n"
    }

    fn inventory() -> &'static str {
        "Associated databases for user deadbeef:\n\n\
         Database Name(s)     | Identity\n\
         ---------------------+---------\n\
         lyracore             | 01\n\
         lyracore-world-1     | 02\n\
         lyracore-instances   | 03\n\
         lyracore-realm       | 04\n"
    }

    fn healthy_stack() -> FakeStack {
        FakeStack::new().with_stdout("spacetime list -s http://127.0.0.1:3000", inventory())
    }

    #[test]
    fn a_real_production_topology_passes_without_fixture_names() {
        let tmp = TempDir::new().unwrap();
        let stack = healthy_stack();
        let status = inspect(&options(&tmp, healthy_log()), &stack.runner());
        assert!(!status.blocking(), "{status:?}");
        assert!(status.checks.iter().any(|check| {
            check.label == "connection lyracore-world-1" && check.outcome == Outcome::Pass
        }));
        assert!(!status
            .checks
            .iter()
            .any(|check| check.label.contains("kalimdor")));
        assert_eq!(
            stack.rendered(),
            vec![
                "spacetime --version",
                "spacetime list -s http://127.0.0.1:3000",
                "spacetime describe --json -s http://127.0.0.1:3000 lyracore",
                "spacetime describe --json -s http://127.0.0.1:3000 lyracore-world-1",
                "spacetime describe --json -s http://127.0.0.1:3000 lyracore-instances",
                "spacetime describe --json -s http://127.0.0.1:3000 lyracore-realm",
            ],
            "production status must remain a read-only inventory probe"
        );
    }

    #[test]
    fn a_configured_but_disconnected_shard_is_a_failure() {
        let tmp = TempDir::new().unwrap();
        let log = healthy_log().replace("coordinator connected to shard lyracore-instances\n", "");
        let status = inspect(&options(&tmp, &log), &healthy_stack().runner());
        assert!(status.blocking());
        assert!(status.checks.iter().any(|check| {
            check.label == "connection lyracore-instances" && check.outcome == Outcome::Fail
        }));
    }

    #[test]
    fn client_routing_and_metrics_gaps_are_warnings_not_false_health() {
        let tmp = TempDir::new().unwrap();
        let log = format!(
            "{}WARN realm advertises 127.0.0.1:8085 but the world listener is bound\n\
             WARN LYRACORE_METRICS_DB_IDS is unset\n",
            healthy_log()
        );
        let status = inspect(&options(&tmp, &log), &healthy_stack().runner());
        assert!(!status.blocking());
        assert_eq!(status.outcome(), Outcome::Warn);
        assert_eq!(
            status
                .checks
                .iter()
                .filter(|check| check.outcome == Outcome::Warn)
                .count(),
            2
        );
        let realm_address = status
            .checks
            .iter()
            .find(|check| check.label == "realm address")
            .unwrap();
        assert!(realm_address.detail.contains("bounce"), "{realm_address:?}");
        assert!(
            realm_address.detail.contains("set_realm_address"),
            "{realm_address:?}"
        );
        assert!(
            realm_address.detail.contains("every database"),
            "{realm_address:?}"
        );
    }

    #[test]
    fn an_unreachable_database_is_distinct_from_a_missing_connection() {
        let tmp = TempDir::new().unwrap();
        let stack = healthy_stack().fail_on(
            "spacetime describe --json -s http://127.0.0.1:3000 lyracore-world-1",
            "not found",
        );
        let status = inspect(&options(&tmp, healthy_log()), &stack.runner());
        assert!(status.checks.iter().any(|check| {
            check.label == "reachability lyracore-world-1" && check.outcome == Outcome::Fail
        }));
        assert!(status.checks.iter().any(|check| {
            check.label == "connection lyracore-world-1" && check.outcome == Outcome::Pass
        }));
    }

    #[test]
    fn missing_inventory_is_distinct_from_database_reachability() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new().with_stdout(
            "spacetime list -s http://127.0.0.1:3000",
            &inventory().replace("lyracore-world-1     | 02\n", ""),
        );
        let status = inspect(&options(&tmp, healthy_log()), &stack.runner());
        assert!(status.checks.iter().any(|check| {
            check.label == "inventory lyracore-world-1" && check.outcome == Outcome::Fail
        }));
        assert!(status.checks.iter().any(|check| {
            check.label == "reachability lyracore-world-1" && check.outcome == Outcome::Pass
        }));
    }

    #[test]
    fn missing_coordinator_token_and_fatal_log_lines_are_blocking() {
        let tmp = TempDir::new().unwrap();
        let log = format!(
            "{}WARN LYRACORE_COORDINATOR_TOKEN is unset; private tables are unavailable\n\
             ERROR coordinator startup failed\n\
             Error: gateway task exited\n",
            healthy_log()
        );
        let status = inspect(&options(&tmp, &log), &healthy_stack().runner());
        assert!(status.checks.iter().any(|check| {
            check.label == "coordinator credential" && check.outcome == Outcome::Fail
        }));
        assert!(status.checks.iter().any(|check| {
            check.label == "startup errors" && check.outcome == Outcome::Fail && check.detail == "2"
        }));
    }
}
