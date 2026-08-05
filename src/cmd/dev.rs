//! The local single-realm lifecycle: `dev up | status | logs | smoke | down`.
//!
//! Scope is the seeded loopback fixture — ONE database. The five-database production recipe in
//! `docs/danger-zones.md` §3 is deliberately not reproduced here, and the variables that would
//! enable it are actively unset for the child gateway.

use crate::cmd::{preflight, publish};
use crate::harness::{self, Harness};
use crate::http::HttpClient;
use crate::proc::{CommandSpec, ProcessInspector, ProcessRunner};
use crate::project::{ClientBind, Component, ProjectLayout};
use crate::state::{ProcessRecord, RuntimeState};
use crate::token::Credential;
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
    /// How the client-facing listeners are (or are about to be) bound. Read from state, so
    /// `status`/`logs`/`smoke`/`down` all describe the stack that is actually running rather than
    /// assuming loopback; replaced by `up`'s argument once that is checked.
    bind: ClientBind,
}

impl DevManager {
    pub fn new(project: ProjectLayout) -> Result<Self> {
        let state = RuntimeState::load(&project.state_file())?;
        let bind = state.bind();
        Ok(Self {
            project,
            state,
            bind,
        })
    }

    fn status_for(
        &self,
        component: Component,
        inspector: &dyn ProcessInspector,
    ) -> ComponentStatus {
        let record = self.state.record(component);
        let live = record.and_then(|r| inspector.identity(r.pid));
        let serving =
            inspector.serving(&component.health_host(&self.bind), component.health_port());
        classify(record, live.as_deref(), serving)
    }

    // ---- up ----

    pub fn up(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        http: &dyn HttpClient,
        bind: ClientBind,
    ) -> Result<()> {
        self.project.ensure_dirs()?;
        self.state.database = ProjectLayout::DATABASE.to_string();

        let spacetime = self.status_for(Component::Spacetime, inspector);
        // Under the RECORDED bind — "is a gateway of ours running, wherever it was put?".
        self.check_bind_change(&self.status_for(Component::Gateway, inspector), &bind)?;
        self.bind = bind;
        self.state.client_host = self.bind.host();
        // ...and now under the requested one, which is where anything we start must answer.
        let gateway = self.status_for(Component::Gateway, inspector);

        if matches!(
            spacetime,
            ComponentStatus::Healthy | ComponentStatus::External
        ) && gateway == ComponentStatus::Healthy
        {
            println!("dev stack already up — nothing to do.");
            self.print_status(runner, inspector);
            return Ok(());
        }

        self.ensure_spacetime(runner, inspector, &spacetime)?;
        self.build_gateway(runner)?;
        self.publish(runner)?;
        // ONE credential for the rest of this run, resolved here rather than inside each step
        // that needs it: `claim_operator` and the gateway MUST be the same identity, or the
        // module's `require_operator` refuses every provision the gateway makes.
        //
        // After the publish, because on a fresh host `publish` is the step that can leave behind a
        // `spacetime` login there was none of a minute ago — and reusing that is better than
        // minting a second identity. Before the gateway spawn, because a credential we cannot get
        // must be a clear refusal rather than a gateway that starts, warns, and dies 15s later.
        let credential = crate::token::resolve_or_mint(
            runner,
            http,
            &self.project.token_file(),
            &ProjectLayout::stdb_uri(),
        )?;
        self.claim_operator(runner, http, &credential)?;
        self.ensure_gateway(runner, inspector, &gateway, &credential)?;

        self.state.save(&self.project.state_file())?;
        println!("✓ dev stack is up.");
        if let ClientBind::Lan(ip) = &self.bind {
            println!(
                "  LAN mode: clients on this network use realmlist {ip}. SpacetimeDB stays on \
                 127.0.0.1 — only the logon and world ports are reachable from the LAN."
            );
        }
        self.print_status(runner, inspector);
        Ok(())
    }

    /// A running gateway cannot change where it listens. Silently reporting "already up" for a
    /// `--lan` request that a loopback gateway is serving would leave the contributor waiting for
    /// a LAN connection that can never arrive — and the reverse would leave a LAN listener up
    /// after a plain `dev up`.
    fn check_bind_change(&self, gateway: &ComponentStatus, wanted: &ClientBind) -> Result<()> {
        if !matches!(
            gateway,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) || &self.bind == wanted
        {
            return Ok(());
        }
        Err(Error::Process(format!(
            "the running gateway is bound to {} and a running process cannot be rebound; \
             `lyracore dev down` first, then start it the way you want ({}).",
            ProjectLayout::world_bind(&self.bind),
            match wanted {
                ClientBind::Loopback => "lyracore dev up".to_string(),
                ClientBind::Lan(ip) => format!("lyracore dev up --lan {ip}"),
            }
        )))
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

        println!(
            "· starting SpacetimeDB on {}...",
            ProjectLayout::stdb_listen()
        );
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

    /// The offline deploy gate, then the one correct publish.
    ///
    /// Both are this CLI's own (`lyracore preflight` / `lyracore publish`) rather than a shell-out
    /// to the server repo's `scripts/`: the guarantees — `--features=debug_reducers`, `--yes`,
    /// `-s local`, and the unreachability of a `-c` wipe — are properties of
    /// `publish::publish_command`, which is the ONLY way a publish is rendered anywhere in this
    /// CLI. The multi-shard note `lyracore publish` prints is deliberately absent: this fixture is
    /// a single database on purpose.
    fn publish(&self, runner: &dyn ProcessRunner) -> Result<()> {
        println!("· preflight (offline: build, schema, visibility filters)...");
        preflight::run(&self.project, runner)?;
        println!("· publishing {}...", ProjectLayout::DATABASE);
        runner.run_streaming(&publish::publish_command(
            &self.project,
            ProjectLayout::DATABASE,
        )?)?;
        Ok(())
    }

    /// Claim the operator AS the identity the gateway is about to use.
    ///
    /// `claim_operator` is TOFU on `ctx.sender()`: idempotent for the same identity, refused for a
    /// different one. So the caller matters more than the call, and it differs by credential:
    ///
    /// - a `spacetime login` token IS the `spacetime` CLI's identity, so the proven `spacetime
    ///   call` path is kept exactly as it was;
    /// - a server-issued token is an identity the `spacetime` CLI knows nothing about. Shelling
    ///   out would claim the operator for the CLI's identity instead, and the gateway — running as
    ///   the minted one — would then be refused by its own database. It is called with the token.
    fn claim_operator(
        &self,
        runner: &dyn ProcessRunner,
        http: &dyn HttpClient,
        credential: &Credential,
    ) -> Result<()> {
        println!("· claiming the operator identity...");
        if !credential.is_server_issued() {
            runner.run_and_wait(&claim_command())?;
            return Ok(());
        }
        http.post_json(&claim_url(), Some(credential.token()), "[]")
            .map_err(|e| {
                Error::Process(format!(
                    "{e}\n  `claim_operator` was called with the server-issued identity in {}. A \
                     refusal there almost always means this database was claimed by a DIFFERENT \
                     identity (an earlier `spacetime login`, or another checkout): delete that \
                     file and re-run `lyracore dev up` to fall back to that login.",
                    self.project.token_file().display()
                ))
            })?;
        Ok(())
    }

    fn ensure_gateway(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        current: &ComponentStatus,
        credential: &Credential,
    ) -> Result<()> {
        if matches!(
            current,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) {
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
        println!(
            "· starting the gateway on {}...",
            ProjectLayout::world_bind(&self.bind)
        );
        let command = gateway_command(&self.project, &self.bind, credential.token());
        let record = self.spawn_recorded(Component::Gateway, &command, runner, inspector)?;
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

    fn wait_for_port(&self, component: Component, inspector: &dyn ProcessInspector) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let host = component.health_host(&self.bind);
        while Instant::now() < deadline {
            if inspector.serving(&host, component.health_port()) {
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
            "{} did not answer on {}:{} within {}s; see {}",
            component.as_str(),
            host,
            component.health_port(),
            STARTUP_TIMEOUT.as_secs(),
            self.project.log_file(component).display()
        )))
    }

    // ---- status ----

    pub fn status(
        &self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
    ) -> Result<()> {
        self.print_status(runner, inspector);
        Ok(())
    }

    fn print_status(&self, runner: &dyn ProcessRunner, inspector: &dyn ProcessInspector) {
        let spacetime = self.status_for(Component::Spacetime, inspector);
        for component in Component::ALL {
            let record = self.state.record(component);
            let pid = record.map(|r| r.pid);
            let endpoint = format!(
                "{}:{}",
                component.health_host(&self.bind),
                component.health_port()
            );
            let line = match self.status_for(component, inspector) {
                ComponentStatus::Stopped => "stopped".to_string(),
                ComponentStatus::Starting => format!(
                    "starting  (PID {}, not yet answering on {endpoint})",
                    pid.unwrap_or(0),
                ),
                ComponentStatus::Healthy => {
                    format!("healthy   (PID {}, {endpoint})", pid.unwrap_or(0))
                }
                ComponentStatus::Unhealthy(why) => format!("unhealthy ({why})"),
                ComponentStatus::External => {
                    format!("external  ({endpoint} answers; not started by this CLI)")
                }
            };
            println!("  {:<10} {}", component.as_str(), line);
        }
        // The third thing a component can be wrong about: both processes alive, and the database
        // they are supposed to be serving never published (or published to another node). Ports
        // and PIDs both look perfect in that state.
        println!(
            "  {:<10} {}",
            "database",
            self.database_health(runner, &spacetime)
        );
        if let ClientBind::Lan(ip) = &self.bind {
            println!("  {:<10} LAN — clients connect to realmlist {ip}", "bind");
        }
    }

    /// Ask the node about the fixture database itself. `describe` reads the published schema: it
    /// proves the database exists on the node the gateway is pointed at, and it reads no rows, so
    /// it cannot be confused by row-level visibility.
    fn database_health(&self, runner: &dyn ProcessRunner, spacetime: &ComponentStatus) -> String {
        let database = ProjectLayout::DATABASE;
        if matches!(
            spacetime,
            ComponentStatus::Stopped | ComponentStatus::Unhealthy(_)
        ) {
            return format!("{database} (not checked — SpacetimeDB is not serving)");
        }
        match runner.run_and_wait(&describe_command()) {
            Ok(_) => format!("{database} published on {}", ProjectLayout::stdb_uri()),
            Err(e) => format!(
                "{database} UNREACHABLE on {} ({e}) — run `lyracore dev up` to publish it",
                ProjectLayout::stdb_uri()
            ),
        }
    }

    // ---- smoke ----

    /// Hand off to the pinned wire harness's generic login smoke (#246): logon → world handshake →
    /// character enumerate → enter world, against the running fixture.
    ///
    /// The harness is a separate, server-agnostic repository consumed as the RELEASE pinned in
    /// `.wire-harness-rev`, and everything below the seam — the fixtures, the scenarios, the
    /// assertions — belongs to it. What changed with the CLI absorbing the lifecycle scripts is
    /// only WHERE the seam is resolved from: the pinned harness checkout in `.lyracore/`, not a
    /// `adapters/` directory in the server repo. The server repo's copy was the last thing making
    /// `dev smoke` depend on a directory the public mirror does not carry.
    pub fn smoke(
        &self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
    ) -> Result<()> {
        for component in Component::ALL {
            match self.status_for(component, inspector) {
                ComponentStatus::Healthy | ComponentStatus::External => {}
                other => {
                    return Err(Error::Process(format!(
                        "{} is {} — `lyracore dev smoke` needs a running stack; run \
                         `lyracore dev up` first",
                        component.as_str(),
                        match other {
                            ComponentStatus::Stopped => "stopped".to_string(),
                            ComponentStatus::Starting => "still starting".to_string(),
                            ComponentStatus::Unhealthy(why) => format!("unhealthy ({why})"),
                            _ => unreachable!("healthy and external are handled above"),
                        }
                    )))
                }
            }
        }

        // Resolved AFTER the stack check, so a `dev smoke` against a stopped stack does not clone
        // a harness release to tell you to run `dev up`.
        let harness = harness::resolve(
            &self.project,
            runner,
            harness::override_from_env().as_deref(),
            &harness::remote_from_env(),
        )?;
        println!("· building the wire client from the pinned harness...");
        runner.run_and_wait(&client_build_command(&harness))?;

        println!("· running the pinned wire harness's login smoke...");
        runner
            .run_streaming(&smoke_command(&self.project, &harness, &self.bind))
            .map_err(|e| {
                Error::Process(format!(
                "{e}\n  The smoke test signs in as the fixture account. If it failed to log in, \
                 provision it first:\n    printf 'test123' | ./lyracore account create TEST \
                 --password-stdin"
            ))
            })?;
        println!("✓ smoke passed.");
        Ok(())
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
                    println!(
                        "· {} PID {pid} is already gone — clearing it.",
                        component.as_str()
                    );
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

        if self.state.record(Component::Gateway).is_none() {
            // The bind belonged to that process. Leaving it recorded would make the next
            // `dev up` refuse a mode change that nothing is holding any more.
            self.state.client_host = String::new();
            self.bind = ClientBind::Loopback;
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
fn gateway_command(project: &ProjectLayout, bind: &ClientBind, token: &str) -> CommandSpec {
    let mut cmd = CommandSpec::new(project.gateway_bin().to_string_lossy().to_string())
        // The privileged coordinator connection. `game_account`/`game_session` are private module
        // tables and `provision_account` is operator-gated, so without this the gateway connects
        // anonymously and cannot authenticate anyone. Environment, never argv — and `CommandSpec`
        // renders program + args only, so it cannot reach a log line or an error message.
        .env(crate::token::TOKEN_VAR, token)
        .env("LYRACORE_DATABASE", ProjectLayout::DATABASE)
        .env("LYRACORE_SPACETIMEDB_URL", ProjectLayout::stdb_uri())
        .env("LYRACORE_LOGON_BIND", ProjectLayout::logon_bind(bind))
        .env("LYRACORE_WORLD_BIND", ProjectLayout::world_bind(bind))
        // The realm list is answered from the seeded `game_realm` row, which says 127.0.0.1 — a
        // client that reached the logon tier over the LAN would be sent to its OWN loopback for
        // the world tier. This override is what makes `--lan` a working realm rather than a
        // working handshake. It is set in loopback mode too, so the advertised address is a
        // property of how the CLI launched the gateway and not of a row someone may have edited.
        .env("LYRACORE_REALM_ADDRESS", ProjectLayout::realm_address(bind))
        .env("LYRACORE_AOI", "1")
        .env("MALLOC_ARENA_MAX", "2")
        .env("RUST_LOG", "info");
    for var in PRODUCTION_TOPOLOGY_VARS {
        cmd = cmd.env_remove(var);
    }
    cmd
}

/// The TOFU operator claim, for the credential the `spacetime` CLI already carries.
fn claim_command() -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("call")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg(ProjectLayout::DATABASE)
        .arg("claim_operator")
}

/// The same reducer over the node's HTTP API, which is the only way to call it as an identity the
/// `spacetime` CLI does not hold. `[]` is `claim_operator`'s (empty) argument list.
fn claim_url() -> String {
    format!(
        "{}/v1/database/{}/call/claim_operator",
        ProjectLayout::stdb_uri(),
        ProjectLayout::DATABASE
    )
}

/// The database-health probe: schema only, no rows, no writes.
fn describe_command() -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("describe")
        .arg("--json")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg(ProjectLayout::DATABASE)
}

/// Build the harness's wire client from ITS manifest, never from this checkout's workspace —
/// `--manifest-path` so the cwd cannot decide which `Cargo.toml` wins.
fn client_build_command(harness: &Harness) -> CommandSpec {
    CommandSpec::new("cargo")
        .arg("build")
        .arg("-q")
        .arg("--manifest-path")
        .arg(harness.manifest().to_string_lossy().to_string())
        .arg("--bin")
        .arg(ProjectLayout::HARNESS_CLIENT_BIN)
}

/// `dev smoke` — the pinned harness's own seam, told which database and gateway this fixture is
/// and (in LAN mode) which host to connect to. Everything below it belongs to the harness.
///
/// A release that carries its own suite entrypoint is driven through that; otherwise the generic
/// login smoke is driven straight through the adapter seam, which is precisely what the server
/// repo's `run-suite.sh --smoke` did with the flags it was given.
fn smoke_command(project: &ProjectLayout, harness: &Harness, bind: &ClientBind) -> CommandSpec {
    let base = match harness.suite_script() {
        Some(suite) => CommandSpec::new("bash")
            .arg(suite.to_string_lossy().to_string())
            .arg("--smoke")
            .arg("--database")
            .arg(ProjectLayout::DATABASE)
            .arg("--gateway")
            .arg(project.gateway_bin().to_string_lossy().to_string()),
        None => CommandSpec::new("bash")
            .arg(harness.smoke_seam().to_string_lossy().to_string())
            .arg(ProjectLayout::SMOKE_ACCOUNT)
            .arg(ProjectLayout::SMOKE_CHARACTER),
    };
    base.env(
        "WIRE_BIN",
        harness.client_bin().to_string_lossy().to_string(),
    )
    .env("WIRE_HOST", bind.host())
    // The harness resolves TWO roots and this is the one it cannot guess from its own
    // location: it now lives under `.lyracore/`, not inside the checkout it is testing.
    .env("LYRACORE_DIR", project.root.to_string_lossy().to_string())
    .env("DB", ProjectLayout::DATABASE)
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
        (Some(record), Some(found)) if found != record.identity => {
            ComponentStatus::Unhealthy(format!(
                "PID {} has been reused by another process; run `lyracore dev down --forget`",
                record.pid
            ))
        }
        (Some(_), Some(_)) if port_serving => ComponentStatus::Healthy,
        (Some(_), Some(_)) => ComponentStatus::Starting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::{FakeHttp, MINTED_TOKEN};
    use crate::proc::fake::{Call, FakeStack, FAKE_TOKEN};
    use tempfile::TempDir;

    fn record(pid: u32, identity: &str) -> ProcessRecord {
        ProcessRecord {
            pid,
            identity: identity.to_string(),
        }
    }

    const HARNESS_SHA: &str = "30e18083c8df705a484f157bd16a3f12b1aeb5ba";

    /// A checkout the internal preflight passes, so `up` tests exercise `up`.
    fn project(tmp: &TempDir) -> ProjectLayout {
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
            "spacetimedb = { version = \"=2.5.0\" }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        ProjectLayout::from_root(root).unwrap()
    }

    // ---- the stop-safety contract ----

    #[test]
    fn an_unrecorded_component_is_never_signalled() {
        assert_eq!(
            stop_action(None, Some("anything")),
            StopAction::NothingRecorded
        );
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

        let error = dev
            .down(&stack.runner(), &stack.inspector(), false)
            .unwrap_err();
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
        dev.state
            .set(Component::Spacetime, Some(record(10, "stdb")));
        dev.state.set(Component::Gateway, Some(record(20, "gw")));

        let stack = FakeStack::new()
            .with_process(10, "stdb")
            .with_process(20, "gw");

        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
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

    // ---- --lan ----

    fn lan() -> ClientBind {
        ClientBind::parse_lan("192.168.1.50").unwrap()
    }

    #[test]
    fn lan_moves_only_the_client_facing_listeners() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();

        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();

        let gateway = gateway_command(&dev.project, &lan(), FAKE_TOKEN);
        assert_eq!(
            gateway.env_value("LYRACORE_LOGON_BIND"),
            Some("192.168.1.50:3724")
        );
        assert_eq!(
            gateway.env_value("LYRACORE_WORLD_BIND"),
            Some("192.168.1.50:8085")
        );
        // The realm list must send the client to an address it can reach — the seeded
        // `game_realm` row says 127.0.0.1, which is the client's OWN machine over the LAN.
        assert_eq!(
            gateway.env_value("LYRACORE_REALM_ADDRESS"),
            Some("192.168.1.50:8085")
        );
        // The database is the whole reason this flag is narrow: it never leaves loopback.
        assert_eq!(
            gateway.env_value("LYRACORE_SPACETIMEDB_URL"),
            Some("http://127.0.0.1:3000")
        );
        let started_stdb: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("spacetime start"))
            .collect();
        assert_eq!(
            started_stdb,
            vec!["spacetime start --listen-addr 127.0.0.1:3000"]
        );
        assert!(
            !stack.rendered().iter().any(|r| r.contains("192.168.1.50")),
            "no LAN address may reach the database tier: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_lan_gateway_is_health_checked_where_it_actually_listens() {
        // The bug this exists for: probing 127.0.0.1:8085 for a gateway bound to the LAN address
        // reports a perfectly healthy stack as "starting" forever.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();

        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();

        assert_eq!(
            dev.status_for(Component::Gateway, &stack.inspector()),
            ComponentStatus::Healthy
        );
        assert!(
            !stack
                .inspector()
                .serving("127.0.0.1", ProjectLayout::WORLD_PORT),
            "the fake must model a LAN bind as not answering on loopback, or this proves nothing"
        );
    }

    #[test]
    fn the_recorded_bind_survives_into_the_next_command() {
        let tmp = TempDir::new().unwrap();
        let layout_root = tmp.path().to_path_buf();
        let stack = FakeStack::new();
        {
            let mut dev = DevManager::new(project(&tmp)).unwrap();
            dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
                .unwrap();
        }
        // A fresh process (`dev status`) reads state.json and must probe the LAN address.
        let dev = DevManager::new(ProjectLayout::from_root(&layout_root).unwrap()).unwrap();
        assert_eq!(dev.bind, lan());
        assert_eq!(
            dev.status_for(Component::Gateway, &stack.inspector()),
            ComponentStatus::Healthy
        );
    }

    #[test]
    fn a_running_gateway_is_never_silently_left_on_the_wrong_bind() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        // `dev up --lan` onto a running loopback stack: the old "already up — nothing to do" would
        // report success for a LAN realm that is not listening on the LAN at all.
        let error = dev
            .up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap_err();
        assert!(
            error.to_string().contains("dev down"),
            "the refusal must say how to fix it: {error}"
        );
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);

        // ...and after `dev down` the same request is accepted.
        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();
    }

    #[test]
    fn up_is_still_idempotent_in_lan_mode() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();
        let after_first: Vec<String> = stack.rendered();

        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();
        let added: Vec<String> = stack.rendered()[after_first.len()..].to_vec();
        assert!(
            added
                .iter()
                .all(|r| r == "spacetime describe --json -s local lyracore"),
            "the second `up --lan` did more than report status: {added:?}"
        );
    }

    // ---- smoke ----

    /// A checkout with a harness pin, and that pinned release already in the `.lyracore/` cache.
    fn project_with_harness(tmp: &TempDir) -> ProjectLayout {
        let layout = project(tmp);
        std::fs::write(
            layout.wire_harness_pin(),
            format!("v0.1.0-alpha.2 {HARNESS_SHA}\n"),
        )
        .unwrap();
        let cached = layout.harness_cache().join(HARNESS_SHA);
        std::fs::create_dir_all(cached.join(".git")).unwrap();
        std::fs::create_dir_all(cached.join("src")).unwrap();
        std::fs::create_dir_all(cached.join("adapters/lyracore")).unwrap();
        std::fs::write(cached.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(
            cached.join(ProjectLayout::HARNESS_SMOKE_SEAM),
            "#!/bin/sh\n",
        )
        .unwrap();
        layout
    }

    /// A stack whose `git rev-parse` answers the pinned sha, so harness resolution succeeds.
    fn harness_stack() -> FakeStack {
        FakeStack::new().with_stdout("rev-parse HEAD", HARNESS_SHA)
    }

    fn resolved_harness(project: &ProjectLayout, stack: &FakeStack) -> Harness {
        harness::resolve(project, &stack.runner(), None, harness::DEFAULT_REMOTE).unwrap()
    }

    #[test]
    fn smoke_runs_the_seam_out_of_the_pinned_harness_checkout_not_the_server_repo() {
        // #246 + the mirror: `adapters/` is not a directory the published repository carries, so
        // resolving the seam relative to the checkout would make `dev smoke` unrunnable there.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project_with_harness(&tmp)).unwrap();
        let stack = harness_stack();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        dev.smoke(&stack.runner(), &stack.inspector()).unwrap();

        let smoke: Vec<CommandSpec> = stack
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Stream(spec) if spec.render().contains("adapters/lyracore") => Some(spec),
                _ => None,
            })
            .collect();
        assert_eq!(smoke.len(), 1, "exactly one harness run: {smoke:?}");
        let rendered = smoke[0].render();
        assert!(
            rendered.contains(&format!("wire-harness/{HARNESS_SHA}")),
            "the seam must come from the pinned cache: {rendered}"
        );
        assert!(
            rendered.contains(ProjectLayout::HARNESS_SMOKE_SEAM),
            "{rendered}"
        );
        assert!(
            !rendered.starts_with(&tmp.path().join("adapters").display().to_string()),
            "nothing may be resolved out of the server repo's adapters/: {rendered}"
        );
        assert_eq!(smoke[0].env_value("WIRE_HOST"), Some("127.0.0.1"));
        assert_eq!(
            smoke[0].env_value("LYRACORE_DIR"),
            Some(dev.project.root.to_string_lossy().to_string()).as_deref(),
            "the harness resolves two roots; this is the one it cannot guess"
        );
        // The client is built from the HARNESS's manifest, never this workspace's.
        let built = stack
            .rendered()
            .into_iter()
            .find(|r| r.contains("cargo build") && r.contains(ProjectLayout::HARNESS_CLIENT_BIN))
            .expect("the wire client must be built");
        assert!(
            built.contains(&format!("wire-harness/{HARNESS_SHA}")),
            "{built}"
        );
    }

    #[test]
    fn a_release_that_carries_its_own_suite_entrypoint_is_preferred() {
        let tmp = TempDir::new().unwrap();
        let project = project_with_harness(&tmp);
        let cached = project.harness_cache().join(HARNESS_SHA);
        std::fs::write(
            cached.join(ProjectLayout::HARNESS_SUITE_SCRIPT),
            "#!/bin/sh\n",
        )
        .unwrap();
        let mut dev = DevManager::new(project).unwrap();
        let stack = harness_stack();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        dev.smoke(&stack.runner(), &stack.inspector()).unwrap();
        let harness = resolved_harness(&dev.project, &stack);
        let rendered = smoke_command(&dev.project, &harness, &dev.bind).render();
        assert!(rendered.contains("run-suite.sh"), "{rendered}");
        assert!(rendered.contains("--smoke"), "{rendered}");
        assert!(rendered.contains("--database lyracore"), "{rendered}");
    }

    #[test]
    fn smoke_in_lan_mode_connects_to_the_lan_address() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project_with_harness(&tmp)).unwrap();
        let stack = harness_stack();
        dev.up(&stack.runner(), &stack.inspector(), &FakeHttp::new(), lan())
            .unwrap();

        dev.smoke(&stack.runner(), &stack.inspector()).unwrap();
        let harness = resolved_harness(&dev.project, &stack);
        assert_eq!(
            smoke_command(&dev.project, &harness, &dev.bind).env_value("WIRE_HOST"),
            Some("192.168.1.50")
        );
    }

    #[test]
    fn smoke_refuses_a_stack_that_is_not_up_and_says_what_to_run() {
        let tmp = TempDir::new().unwrap();
        let dev = DevManager::new(project_with_harness(&tmp)).unwrap();
        let stack = harness_stack();

        let error = dev.smoke(&stack.runner(), &stack.inspector()).unwrap_err();
        assert!(
            error.to_string().contains("lyracore dev up"),
            "must name the fix: {error}"
        );
        assert!(
            stack.calls().is_empty(),
            "nothing may be run — not even a harness clone — against a stack that is not up"
        );
    }

    #[test]
    fn smoke_on_a_checkout_with_no_harness_pin_fails_cleanly() {
        let tmp = TempDir::new().unwrap();
        // `project()` writes no `.wire-harness-rev`.
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();
        let error = dev.smoke(&stack.runner(), &stack.inspector()).unwrap_err();
        assert!(
            error.to_string().contains(ProjectLayout::WIRE_HARNESS_PIN),
            "{error}"
        );
    }

    #[test]
    fn a_failing_smoke_points_at_the_fixture_account() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project_with_harness(&tmp)).unwrap();
        let stack = harness_stack();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        let failing = harness_stack()
            .with_process(4001, "x")
            .fail_on(ProjectLayout::HARNESS_SMOKE_SEAM, "logon failed");
        // Reuse the running stack's view of the world, but a runner that fails the harness.
        let error = dev
            .smoke(&failing.runner(), &stack.inspector())
            .unwrap_err();
        assert!(
            error.to_string().contains("account create TEST"),
            "the most common smoke failure is an unprovisioned fixture account: {error}"
        );
    }

    // ---- database health ----

    #[test]
    fn status_reports_the_database_not_just_the_processes() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        // Both processes are alive and both ports answer — the state in which an unpublished (or
        // wrong-node) database is invisible to a PID-and-port check.
        let healthy = dev.database_health(&stack.runner(), &ComponentStatus::Healthy);
        assert!(healthy.contains(ProjectLayout::DATABASE), "{healthy}");
        assert!(!healthy.contains("UNREACHABLE"), "{healthy}");

        let broken = FakeStack::new().fail_on("describe", "database not found");
        let unhealthy = dev.database_health(&broken.runner(), &ComponentStatus::Healthy);
        assert!(unhealthy.contains("UNREACHABLE"), "{unhealthy}");
        assert!(
            unhealthy.contains("lyracore dev up"),
            "must be actionable: {unhealthy}"
        );
    }

    #[test]
    fn the_database_probe_reads_schema_and_never_writes() {
        let cmd = describe_command().render();
        assert_eq!(cmd, "spacetime describe --json -s local lyracore");
        for forbidden in [
            "publish",
            "delete",
            "sql",
            "call",
            "-c",
            "server set-default",
        ] {
            assert!(!cmd.contains(forbidden), "{cmd} must not {forbidden}");
        }
    }

    #[test]
    fn a_stopped_database_is_reported_as_unchecked_not_as_broken() {
        let tmp = TempDir::new().unwrap();
        let dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        let line = dev.database_health(&stack.runner(), &ComponentStatus::Stopped);
        assert!(line.contains("not checked"), "{line}");
        assert!(
            stack.calls().is_empty(),
            "no node to ask — the probe must not be attempted"
        );
    }

    // ---- the four status states ----

    #[test]
    fn status_distinguishes_all_four_states() {
        let ours = record(7, "ours");
        assert_eq!(classify(None, None, false), ComponentStatus::Stopped);
        assert_eq!(
            classify(Some(&ours), Some("ours"), false),
            ComponentStatus::Starting
        );
        assert_eq!(
            classify(Some(&ours), Some("ours"), true),
            ComponentStatus::Healthy
        );
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
        assert!(
            why.contains("lyracore dev"),
            "diagnostic must be actionable: {why}"
        );
    }

    // ---- the fixture / safety contract ----

    #[test]
    fn the_gateway_runs_against_exactly_one_database() {
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(&project(&tmp), &ClientBind::Loopback, FAKE_TOKEN);
        assert_eq!(
            cmd.env_value("LYRACORE_DATABASE"),
            Some(ProjectLayout::DATABASE)
        );
        for var in PRODUCTION_TOPOLOGY_VARS {
            assert_eq!(cmd.env_value(var), None, "{var} must not be set");
            assert!(
                cmd.removes_env(var),
                "{var} must be actively unset so an exported production recipe cannot leak in"
            );
        }
    }

    #[test]
    fn the_gateway_gets_the_coordinator_token_and_nothing_loggable_does() {
        // `game_account`/`game_session` are PRIVATE module tables and `provision_account` is
        // operator-gated. A gateway launched without this token connects anonymously, cannot read
        // an account, and dies 15s later on "coordinator subscriptions not applied" — which reads
        // like a node fault, not a credential one. So: it must be set, and it must be set as an
        // ENVIRONMENT variable, never an argument.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        let cmd = gateway_command(&dev.project, &ClientBind::Loopback, FAKE_TOKEN);
        assert_eq!(cmd.env_value(crate::token::TOKEN_VAR), Some(FAKE_TOKEN));
        assert!(
            !cmd.args().iter().any(|a| a.contains(FAKE_TOKEN)),
            "a token in argv is world-readable via `ps`"
        );
        assert!(!cmd.render().contains(FAKE_TOKEN), "{}", cmd.render());

        // Nothing this run could have written to a log or an error message carries it.
        for rendered in stack.rendered() {
            assert!(
                !rendered.contains(FAKE_TOKEN),
                "the coordinator token leaked into: {rendered}"
            );
        }
        // ...nor does the state file the CLI just wrote.
        let state = std::fs::read_to_string(dev.project.state_file()).unwrap();
        assert!(
            !state.contains(FAKE_TOKEN),
            "leaked into state.json: {state}"
        );
    }

    #[test]
    fn a_host_with_no_spacetime_login_mints_a_local_identity_and_uses_it_throughout() {
        // #297: `spacetime login` (2.5.0) offers only the spacetimedb.com browser flow, so
        // requiring it would put a third-party signup in front of `git clone && ./lyracore dev up`.
        // A logged-out host mints a SERVER-ISSUED identity from its own node instead.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
        )
        .unwrap();

        // Minted once, from the loopback node.
        assert_eq!(http.mints().len(), 1, "{:?}", http.requests());
        assert_eq!(http.mints()[0].url, "http://127.0.0.1:3000/v1/identity");

        // ...and the gateway was started with it.
        let gateway: Vec<CommandSpec> = stack
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Spawn { spec, .. } if spec.render().contains("gateway") => Some(spec),
                _ => None,
            })
            .collect();
        assert_eq!(gateway.len(), 1, "{gateway:?}");
        assert_eq!(
            gateway[0].env_value(crate::token::TOKEN_VAR),
            Some(MINTED_TOKEN)
        );

        // Nothing loggable, and nothing on disk except the 0600 credential file, carries it.
        for rendered in stack.rendered() {
            assert!(!rendered.contains(MINTED_TOKEN), "leaked into: {rendered}");
        }
        let state = std::fs::read_to_string(dev.project.state_file()).unwrap();
        assert!(
            !state.contains(MINTED_TOKEN),
            "leaked into state.json: {state}"
        );
    }

    #[test]
    fn a_minted_identity_claims_the_operator_as_itself_not_as_the_spacetime_cli() {
        // The lock-out this prevents: `spacetime call claim_operator` runs as the CLI's identity,
        // which for a minted credential is a DIFFERENT identity — the operator would be claimed by
        // one identity and the gateway would run as another, and every provision would be refused.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
        )
        .unwrap();

        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("claim_operator")),
            "the claim must not be shelled out as the `spacetime` CLI's identity: {:?}",
            stack.rendered()
        );
        let claims: Vec<_> = http
            .requests()
            .into_iter()
            .filter(|r| r.url.ends_with("/call/claim_operator"))
            .collect();
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(
            claims[0].url,
            "http://127.0.0.1:3000/v1/database/lyracore/call/claim_operator"
        );
        assert_eq!(claims[0].bearer.as_deref(), Some(MINTED_TOKEN));
        assert_eq!(claims[0].body, "[]", "claim_operator takes no arguments");
    }

    #[test]
    fn a_logged_in_host_still_claims_through_the_spacetime_cli_and_mints_nothing() {
        // The dev-machine case must be untouched by #297: an existing login is reused, the claim
        // goes through the proven `spacetime call` path, and no credential is written into the
        // checkout.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        let http = FakeHttp::new();

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
        )
        .unwrap();

        assert!(
            stack
                .rendered()
                .iter()
                .any(|r| r == "spacetime call -s local lyracore claim_operator"),
            "{:?}",
            stack.rendered()
        );
        assert!(http.requests().is_empty(), "{:?}", http.requests());
        assert!(!dev.project.token_file().exists());
    }

    #[test]
    fn the_minted_identity_survives_into_the_next_run() {
        // `claim_operator` refuses a second identity, so the credential MUST be stable across
        // invocations — including a fresh process that reloads everything from `.lyracore/`.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let http = FakeHttp::new();
        {
            let mut dev = DevManager::new(project(&tmp)).unwrap();
            let stack = FakeStack::new().fail_on("login show", "You are not logged in");
            dev.up(
                &stack.runner(),
                &stack.inspector(),
                &http,
                ClientBind::Loopback,
            )
            .unwrap();
            dev.down(&stack.runner(), &stack.inspector(), false)
                .unwrap();
        }
        let mut dev = DevManager::new(ProjectLayout::from_root(&root).unwrap()).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
        )
        .unwrap();

        assert_eq!(
            http.mints().len(),
            1,
            "the second run minted a second identity: {:?}",
            http.requests()
        );
    }

    #[test]
    fn up_refuses_to_start_an_anonymous_gateway_when_no_credential_can_be_had() {
        // Logged out AND unable to mint (a node that answers its port but not the API): there is
        // no credential, and an anonymous gateway would look up for 15s and then die on a
        // subscription timeout. Refuse before spawning anything.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::failing("no such endpoint");

        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &http,
                ClientBind::Loopback,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("SpacetimeDB node"),
            "the refusal must name what could not be reached: {error}"
        );
        assert!(
            !stack
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Spawn { spec, .. }
                if spec.render().contains("gateway"))),
            "an anonymous gateway must not be started at all"
        );
    }

    #[test]
    fn a_database_claimed_by_another_identity_says_which_file_to_delete() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");

        // The node mints happily; it is the CLAIM it refuses.
        struct ClaimRefused;
        impl crate::http::HttpClient for ClaimRefused {
            fn post_json(&self, url: &str, _bearer: Option<&str>, _body: &str) -> Result<String> {
                if url.ends_with("/v1/identity") {
                    return Ok(format!(
                        "{{\"identity\":\"c2\",\"token\":\"{}\"}}",
                        crate::http::fake::MINTED_TOKEN
                    ));
                }
                Err(Error::Http(format!(
                    "{url} answered HTTP 400: operator already claimed"
                )))
            }
        }

        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &ClaimRefused,
                ClientBind::Loopback,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("operator already claimed"),
            "the node's own reason must survive: {error}"
        );
        assert!(
            error.to_string().contains("coordinator-token"),
            "and the way out must be named: {error}"
        );
        // A node failure wrapped in a node failure would print "process error: process error: …",
        // which is the only thing anyone would notice about the message.
        assert_eq!(
            error.to_string().matches("process error:").count(),
            1,
            "the wrapped error double-prefixed itself: {error}"
        );
    }

    #[test]
    fn the_gateway_binds_loopback_only() {
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(&project(&tmp), &ClientBind::Loopback, FAKE_TOKEN);
        for var in ["LYRACORE_LOGON_BIND", "LYRACORE_WORLD_BIND"] {
            let bind = cmd.env_value(var).unwrap();
            assert!(
                bind.starts_with("127.0.0.1:"),
                "{var} must be loopback, got {bind}"
            );
        }
    }

    #[test]
    fn up_never_wipes_a_database_or_reselects_the_server() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Nothing running: `up` must start both, and spawning is what opens their ports.
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        for rendered in stack.rendered() {
            let args: Vec<&str> = rendered.split_whitespace().collect();
            assert!(
                !args.contains(&"-c"),
                "a -c wipe must never be rendered: {rendered}"
            );
            assert!(!args.contains(&"--clear-database"), "no wipe: {rendered}");
            assert!(
                !rendered.contains("server set-default"),
                "the selected server must never be changed: {rendered}"
            );
            assert!(
                !rendered.contains("delete"),
                "no database deletion: {rendered}"
            );
        }
    }

    #[test]
    fn up_publishes_only_the_single_seeded_database_and_always_with_the_two_flags() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        let publishes: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(publishes.len(), 1, "exactly one publish: {publishes:?}");
        assert!(
            publishes[0].ends_with(ProjectLayout::DATABASE),
            "{publishes:?}"
        );
        for shard in [
            "lyracore-world-1",
            "lyracore-world-2",
            "lyracore-instances",
            "realm-core",
        ] {
            assert!(
                !publishes[0].contains(shard),
                "the fixture must not touch {shard}: {}",
                publishes[0]
            );
        }
        // The guarantees that used to belong to `scripts/publish-module.sh` are now properties of
        // the ONE command builder every publish in this CLI goes through.
        assert!(
            publishes[0].contains("--build-options=--features=debug_reducers"),
            "{publishes:?}"
        );
        assert!(publishes[0].contains("--yes"), "{publishes:?}");
    }

    #[test]
    fn up_preflights_before_it_publishes() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        let rendered = stack.rendered();
        let publish = rendered
            .iter()
            .position(|r| r.starts_with("spacetime publish"))
            .expect("published");
        let gate = rendered
            .iter()
            .position(|r| r.contains("cargo check"))
            .expect("preflighted");
        assert!(gate < publish, "{rendered:?}");
    }

    #[test]
    fn up_refuses_to_publish_a_checkout_the_gate_rejects() {
        // The deploy-time break class: green under `cargo test`, fatal on publish.
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        std::fs::write(
            project.module_sources().join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE no_such_column = :sender\");\n",
        )
        .unwrap();
        let mut dev = DevManager::new(project).unwrap();
        let stack = FakeStack::new();

        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
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
            "{:?}",
            stack.rendered()
        );
        assert!(
            !stack
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Spawn { spec, .. }
                if spec.render().contains("gateway"))),
            "and no gateway may be started against an unpublished module"
        );
    }

    #[test]
    fn up_is_idempotent_when_everything_is_already_healthy() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.state
            .set(Component::Spacetime, Some(record(10, "stdb")));
        dev.state.set(Component::Gateway, Some(record(20, "gw")));

        let stack = FakeStack::new()
            .with_process(10, "stdb")
            .with_process(20, "gw")
            .with_port(ProjectLayout::STDB_PORT)
            .with_port(ProjectLayout::WORLD_PORT);

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();
        // The status report's read-only database probe is the ONE thing a second `up` may run.
        // Nothing may be started, built, published or re-claimed.
        for rendered in stack.rendered() {
            assert_eq!(
                rendered, "spacetime describe --json -s local lyracore",
                "a healthy stack must not be restarted, rebuilt, republished or re-claimed"
            );
        }
    }

    #[test]
    fn up_reuses_a_spacetimedb_it_did_not_start_and_never_records_it() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Port 3000 answers, but no PID of ours owns it.
        let stack = FakeStack::new().with_port(ProjectLayout::STDB_PORT);

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        assert!(
            dev.state.record(Component::Spacetime).is_none(),
            "a pre-existing server must never be recorded as ours"
        );
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime start")),
            "must not start a second node"
        );
    }

    #[test]
    fn a_second_up_after_a_partial_start_completes_the_stack() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // SpacetimeDB is ours and healthy; the gateway died.
        dev.state
            .set(Component::Spacetime, Some(record(10, "stdb")));

        let stack = FakeStack::new()
            .with_process(10, "stdb")
            .with_port(ProjectLayout::STDB_PORT);

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
        )
        .unwrap();

        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("spacetime start")),
            "the healthy node must not be restarted"
        );
        assert!(
            stack
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Spawn { spec, .. }
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

        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&ProjectLayout::WORLD_PORT.to_string()),
            "the refusal must name the contended port: {error}"
        );
        assert!(
            !stack
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Spawn { .. })),
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
