//! The local single-realm lifecycle: `dev up | status | logs | down`.
//!
//! Scope is the seeded loopback fixture — ONE database. The five-database production recipe in
//! `docs/danger-zones.md` §3 is deliberately not reproduced here, and the variables that would
//! enable it are actively unset for the child gateway.

use crate::project::{Component, ProjectLayout};
use crate::proc::{CommandSpec, ProcessInspector, ProcessRunner};
use crate::state::{ProcessRecord, RuntimeState};
use crate::{Error, Result};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Variables that would turn the single-database fixture into a multi-shard gateway. A
/// contributor who has the production recipe exported must still get the fixture.
const PRODUCTION_TOPOLOGY_VARS: [&str; 4] = [
    "LYRACORE_SHARD_MAP",
    "LYRACORE_SHARD_MAP_FILE",
    "LYRACORE_REALM_CORE",
    "LYRACORE_REGION_SHARDS",
];

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub enum ComponentStatus {
    /// Nothing recorded and nothing answering.
    Stopped,
    /// Our process is alive but not answering its port yet.
    Starting,
    /// Our process is alive and answering.
    Healthy,
    /// Recorded, but wrong — with the reason.
    Unhealthy(String),
    /// Answering, but not started by us. Used, never owned, never stopped.
    External,
}

/// What `down` may do with a recorded process. Pure so every branch — including the refusal —
/// is unit-testable without a real process table.
#[derive(Debug, PartialEq, Eq)]
pub enum StopAction {
    NothingRecorded,
    Stop(u32),
    /// The PID is gone; just drop the record.
    AlreadyGone(u32),
    /// The PID exists but is somebody else's now. Never signal it.
    Refuse {
        pid: u32,
        expected: String,
        found: String,
    },
}

/// Decide what `down` should do, given the record and who actually owns that PID now.
pub fn stop_action(record: Option<&ProcessRecord>, live_identity: Option<&str>) -> StopAction {
    match (record, live_identity) {
        (None, _) => StopAction::NothingRecorded,
        (Some(record), None) => StopAction::AlreadyGone(record.pid),
        (Some(record), Some(found)) if found == record.identity => StopAction::Stop(record.pid),
        (Some(record), Some(found)) => StopAction::Refuse {
            pid: record.pid,
            expected: record.identity.clone(),
            found: found.to_string(),
        },
    }
}

pub struct DevManager {
    project: ProjectLayout,
    state: RuntimeState,
}

impl DevManager {
    pub fn new(project: ProjectLayout) -> Result<Self> {
        let state = RuntimeState::load(&project.state_file())?;
        Ok(Self { project, state })
    }

    fn status_for(
        &self,
        component: Component,
        inspector: &dyn ProcessInspector,
    ) -> ComponentStatus {
        let record = self.state.record(component);
        let live = record.and_then(|r| inspector.identity(r.pid));
        let serving = inspector.port_serving(component.health_port());
        classify(record, live.as_deref(), serving)
    }

    // ---- up ----

    pub fn up(&mut self, runner: &dyn ProcessRunner, inspector: &dyn ProcessInspector) -> Result<()> {
        self.project.ensure_dirs()?;
        self.state.database = ProjectLayout::DATABASE.to_string();

        let spacetime = self.status_for(Component::Spacetime, inspector);
        let gateway = self.status_for(Component::Gateway, inspector);
        if matches!(spacetime, ComponentStatus::Healthy | ComponentStatus::External)
            && gateway == ComponentStatus::Healthy
        {
            println!("dev stack already up — nothing to do.");
            self.print_status(inspector);
            return Ok(());
        }

        self.ensure_spacetime(runner, inspector, &spacetime)?;
        self.build_gateway(runner)?;
        self.publish(runner)?;
        self.claim_operator(runner)?;
        self.ensure_gateway(runner, inspector, &gateway)?;

        self.state.save(&self.project.state_file())?;
        println!("✓ dev stack is up.");
        self.print_status(inspector);
        Ok(())
    }

    fn ensure_spacetime(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        current: &ComponentStatus,
    ) -> Result<()> {
        match current {
            ComponentStatus::Healthy | ComponentStatus::Starting => {
                println!("· SpacetimeDB already running (ours) — reusing it.");
                return Ok(());
            }
            ComponentStatus::External => {
                // Somebody else's node on :3000. Use it, but never record it — `down` must not
                // be able to stop a server this CLI did not start.
                println!(
                    "· SpacetimeDB already listening on {} (not started by this CLI) — reusing, \
                     and it will be left running on `dev down`.",
                    ProjectLayout::stdb_listen()
                );
                return Ok(());
            }
            _ => {}
        }

        println!("· starting SpacetimeDB on {}...", ProjectLayout::stdb_listen());
        let cmd = CommandSpec::new("spacetime")
            .arg("start")
            .arg("--listen-addr")
            .arg(ProjectLayout::stdb_listen());
        let record = self.spawn_recorded(Component::Spacetime, &cmd, runner, inspector)?;
        self.state.set(Component::Spacetime, Some(record));
        self.wait_for_port(Component::Spacetime, inspector)?;
        Ok(())
    }

    fn build_gateway(&self, runner: &dyn ProcessRunner) -> Result<()> {
        println!("· building the gateway...");
        runner.run_and_wait(
            &CommandSpec::new("cargo")
                .arg("build")
                .arg("-p")
                .arg(ProjectLayout::GATEWAY_PACKAGE),
        )?;
        Ok(())
    }

    fn publish(&self, runner: &dyn ProcessRunner) -> Result<()> {
        let script = self.project.publish_script();
        if !script.exists() {
            return Err(Error::ProjectLayout(format!(
                "{} is missing — this does not look like a full checkout",
                ProjectLayout::PUBLISH_SCRIPT
            )));
        }
        println!("· publishing {}...", ProjectLayout::DATABASE);
        // Always through the authoritative script: it is what guarantees --features=debug_reducers,
        // --yes, `-s local`, and the refusal to forward a `-c` wipe.
        runner.run_and_wait(
            &CommandSpec::new("bash")
                .arg(script.to_string_lossy().to_string())
                .arg(ProjectLayout::DATABASE),
        )?;
        Ok(())
    }

    fn claim_operator(&self, runner: &dyn ProcessRunner) -> Result<()> {
        println!("· claiming the operator identity...");
        // Idempotent for the same identity, so repeated `dev up` is not an error.
        runner.run_and_wait(
            &CommandSpec::new("spacetime")
                .arg("call")
                .arg("-s")
                .arg(ProjectLayout::STDB_SERVER)
                .arg(ProjectLayout::DATABASE)
                .arg("claim_operator"),
        )?;
        Ok(())
    }

    fn ensure_gateway(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        current: &ComponentStatus,
    ) -> Result<()> {
        if matches!(current, ComponentStatus::Healthy | ComponentStatus::Starting) {
            println!("· gateway already running — reusing it.");
            return Ok(());
        }
        if *current == ComponentStatus::External {
            // Unlike SpacetimeDB, a foreign gateway must NOT be adopted: we cannot know what build
            // or topology it is running. Starting ours anyway would fail to bind, and the health
            // probe would then pass against *their* listener and record a dead PID as healthy.
            return Err(Error::Process(format!(
                "port {} is already served by a gateway this CLI did not start — stop it first, \
                 or run `lyracore dev down --forget` if it is stale state from an earlier run",
                ProjectLayout::WORLD_PORT
            )));
        }
        println!("· starting the gateway on {}...", ProjectLayout::world_bind());
        let record = self.spawn_recorded(Component::Gateway, &gateway_command(&self.project), runner, inspector)?;
        self.state.set(Component::Gateway, Some(record));
        self.wait_for_port(Component::Gateway, inspector)?;
        Ok(())
    }

    fn spawn_recorded(
        &self,
        component: Component,
        cmd: &CommandSpec,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
    ) -> Result<ProcessRecord> {
        let log = self.project.log_file(component);
        let pid = runner.spawn_logged(cmd, &log)?;
        let identity = inspector.identity(pid).ok_or_else(|| {
            Error::Process(format!(
                "{} exited immediately (PID {pid}); see {}",
                component.as_str(),
                log.display()
            ))
        })?;
        Ok(ProcessRecord { pid, identity })
    }

    fn wait_for_port(
        &self,
        component: Component,
        inspector: &dyn ProcessInspector,
    ) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if inspector.port_serving(component.health_port()) {
                return Ok(());
            }
            // Died during startup — fail now rather than after the full timeout.
            if let Some(record) = self.state.record(component) {
                if inspector.identity(record.pid).as_deref() != Some(record.identity.as_str()) {
                    return Err(Error::Process(format!(
                        "{} exited during startup; see {}",
                        component.as_str(),
                        self.project.log_file(component).display()
                    )));
                }
            }
            sleep(Duration::from_millis(200));
        }
        Err(Error::Process(format!(
            "{} did not answer on port {} within {}s; see {}",
            component.as_str(),
            component.health_port(),
            STARTUP_TIMEOUT.as_secs(),
            self.project.log_file(component).display()
        )))
    }

    // ---- status ----

    pub fn status(&self, inspector: &dyn ProcessInspector) -> Result<()> {
        self.print_status(inspector);
        Ok(())
    }

    fn print_status(&self, inspector: &dyn ProcessInspector) {
        println!("database: {}", ProjectLayout::DATABASE);
        for component in Component::ALL {
            let record = self.state.record(component);
            let pid = record.map(|r| r.pid);
            let line = match self.status_for(component, inspector) {
                ComponentStatus::Stopped => "stopped".to_string(),
                ComponentStatus::Starting => format!(
                    "starting  (PID {}, not yet answering on {})",
                    pid.unwrap_or(0),
                    component.health_port()
                ),
                ComponentStatus::Healthy => {
                    format!("healthy   (PID {}, port {})", pid.unwrap_or(0), component.health_port())
                }
                ComponentStatus::Unhealthy(why) => format!("unhealthy ({why})"),
                ComponentStatus::External => format!(
                    "external  (port {} answers; not started by this CLI)",
                    component.health_port()
                ),
            };
            println!("  {:<10} {}", component.as_str(), line);
        }
    }

    // ---- logs ----

    pub fn logs(&self, component: Option<Component>) -> Result<()> {
        let components = component.map_or_else(|| Component::ALL.to_vec(), |c| vec![c]);
        for component in components {
            let path = self.project.log_file(component);
            println!("=== {} — {} ===", component.as_str(), path.display());
            match std::fs::read_to_string(&path) {
                Ok(content) if content.is_empty() => println!("(empty)"),
                Ok(content) => print!("{content}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("(no log yet — `lyracore dev up` has not started this component)");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    // ---- down ----

    pub fn down(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        forget: bool,
    ) -> Result<()> {
        // Gateway first: it holds connections to the database.
        let mut refusal = None;
        for component in [Component::Gateway, Component::Spacetime] {
            let record = self.state.record(component);
            let live = record.and_then(|r| inspector.identity(r.pid));
            match stop_action(record, live.as_deref()) {
                StopAction::NothingRecorded => {}
                StopAction::AlreadyGone(pid) => {
                    println!("· {} PID {pid} is already gone — clearing it.", component.as_str());
                    self.state.set(component, None);
                }
                StopAction::Stop(pid) => {
                    println!("· stopping {} (PID {pid})...", component.as_str());
                    runner.terminate(pid)?;
                    self.state.set(component, None);
                }
                StopAction::Refuse {
                    pid,
                    expected,
                    found,
                } => {
                    if forget {
                        println!(
                            "· forgetting {} PID {pid} (now {found}) without signalling it.",
                            component.as_str()
                        );
                        self.state.set(component, None);
                    } else {
                        refusal.get_or_insert(Error::ForeignProcess {
                            component: component.as_str(),
                            pid,
                            expected,
                            found,
                        });
                    }
                }
            }
        }

        self.state.save(&self.project.state_file())?;
        match refusal {
            Some(error) => Err(error),
            None => {
                println!("✓ dev stack stopped.");
                Ok(())
            }
        }
    }
}

/// The single-database fixture gateway. Only `LYRACORE_DATABASE` is configured, which — per
/// `gateway/src/config.rs` — collapses routing to that one database.
fn gateway_command(project: &ProjectLayout) -> CommandSpec {
    let mut cmd = CommandSpec::new(project.gateway_bin().to_string_lossy().to_string())
        .env("LYRACORE_DATABASE", ProjectLayout::DATABASE)
        .env("LYRACORE_SPACETIMEDB_URL", ProjectLayout::stdb_uri())
        .env("LYRACORE_LOGON_BIND", ProjectLayout::logon_bind())
        .env("LYRACORE_WORLD_BIND", ProjectLayout::world_bind())
        .env("LYRACORE_AOI", "1")
        .env("MALLOC_ARENA_MAX", "2")
        .env("RUST_LOG", "info");
    for var in PRODUCTION_TOPOLOGY_VARS {
        cmd = cmd.env_remove(var);
    }
    cmd
}

fn classify(
    record: Option<&ProcessRecord>,
    live_identity: Option<&str>,
    port_serving: bool,
) -> ComponentStatus {
    match (record, live_identity) {
        (None, _) if port_serving => ComponentStatus::External,
        (None, _) => ComponentStatus::Stopped,
        (Some(record), None) => ComponentStatus::Unhealthy(format!(
            "recorded PID {} is gone; run `lyracore dev down` then `lyracore dev up`",
            record.pid
        )),
        (Some(record), Some(found)) if found != record.identity => ComponentStatus::Unhealthy(
            format!(
                "PID {} has been reused by another process; run `lyracore dev down --forget`",
                record.pid
            ),
        ),
        (Some(_), Some(_)) if port_serving => ComponentStatus::Healthy,
        (Some(_), Some(_)) => ComponentStatus::Starting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeStack};
    use tempfile::TempDir;

    fn record(pid: u32, identity: &str) -> ProcessRecord {
        ProcessRecord {
            pid,
            identity: identity.to_string(),
        }
    }

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        std::fs::write(tmp.path().join(ProjectLayout::PUBLISH_SCRIPT), "#!/bin/sh\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    // ---- the stop-safety contract ----

    #[test]
    fn an_unrecorded_component_is_never_signalled() {
        assert_eq!(stop_action(None, Some("anything")), StopAction::NothingRecorded);
    }

    #[test]
    fn a_matching_identity_is_stopped() {
        let ours = record(42, "Mon Aug 4 10:00 2026 spacetime");
        assert_eq!(
            stop_action(Some(&ours), Some("Mon Aug 4 10:00 2026 spacetime")),
            StopAction::Stop(42)
        );
    }

    #[test]
    fn a_dead_pid_is_cleared_not_signalled() {
        let ours = record(42, "Mon Aug 4 10:00 2026 spacetime");
        assert_eq!(stop_action(Some(&ours), None), StopAction::AlreadyGone(42));
    }

    #[test]
    fn a_reused_pid_is_refused() {
        // The whole reason identity exists: PID 42 is now somebody's editor.
        let ours = record(42, "Mon Aug 4 10:00 2026 spacetime");
        assert_eq!(
            stop_action(Some(&ours), Some("Tue Aug 5 09:00 2026 vim")),
            StopAction::Refuse {
                pid: 42,
                expected: "Mon Aug 4 10:00 2026 spacetime".to_string(),
                found: "Tue Aug 5 09:00 2026 vim".to_string(),
            }
        );
    }

    #[test]
    fn down_refuses_a_foreign_pid_and_kills_nothing() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.state.set(Component::Gateway, Some(record(42, "ours")));

        let stack = FakeStack::new().with_process(42, "somebody-elses-vim");

        let error = dev.down(&stack.runner(), &stack.inspector(), false).unwrap_err();
        assert!(matches!(error, Error::ForeignProcess { pid: 42, .. }));
        assert!(
            stack.terminated().is_empty(),
            "a foreign PID must never be signalled"
        );
        // The record survives so the refusal is not silently lost.
        assert!(dev.state.record(Component::Gateway).is_some());
    }

    #[test]
    fn down_forget_clears_a_foreign_pid_without_signalling_it() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.state.set(Component::Gateway, Some(record(42, "ours")));

        let stack = FakeStack::new().with_process(42, "somebody-elses-vim");

        dev.down(&stack.runner(), &stack.inspector(), true).unwrap();
        assert!(stack.terminated().is_empty());
        assert!(dev.state.record(Component::Gateway).is_none());
    }

    #[test]
    fn down_stops_our_own_processes_gateway_first() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.state.set(Component::Spacetime, Some(record(10, "stdb")));
        dev.state.set(Component::Gateway, Some(record(20, "gw")));

        let stack = FakeStack::new().with_process(10, "stdb").with_process(20, "gw");

        dev.down(&stack.runner(), &stack.inspector(), false).unwrap();
        assert_eq!(stack.terminated(), vec![20, 10]);
        assert!(dev.state.record(Component::Spacetime).is_none());
    }

    #[test]
    fn down_on_a_stopped_stack_succeeds() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        assert!(dev.down(&stack.runner(), &stack.inspector(), false).is_ok());
        assert!(stack.terminated().is_empty());
    }

    // ---- the four status states ----

    #[test]
    fn status_distinguishes_all_four_states() {
        let ours = record(7, "ours");
        assert_eq!(classify(None, None, false), ComponentStatus::Stopped);
        assert_eq!(classify(Some(&ours), Some("ours"), false), ComponentStatus::Starting);
        assert_eq!(classify(Some(&ours), Some("ours"), true), ComponentStatus::Healthy);
        assert!(matches!(
            classify(Some(&ours), None, false),
            ComponentStatus::Unhealthy(_)
        ));
        assert!(matches!(
            classify(Some(&ours), Some("theirs"), true),
            ComponentStatus::Unhealthy(_)
        ));
        assert_eq!(classify(None, None, true), ComponentStatus::External);
    }

    #[test]
    fn unhealthy_diagnostics_name_the_next_command() {
        let ours = record(7, "ours");
        let ComponentStatus::Unhealthy(why) = classify(Some(&ours), None, false) else {
            panic!("expected unhealthy");
        };
        assert!(why.contains("lyracore dev"), "diagnostic must be actionable: {why}");
    }

    // ---- the fixture / safety contract ----

    #[test]
    fn the_gateway_runs_against_exactly_one_database() {
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(&project(&tmp));
        assert_eq!(cmd.env_value("LYRACORE_DATABASE"), Some(ProjectLayout::DATABASE));
        for var in PRODUCTION_TOPOLOGY_VARS {
            assert_eq!(cmd.env_value(var), None, "{var} must not be set");
            assert!(
                cmd.removes_env(var),
                "{var} must be actively unset so an exported production recipe cannot leak in"
            );
        }
    }

    #[test]
    fn the_gateway_binds_loopback_only() {
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(&project(&tmp));
        for var in ["LYRACORE_LOGON_BIND", "LYRACORE_WORLD_BIND"] {
            let bind = cmd.env_value(var).unwrap();
            assert!(bind.starts_with("127.0.0.1:"), "{var} must be loopback, got {bind}");
        }
    }

    #[test]
    fn up_never_wipes_a_database_or_reselects_the_server() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Nothing running: `up` must start both, and spawning is what opens their ports.
        let stack = FakeStack::new();
        dev.up(&stack.runner(), &stack.inspector()).unwrap();

        for rendered in stack.rendered() {
            let args: Vec<&str> = rendered.split_whitespace().collect();
            assert!(!args.contains(&"-c"), "a -c wipe must never be rendered: {rendered}");
            assert!(!args.contains(&"--clear-database"), "no wipe: {rendered}");
            assert!(
                !rendered.contains("server set-default"),
                "the selected server must never be changed: {rendered}"
            );
            assert!(!rendered.contains("delete"), "no database deletion: {rendered}");
        }
    }

    #[test]
    fn up_publishes_only_the_single_seeded_database_through_the_script() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(&stack.runner(), &stack.inspector()).unwrap();

        let publishes: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains(ProjectLayout::PUBLISH_SCRIPT))
            .collect();
        assert_eq!(publishes.len(), 1, "exactly one publish: {publishes:?}");
        for shard in ["spacetime-world-1", "spacetime-world-2", "spacetime-instances", "realm-core"] {
            assert!(
                !publishes[0].contains(shard),
                "the fixture must not touch {shard}: {}",
                publishes[0]
            );
        }
        // And a bare `spacetime publish` never appears — only the authoritative wrapper.
        assert!(
            !stack.rendered().iter().any(|r| r.starts_with("spacetime publish")),
            "publishing must go through {}",
            ProjectLayout::PUBLISH_SCRIPT
        );
    }

    #[test]
    fn up_is_idempotent_when_everything_is_already_healthy() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.state.set(Component::Spacetime, Some(record(10, "stdb")));
        dev.state.set(Component::Gateway, Some(record(20, "gw")));

        let stack = FakeStack::new()
            .with_process(10, "stdb")
            .with_process(20, "gw")
            .with_port(ProjectLayout::STDB_PORT)
            .with_port(ProjectLayout::WORLD_PORT);

        dev.up(&stack.runner(), &stack.inspector()).unwrap();
        assert!(
            stack.calls().is_empty(),
            "a healthy stack must not be restarted, republished, or re-claimed: {:?}",
            stack.calls()
        );
    }

    #[test]
    fn up_reuses_a_spacetimedb_it_did_not_start_and_never_records_it() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Port 3000 answers, but no PID of ours owns it.
        let stack = FakeStack::new().with_port(ProjectLayout::STDB_PORT);

        dev.up(&stack.runner(), &stack.inspector()).unwrap();

        assert!(
            dev.state.record(Component::Spacetime).is_none(),
            "a pre-existing server must never be recorded as ours"
        );
        assert!(
            !stack.rendered().iter().any(|r| r.contains("spacetime start")),
            "must not start a second node"
        );
    }

    #[test]
    fn a_second_up_after_a_partial_start_completes_the_stack() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // SpacetimeDB is ours and healthy; the gateway died.
        dev.state.set(Component::Spacetime, Some(record(10, "stdb")));

        let stack = FakeStack::new()
            .with_process(10, "stdb")
            .with_port(ProjectLayout::STDB_PORT);

        dev.up(&stack.runner(), &stack.inspector()).unwrap();

        assert!(
            !stack.rendered().iter().any(|r| r.contains("spacetime start")),
            "the healthy node must not be restarted"
        );
        assert!(
            stack.calls().iter().any(|c| matches!(c, Call::Spawn { spec, .. }
                if spec.render().contains("gateway"))),
            "the missing gateway must be started"
        );
    }

    #[test]
    fn up_refuses_to_adopt_a_gateway_it_did_not_start() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Both ports answer but nothing is ours. Reusing SpacetimeDB is fine; silently treating a
        // stranger's gateway as our healthy one is not — its build and topology are unknown, and
        // the health probe would happily pass against their listener.
        let stack = FakeStack::new()
            .with_port(ProjectLayout::STDB_PORT)
            .with_port(ProjectLayout::WORLD_PORT);

        let error = dev.up(&stack.runner(), &stack.inspector()).unwrap_err();
        assert!(
            error.to_string().contains(&ProjectLayout::WORLD_PORT.to_string()),
            "the refusal must name the contended port: {error}"
        );
        assert!(
            !stack.calls().iter().any(|c| matches!(c, Call::Spawn { .. })),
            "nothing may be spawned against a foreign listener"
        );
        assert!(dev.state.record(Component::Gateway).is_none());
    }

    #[test]
    fn logs_report_the_file_and_survive_a_missing_one() {
        let tmp = TempDir::new().unwrap();
        let dev = DevManager::new(project(&tmp)).unwrap();
        // No log files exist yet — this must not error.
        assert!(dev.logs(None).is_ok());
        assert!(dev.logs(Some(Component::Gateway)).is_ok());
    }
}
