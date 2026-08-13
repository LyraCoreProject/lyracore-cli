//! Read-only status for an explicitly named production topology.

use crate::cmd::gateway_log::GatewayEvidence;
use crate::proc::{CommandSpec, ProcessRunner};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusOptions {
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
        self.checks
            .iter()
            .any(|check| check.outcome == Outcome::Fail)
    }
}

fn describe_command(database: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("describe")
        .arg("--json")
        .arg("-s")
        .arg("local")
        .arg(database)
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

    for database in &options.databases {
        match runner.run_and_wait(&describe_command(database)) {
            Ok(_) => report.check(database, Outcome::Pass, "published schema is reachable"),
            Err(error) => report.check(database, Outcome::Fail, error.to_string()),
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
        "realm address",
        if evidence.realm_address_warning {
            Outcome::Warn
        } else {
            Outcome::Pass
        },
        if evidence.realm_address_warning {
            "gateway reported an advertised-address/listener mismatch"
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
            "LYRACORE_METRICS_DB_IDS is absent or occupancy is unmeasured"
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
        if status.blocking() {
            "FAILED"
        } else {
            "HEALTHY"
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

    #[test]
    fn a_real_production_topology_passes_without_fixture_names() {
        let tmp = TempDir::new().unwrap();
        let status = inspect(&options(&tmp, healthy_log()), &FakeStack::new().runner());
        assert!(!status.blocking(), "{status:?}");
        assert!(status.checks.iter().any(|check| {
            check.label == "connection lyracore-world-1" && check.outcome == Outcome::Pass
        }));
        assert!(!status
            .checks
            .iter()
            .any(|check| check.label.contains("kalimdor")));
    }

    #[test]
    fn a_configured_but_disconnected_shard_is_a_failure() {
        let tmp = TempDir::new().unwrap();
        let log = healthy_log().replace("coordinator connected to shard lyracore-instances\n", "");
        let status = inspect(&options(&tmp, &log), &FakeStack::new().runner());
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
        let status = inspect(&options(&tmp, &log), &FakeStack::new().runner());
        assert!(!status.blocking());
        assert_eq!(
            status
                .checks
                .iter()
                .filter(|check| check.outcome == Outcome::Warn)
                .count(),
            2
        );
    }

    #[test]
    fn an_unreachable_database_is_distinct_from_a_missing_connection() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new().fail_on(
            "spacetime describe --json -s local lyracore-world-1",
            "not found",
        );
        let status = inspect(&options(&tmp, healthy_log()), &stack.runner());
        assert!(status
            .checks
            .iter()
            .any(|check| { check.label == "lyracore-world-1" && check.outcome == Outcome::Fail }));
        assert!(status.checks.iter().any(|check| {
            check.label == "connection lyracore-world-1" && check.outcome == Outcome::Pass
        }));
    }
}
