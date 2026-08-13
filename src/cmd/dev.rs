//! The local realm lifecycle: `dev up | status | logs | smoke | down`.
//!
//! Scope is the seeded loopback fixture, which since #11 is **sharded by default**: four databases
//! — the default world shard, the map-1 shard, the instance pool (#108) and realm-core
//! (`Topology::Sharded`), one per production tier in core `docs/architecture.md` §3.1. `dev up
//! --single` is the pre-#11 one-database fixture, unchanged down to the hard unset of every
//! topology variable.
//!
//! This is still not the production recipe in `docs/danger-zones.md` §3 — different databases,
//! loopback only — and no path here reads a topology variable out of the contributor's shell.
//! [`Topology::apply_env`](crate::project::Topology::apply_env) decides all four, in both modes.
//!
//! # The failure this file is shaped around
//!
//! A gateway's response to bad topology config is **silent collapse to one database**: a malformed
//! shard-map rule is logged and dropped, an absent or default-equal `LYRACORE_REALM_CORE` reads as
//! unconfigured, an empty `LYRACORE_SHARD_MAP` still counts as set. The result starts, binds,
//! answers its health probe, and serves ONE database while the others sit published, claimed and
//! unused. So `up` does not stop at exporting the right strings — it reads the realised topology
//! back out of the gateway's own log ([`DevManager::verify_topology`]), and `status` reports every
//! database rather than the default one.

use crate::cmd::{
    gateway_log::{connected_shards, CONNECTED_MARKER},
    preflight, publish,
};
use crate::harness::{self, Harness};
use crate::http::HttpClient;
use crate::proc::{start_signature, CommandSpec, ProcessInspector, ProcessRunner};
use crate::project::{ClientBind, Component, ProjectLayout, Topology};
use crate::state::{ProcessRecord, RuntimeState};
use crate::token::Credential;
use crate::{Error, Result};
use std::thread::sleep;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `dev down` waits for a signalled process to exit and release its port. SpacetimeDB
/// answers its listener during graceful shutdown; returning before the port closes hands the race
/// to a scripted `dev down && dev up` (core #542).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`DevManager::verify_topology`] waits for the coordinator lines to appear in the
/// gateway log.
///
/// The gateway awaits `Coordinator::connect` — every shard of it — BEFORE it spawns the logon and
/// world listeners, so by the time the world port answers the lines are already written. This is
/// slack for the log file's own buffering, not for the connections.
const TOPOLOGY_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// How many databases the running stack has. Read from state for the same reason as `bind`:
    /// `dev status` on a `--single` stack must report one database, not four unreachable ones.
    /// Replaced by `up`'s argument.
    topology: Topology,
    /// How long [`Self::verify_topology`] waits for the gateway's coordinator lines. A field
    /// rather than the constant itself so a test of the COLLAPSED case — the one that can only
    /// ever end at the deadline — does not spend ten real seconds proving it.
    verify_timeout: Duration,
    /// Byte length of the gateway log at the moment THIS run spawned the gateway. The log is
    /// opened append (`spawn_logged`), so it accumulates runs; verifying against the whole file
    /// let a PREVIOUS run's connect lines pass a collapsed gateway, and a previous run's
    /// diagnostics false-alarm a healthy one (core #541). Zero = no spawn this run (reuse paths):
    /// verify reads the whole file, the pre-fix behavior, which is right for a log we didn't add to.
    gateway_log_start: u64,
}

impl DevManager {
    pub fn new(project: ProjectLayout) -> Result<Self> {
        let state = RuntimeState::load(&project.state_file())?;
        let bind = state.bind();
        let topology = state.topology();
        Ok(Self {
            project,
            state,
            bind,
            topology,
            verify_timeout: TOPOLOGY_VERIFY_TIMEOUT,
            gateway_log_start: 0,
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
        topology: Topology,
    ) -> Result<()> {
        self.project.ensure_dirs()?;
        self.state.database = ProjectLayout::DATABASE.to_string();

        let spacetime = self.status_for(Component::Spacetime, inspector);
        // Under the RECORDED bind and the RECORDED topology — "is a gateway of ours running,
        // wherever it was put and however many databases it was given?". Neither can be changed
        // under a live process, and both are read from the environment exactly once at startup.
        let running = self.status_for(Component::Gateway, inspector);
        self.check_bind_change(&running, &bind)?;
        self.check_topology_change(&running, topology)?;
        self.bind = bind;
        self.topology = topology;
        self.state.client_host = self.bind.host();
        self.state.topology = self.topology.as_str().to_string();
        // ...and now under the requested bind, which is where anything we start must answer.
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

        // Before the verification, so a realm that came up wrong is still one `dev down` can stop.
        self.state.save(&self.project.state_file())?;
        self.verify_topology()?;
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

    /// A running gateway cannot change its topology either — the shard set is read from the
    /// environment once, at `Coordinator::connect`.
    ///
    /// Without this, `dev up --single` onto a running sharded stack would report "already up" and
    /// hand back a multi-database realm, and `dev up` onto a running `--single` one would report
    /// shards that are not there. Both are the silent collapse wearing a success message.
    fn check_topology_change(&self, gateway: &ComponentStatus, wanted: Topology) -> Result<()> {
        if !matches!(
            gateway,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) || self.topology == wanted
        {
            return Ok(());
        }
        Err(Error::Process(format!(
            "the running gateway was started as a {} realm ({} database(s)) and a running process \
             cannot be re-sharded; `lyracore dev down` first, then start it the way you want ({}).",
            self.topology.as_str(),
            self.topology.databases().len(),
            match wanted {
                Topology::Sharded => "lyracore dev up",
                Topology::Single => "lyracore dev up --single",
            }
        )))
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

    /// The offline deploy gate, then the one correct publish — once per database in the topology.
    ///
    /// Both are this CLI's own (`lyracore preflight` / `lyracore publish`) rather than a shell-out
    /// to the server repo's `scripts/`: the guarantees — `--features=debug_reducers`, `--yes`,
    /// `-s local`, and the unreachability of a `-c` wipe — are properties of
    /// `publish::publish_command`, which is the ONLY way a publish is rendered anywhere in this
    /// CLI.
    ///
    /// A failure PART WAY THROUGH names the databases that did land. A half-published realm is the
    /// state the gateway reports as an unrelated mid-session hang rather than a loud "no such
    /// table", so "which ones are current?" is the question the operator is about to ask, and the
    /// run that knows the answer is this one.
    fn publish(&self, runner: &dyn ProcessRunner) -> Result<()> {
        println!("· preflight (offline: build, schema, visibility filters)...");
        preflight::run(&self.project, runner)?;

        let databases = self.topology.databases();
        let mut published: Vec<&str> = Vec::new();
        for database in &databases {
            println!(
                "· publishing {database} ({}/{})...",
                published.len() + 1,
                databases.len()
            );
            if let Err(e) =
                runner.run_streaming(&publish::publish_command(&self.project, database)?)
            {
                return Err(Error::Process(format!(
                    "{e}\n  publishing {database} failed. Published so far: {}. Still to do: {}.\n  \
                     Every database runs the SAME module, so a partial publish is a schema skew — \
                     fix the failure and re-run `lyracore dev up`, which republishes all of them.",
                    render_list(&published),
                    render_list(&databases[published.len() + 1..])
                )));
            }
            published.push(database);
        }
        Ok(())
    }

    /// Claim the operator AS the identity the gateway is about to use, on EVERY database.
    ///
    /// `claim_operator` is TOFU on `ctx.sender()`: idempotent for the same identity, refused for a
    /// different one. So the caller matters more than the call, and it differs by credential:
    ///
    /// - a `spacetime login` token IS the `spacetime` CLI's identity, so the proven `spacetime
    ///   call` path is kept exactly as it was;
    /// - a server-issued token is an identity the `spacetime` CLI knows nothing about. Shelling
    ///   out would claim the operator for the CLI's identity instead, and the gateway — running as
    ///   the minted one — would then be refused by its own database. It is called with the token.
    ///
    /// Per database, and with the SAME identity every time: a shard claimed by nobody refuses the
    /// gateway's own writes, and one claimed by a different identity refuses them permanently. Both
    /// are delayed deaths — nothing fails until the first write that database has to serve.
    fn claim_operator(
        &self,
        runner: &dyn ProcessRunner,
        http: &dyn HttpClient,
        credential: &Credential,
    ) -> Result<()> {
        for database in self.topology.databases() {
            println!("· claiming the operator identity on {database}...");
            if !credential.is_server_issued() {
                runner.run_and_wait(&claim_command(database))?;
                continue;
            }
            http.post_json(
                &reducer_url(database, "claim_operator"),
                Some(credential.token()),
                "[]",
            )
            .map_err(|e| {
                Error::Process(format!(
                    "{e}\n  `claim_operator` was called on {database} with the server-issued \
                     identity in {}. A refusal there almost always means this database was claimed \
                     by a DIFFERENT identity (an earlier `spacetime login`, or another checkout): \
                     delete that file and re-run `lyracore dev up` to fall back to that login.",
                    self.project.token_file().display()
                ))
            })?;
        }
        Ok(())
    }

    // ---- the realised topology ----

    /// Read the topology the gateway ACTUALLY built back out of its own log.
    ///
    /// This is the whole answer to the silent collapse. Every one of the four topology variables
    /// fails quietly when it is wrong — a malformed rule is dropped, a default-equal realm-core
    /// reads as unconfigured, a database that never published is "unreachable, falling back to the
    /// default" — and the gateway then starts, binds, answers, and passes every PID-and-port check
    /// in this file while serving one database. "We exported the right strings" is not evidence.
    ///
    /// The gateway logs `coordinator connected to shard <db>` once per database it actually
    /// connected, from `Coordinator::connect`, which is awaited BEFORE the listeners are spawned —
    /// so a gateway answering its port has already written all of them.
    ///
    /// A gateway that never logs the phrase at all is reported as UNVERIFIABLE rather than as
    /// collapsed: that is an older (or renamed) build, and failing a working stack because this
    /// CLI could not read its log would be the worse error.
    fn verify_topology(&self) -> Result<()> {
        let wanted = self.topology.databases();
        let log = self.project.log_file(Component::Gateway);
        // Re-read until every wanted database has appeared, or the deadline. A COMPLETE realm
        // therefore costs one read; only a collapsed one waits, which is the right way round.
        let deadline = Instant::now() + self.verify_timeout;
        // Only THIS run's lines: slice off everything written before our spawn (append-mode log,
        // core #541). A shrunken file (rotated/removed underneath us) falls back to the start.
        let this_run = |full: String| -> String {
            let at = usize::try_from(self.gateway_log_start).unwrap_or(0);
            match full.get(at..) {
                Some(tail) => tail.to_string(),
                None => full,
            }
        };
        let mut text = this_run(std::fs::read_to_string(&log).unwrap_or_default());
        while connected_shards(&text).len() < wanted.len() && Instant::now() < deadline {
            sleep(Duration::from_millis(200));
            if let Ok(reread) = std::fs::read_to_string(&log) {
                text = this_run(reread);
            }
        }

        if !text.contains(CONNECTED_MARKER) {
            println!(
                "· topology UNVERIFIED: this gateway build does not log \"{CONNECTED_MARKER}\", so \
                 the realised database set could not be read back from {}.",
                log.display()
            );
            return Ok(());
        }

        let connected = connected_shards(&text);
        let missing: Vec<&str> = wanted
            .iter()
            .copied()
            .filter(|db| !connected.iter().any(|c| c == db))
            .collect();
        // The other direction, which matters most in `--single` mode: a database the fixture never
        // published, never claimed and never asked for. That is a topology variable leaking in from
        // somewhere, and the whole point of the hard unset is that it cannot.
        let unexpected: Vec<&str> = connected
            .iter()
            .map(String::as_str)
            .filter(|db| !wanted.contains(db))
            .collect();
        if missing.is_empty() && unexpected.is_empty() {
            println!(
                "✓ topology verified: the gateway connected to {}.",
                render_list(&wanted)
            );
            return Ok(());
        }
        let mut detail = String::new();
        if !missing.is_empty() {
            detail.push_str(&format!(" {} never connected.", render_list(&missing)));
        }
        if !unexpected.is_empty() {
            detail.push_str(&format!(
                " It also connected to {}, which this fixture never published.",
                render_list(&unexpected)
            ));
        }
        Err(Error::Process(format!(
            "the gateway came up serving {} of {} databases —{detail}\n  This is the SILENT \
             collapse: a gateway with a dropped shard rule, an unconfigured realm-core or an \
             unreachable database starts, binds and answers exactly like a healthy one, while the \
             rest sit published, claimed and unused.\n  It is still running (`lyracore dev down` \
             stops it); the reason is in {}.",
            connected.len(),
            wanted.len(),
            log.display()
        )))
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
            //
            // External = no recorded PID at all (see `classify`), so `dev down --forget` — which
            // only clears a RECORDED PID whose process was reused — is a no-op here and must not
            // be suggested: it sent an operator in circles (core #540). The stray is typically a
            // gateway from another checkout (state.json lives per-checkout); the only remedies are
            // stopping it from ITS checkout or killing it by name.
            return Err(Error::Process(format!(
                "port {} is already served by a gateway this CLI did not start (typically a \
                 `dev up` from another checkout — state is per-checkout). Run `lyracore dev down` \
                 from that checkout, or kill it with `pkill -x lyracore-gatewa` (no trailing 'y': \
                 the kernel truncates process names to 15 chars, and the full 16-char name \
                 silently matches nothing), then re-run `lyracore dev up`",
                ProjectLayout::WORLD_PORT
            )));
        }
        println!(
            "· starting the gateway on {} against {} database(s)...",
            ProjectLayout::world_bind(&self.bind),
            self.topology.databases().len()
        );
        // The log appends across runs; remember where THIS run starts so verify_topology never
        // reads a previous gateway's lines (core #541).
        self.gateway_log_start = std::fs::metadata(self.project.log_file(Component::Gateway))
            .map(|m| m.len())
            .unwrap_or(0);
        let command = gateway_command(&self.project, &self.bind, credential.token(), self.topology);
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

    fn wait_for_port(
        &mut self,
        component: Component,
        inspector: &dyn ProcessInspector,
    ) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let host = component.health_host(&self.bind);
        while Instant::now() < deadline {
            if inspector.serving(&host, component.health_port()) {
                // The process may have `exec`'d since we captured its identity right after
                // spawning it (see the died-during-startup check below) — SpacetimeDB's
                // version-manager shim keeps the PID and start time but replaces `comm`. Refresh
                // the persisted identity to the settled one now, so a later `dev down`/`dev
                // status` — which reads `comm` long after the exec has happened — compares
                // against the process that is actually running rather than a shim that no longer
                // exists (#431).
                if let Some(record) = self.state.record(component) {
                    if let Some(settled) = inspector.identity(record.pid) {
                        if settled != record.identity {
                            let pid = record.pid;
                            self.state.set(
                                component,
                                Some(ProcessRecord {
                                    pid,
                                    identity: settled,
                                }),
                            );
                        }
                    }
                }
                return Ok(());
            }
            // Died during startup — fail now rather than after the full timeout. Compared on
            // only the start-time prefix, not the full identity: the version-manager shim
            // `exec`s into the versioned binary sometime during this poll loop, which changes
            // `comm` while preserving PID and start time. Comparing the full identity here reads
            // that ordinary `exec` as the process having died (#431).
            if let Some(record) = self.state.record(component) {
                let alive = inspector.identity(record.pid).is_some_and(|live| {
                    start_signature(&live) == start_signature(&record.identity)
                });
                if !alive {
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
        if let ClientBind::Lan(ip) = &self.bind {
            println!("  {:<10} LAN — clients connect to realmlist {ip}", "bind");
        }
        // The third thing that can be wrong, independently of every PID and every port: a database
        // the stack is supposed to be serving was never published (or was published to another
        // node), or the gateway never connected to it. Reported PER DATABASE, because in a sharded
        // realm the default one is invariably the one that IS fine — a partial publish and a
        // silently collapsed topology both look like a perfect stack from the outside, and this is
        // the only place either becomes visible.
        let databases = self.topology.databases();
        println!(
            "  {:<10} {}, {} database(s)",
            "topology",
            self.topology.as_str(),
            databases.len()
        );
        let connected = self.connected_shards();
        for database in databases {
            println!(
                "  {:<10} {:<18} {}",
                "",
                database,
                self.database_health(runner, &spacetime, database, connected.as_deref())
            );
        }
    }

    /// Ask the node about ONE database, and the gateway's own log about whether it reached it.
    ///
    /// `describe` reads the published schema: it proves the database exists on the node the gateway
    /// is pointed at, and it reads no rows, so it cannot be confused by row-level visibility. It
    /// cannot, however, prove the gateway is USING it — a published, claimed and entirely unused
    /// shard describes perfectly — which is what the log evidence is for.
    fn database_health(
        &self,
        runner: &dyn ProcessRunner,
        spacetime: &ComponentStatus,
        database: &str,
        connected: Option<&[String]>,
    ) -> String {
        if matches!(
            spacetime,
            ComponentStatus::Stopped | ComponentStatus::Unhealthy(_)
        ) {
            return "not checked — SpacetimeDB is not serving".to_string();
        }
        let Ok(_) = runner.run_and_wait(&describe_command(database)) else {
            return format!(
                "UNREACHABLE on {} — run `lyracore dev up` to publish it",
                ProjectLayout::stdb_uri()
            );
        };
        match connected {
            // No log evidence either way: an older gateway build, or one that has not started.
            None => "published".to_string(),
            Some(shards) if shards.iter().any(|s| s == database) => {
                "published, gateway connected".to_string()
            }
            Some(_) => "published, but the gateway NEVER CONNECTED to it — see `dev logs gateway`"
                .to_string(),
        }
    }

    /// The databases the running gateway actually connected to, per its own log — or `None` when
    /// there is no evidence to read (no log yet, or a build that does not log it).
    fn connected_shards(&self) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(self.project.log_file(Component::Gateway)).ok()?;
        text.contains(CONNECTED_MARKER)
            .then(|| connected_shards(&text))
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
                    // Wait for the process to actually exit AND release its port. `terminate` only
                    // delivers SIGTERM; SpacetimeDB keeps its listener answering while it shuts
                    // down, so a scripted `dev down && dev up` used to probe :3000 mid-shutdown,
                    // classify the dying node External, "reuse" it — and publish against a corpse
                    // (core #542). "Stopped" must mean gone, not signalled.
                    let host = component.health_host(&self.bind);
                    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
                    while (inspector.identity(pid).is_some()
                        || inspector.serving(&host, component.health_port()))
                        && Instant::now() < deadline
                    {
                        sleep(Duration::from_millis(100));
                    }
                    if inspector.identity(pid).is_some() {
                        println!(
                            "  {} (PID {pid}) is taking longer than {}s to exit — it was \
                             signalled and will finish on its own; wait for it before `dev up`.",
                            component.as_str(),
                            SHUTDOWN_TIMEOUT.as_secs()
                        );
                    }
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

/// The fixture gateway.
///
/// `LYRACORE_DATABASE` is the DEFAULT world shard in every topology — it must be, because the
/// gateway builds its own shard list starting from this one and collapses to it for every lookup
/// no rule answers. The rest of the topology is `Topology::apply_env`'s to decide, in both modes:
/// `--single` unsets all four variables (so an exported production recipe cannot leak in), and the
/// sharded default sets the two it means and unsets the other two.
fn gateway_command(
    project: &ProjectLayout,
    bind: &ClientBind,
    token: &str,
    topology: Topology,
) -> CommandSpec {
    let cmd = CommandSpec::new(project.gateway_bin().to_string_lossy().to_string())
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
    topology.apply_env(cmd)
}

/// The TOFU operator claim, for the credential the `spacetime` CLI already carries.
fn claim_command(database: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("call")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg(database)
        .arg("claim_operator")
}

/// An operator-gated reducer over the node's HTTP API, which is the only way to call one as an
/// identity the `spacetime` CLI does not hold — and the way `dev up` calls `claim_operator`,
/// because a claim made by one identity and a write made by another is the lock-out this path
/// exists to avoid. `lyracore character gm` reaches `set_gm_level` the same way.
pub(crate) fn reducer_url(database: &str, reducer: &str) -> String {
    format!(
        "{}/v1/database/{database}/call/{reducer}",
        ProjectLayout::stdb_uri()
    )
}

/// The database-health probe: schema only, no rows, no writes.
fn describe_command(database: &str) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("describe")
        .arg("--json")
        .arg("-s")
        .arg(ProjectLayout::STDB_SERVER)
        .arg(database)
}

/// A list of database names for a human: `a, b and c`, or `none` for an empty one — which is a real
/// case here (a publish that failed on the very first database has published nothing).
fn render_list(names: &[&str]) -> String {
    match names {
        [] => "none".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
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
    fn a_shim_exec_during_startup_is_not_read_as_a_death() {
        // #431: on a cold host, SpacetimeDB's version-manager shim is still running (comm =
        // `spacetime`) at the instant `spawn_recorded` captures its identity, then `exec`s into
        // the versioned binary (comm = `spacetimedb-sta`, kernel-truncated to 15 chars) before
        // the port opens. The old died-during-startup check compared the FULL identity and read
        // that ordinary `exec` as the process having exited.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().with_shim_exec(
            "spacetime start",
            "Thu Aug  7 10:23:45 2026 spacetime",
            "Thu Aug  7 10:23:45 2026 spacetimedb-sta",
        );

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();

        // The persisted identity must also have been refreshed to the settled (post-exec) one —
        // otherwise a LATER `dev status`/`dev down`, which reads `comm` long after the exec has
        // happened, would see today's `comm` and wrongly conclude the PID had been reused.
        assert_eq!(
            dev.status_for(Component::Spacetime, &stack.inspector()),
            ComponentStatus::Healthy
        );
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

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
        .unwrap();

        let gateway = gateway_command(&dev.project, &lan(), FAKE_TOKEN, Topology::Single);
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

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
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
            dev.up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                lan(),
                Topology::Single,
            )
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
            Topology::Single,
        )
        .unwrap();

        // `dev up --lan` onto a running loopback stack: the old "already up — nothing to do" would
        // report success for a LAN realm that is not listening on the LAN at all.
        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                lan(),
                Topology::Single,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("dev down"),
            "the refusal must say how to fix it: {error}"
        );
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);

        // ...and after `dev down` the same request is accepted.
        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
        .unwrap();
    }

    #[test]
    fn up_is_still_idempotent_in_lan_mode() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
        .unwrap();
        let after_first: Vec<String> = stack.rendered();

        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
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
            Topology::Single,
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
            Topology::Single,
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
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            lan(),
            Topology::Single,
        )
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
            Topology::Single,
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
            Topology::Single,
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
            Topology::Single,
        )
        .unwrap();

        // Both processes are alive and both ports answer — the state in which an unpublished (or
        // wrong-node) database is invisible to a PID-and-port check.
        let connected = dev.connected_shards();
        let healthy = dev.database_health(
            &stack.runner(),
            &ComponentStatus::Healthy,
            ProjectLayout::DATABASE,
            connected.as_deref(),
        );
        assert!(healthy.contains("published"), "{healthy}");
        assert!(!healthy.contains("UNREACHABLE"), "{healthy}");
        assert!(
            healthy.contains("gateway connected"),
            "the gateway's own log is the only evidence it is USING the database: {healthy}"
        );

        let broken = FakeStack::new().fail_on("describe", "database not found");
        let unhealthy = dev.database_health(
            &broken.runner(),
            &ComponentStatus::Healthy,
            ProjectLayout::DATABASE,
            connected.as_deref(),
        );
        assert!(unhealthy.contains("UNREACHABLE"), "{unhealthy}");
        assert!(
            unhealthy.contains("lyracore dev up"),
            "must be actionable: {unhealthy}"
        );
    }

    #[test]
    fn the_database_probe_reads_schema_and_never_writes() {
        for database in Topology::Sharded.databases() {
            let cmd = describe_command(database).render();
            assert_eq!(
                cmd,
                format!("spacetime describe --json -s local {database}")
            );
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
    }

    #[test]
    fn a_stopped_database_is_reported_as_unchecked_not_as_broken() {
        let tmp = TempDir::new().unwrap();
        let dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        let line = dev.database_health(
            &stack.runner(),
            &ComponentStatus::Stopped,
            ProjectLayout::DATABASE,
            None,
        );
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
        // `dev up --single`, unchanged from before #11 down to the hard unset. This is the whole
        // point of keeping the flag: a contributor who has the production recipe exported in their
        // shell gets the fixture, not a multi-database gateway pointed at databases this CLI never
        // published.
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(
            &project(&tmp),
            &ClientBind::Loopback,
            FAKE_TOKEN,
            Topology::Single,
        );
        assert_eq!(
            cmd.env_value("LYRACORE_DATABASE"),
            Some(ProjectLayout::DATABASE)
        );
        for var in ProjectLayout::TOPOLOGY_VARS {
            assert_eq!(cmd.env_value(var), None, "{var} must not be set");
            assert!(
                cmd.removes_env(var),
                "{var} must be actively unset so an exported production recipe cannot leak in"
            );
        }
    }

    #[test]
    fn the_gateway_runs_against_exactly_the_databases_the_fixture_published() {
        // The sharded half of the same contract: every database the environment names is one this
        // CLI publishes and claims, and the default world shard is `LYRACORE_DATABASE`.
        let tmp = TempDir::new().unwrap();
        let cmd = gateway_command(
            &project(&tmp),
            &ClientBind::Loopback,
            FAKE_TOKEN,
            Topology::Sharded,
        );
        assert_eq!(
            cmd.env_value("LYRACORE_DATABASE"),
            Some(ProjectLayout::DATABASE)
        );
        assert_eq!(
            cmd.env_value("LYRACORE_SHARD_MAP"),
            Some("1:*=lyracore-kalimdor, 36:*=lyracore-instances")
        );
        assert_eq!(cmd.env_value("LYRACORE_REALM_CORE"), Some("lyracore-realm"));
        // The two this mode does not set are still unset, in this mode too: the env var wins over
        // an inherited `LYRACORE_SHARD_MAP_FILE`, and `LYRACORE_REGION_SHARDS` would name a
        // database from the retired region topology that this fixture never publishes.
        assert!(cmd.removes_env("LYRACORE_SHARD_MAP_FILE"));
        assert!(cmd.removes_env("LYRACORE_REGION_SHARDS"));
        assert_eq!(cmd.env_value("LYRACORE_REGION_SHARDS"), None);
    }

    // ---- the sharded fixture (#11) ----

    fn sharded_up(dev: &mut DevManager, stack: &FakeStack, http: &dyn HttpClient) -> Result<()> {
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            http,
            ClientBind::Loopback,
            Topology::Sharded,
        )
    }

    fn calls_to(http: &FakeHttp, reducer: &str) -> Vec<crate::http::fake::Request> {
        http.requests()
            .into_iter()
            .filter(|r| r.url.contains(&format!("/call/{reducer}")))
            .collect()
    }

    #[test]
    fn up_publishes_and_claims_every_database_in_the_gateways_own_order() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();
        sharded_up(&mut dev, &stack, &http).unwrap();

        let published: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .map(|r| r.rsplit(' ').next().unwrap().to_string())
            .collect();
        assert_eq!(
            published,
            vec![
                "lyracore",
                "lyracore-kalimdor",
                "lyracore-instances",
                "lyracore-realm"
            ],
            "every database, the default one first and realm-core last"
        );

        // Claimed once each, with the SAME identity: a shard claimed by nobody refuses the
        // gateway's writes, and one claimed by a different identity refuses them for good.
        let claims = calls_to(&http, "claim_operator");
        let claimed: Vec<String> = claims
            .iter()
            .map(|r| r.url.split('/').nth(5).unwrap().to_string())
            .collect();
        assert_eq!(claimed, published, "{claims:?}");
        assert!(
            claims
                .iter()
                .all(|r| r.bearer.as_deref() == Some(MINTED_TOKEN)),
            "one identity for all of them: {claims:?}"
        );
    }

    #[test]
    fn the_operator_gated_reducer_never_goes_through_the_spacetime_cli() {
        // `spacetime call` runs as the CLI's identity, which for a minted credential is a DIFFERENT
        // identity from the one that just claimed the operator — so the claim would be refused, on
        // a run that reported success.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();
        sharded_up(&mut dev, &stack, &http).unwrap();

        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("claim_operator")),
            "claim_operator must not be shelled out: {:?}",
            stack.rendered()
        );
        assert!(
            !calls_to(&http, "claim_operator").is_empty(),
            "claim_operator was not called"
        );
        // ...and nothing loggable carries the credential it was made with.
        for rendered in stack.rendered() {
            assert!(!rendered.contains(MINTED_TOKEN), "leaked into: {rendered}");
        }
    }

    #[test]
    fn a_publish_that_fails_part_way_names_the_databases_that_did_land() {
        // A half-published realm presents as an unrelated mid-session hang rather than a loud "no
        // such table", so "which ones are current?" is the next question — and this run is the only
        // thing that knows.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        // Fail only the SECOND publish: one lands, one breaks, the rest are never attempted.
        let stack = FakeStack::new().fail_on(
            &format!("--yes {}", ProjectLayout::KALIMDOR_SHARD),
            "migration rejected: Removed table game_creature",
        );

        let error = sharded_up(&mut dev, &stack, &FakeHttp::new()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("migration rejected"), "{message}");
        assert!(message.contains("Published so far: lyracore."), "{message}");
        assert!(
            message.contains("Still to do: lyracore-instances and lyracore-realm"),
            "every database that is now stale, not just the next one: {message}"
        );
        for untried in [ProjectLayout::INSTANCE_POOL, ProjectLayout::REALM_CORE] {
            assert!(
                !stack
                    .rendered()
                    .iter()
                    .any(|r| r.contains(&format!("--yes {untried}"))),
                "the run must stop at the failure: {:?}",
                stack.rendered()
            );
        }
    }

    #[test]
    fn a_first_database_that_fails_reports_that_nothing_was_published() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on(
            &format!("--yes {}", ProjectLayout::DATABASE),
            "no such server",
        );
        let error = sharded_up(&mut dev, &stack, &FakeHttp::new()).unwrap_err();
        assert!(
            error.to_string().contains("Published so far: none"),
            "{error}"
        );
    }

    // ---- the silent collapse ----

    #[test]
    fn a_realm_that_came_up_short_of_its_databases_is_a_failure_not_a_tick() {
        // THE failure this feature is shaped around. A dropped shard rule, an unconfigured
        // realm-core or an unreachable database all produce a gateway that starts, binds, answers
        // its health probe and serves ONE database — while the rest sit published, claimed and
        // unused. Every PID and every port is perfect in that state.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.topology = Topology::Sharded;
        dev.verify_timeout = Duration::from_millis(50);
        std::fs::create_dir_all(&dev.project.logs_dir).unwrap();
        std::fs::write(
            dev.project.log_file(Component::Gateway),
            "gateway starting: logon=127.0.0.1:3724 world=127.0.0.1:8085\n\
             coordinator connected to shard lyracore\n",
        )
        .unwrap();

        let error = dev.verify_topology().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("1 of 4 databases"), "{message}");
        assert!(
            message.contains("lyracore-kalimdor, lyracore-instances and lyracore-realm"),
            "the refusal must name exactly which ones are missing: {message}"
        );
        assert!(
            message.contains("dev down"),
            "the gateway is still running, so say how to stop it: {message}"
        );
    }

    #[test]
    fn verify_reads_only_real_connect_lines_and_only_this_runs_log() {
        // core #541, both halves. (a) The gateway's own diagnostics QUOTE the marker phrase — the
        // motion-relay warn cites "`coordinator connected to shard`" as advice — and 43 minutes of
        // those warns once parsed as 258 connections to a shard named "`". (b) The log appends
        // across runs, so a previous gateway's connect lines must not vouch for this one.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.topology = Topology::Sharded;
        dev.verify_timeout = Duration::from_millis(50);
        std::fs::create_dir_all(&dev.project.logs_dir).unwrap();

        // (a) A healthy realm whose log also carries the quoting diagnostics, twice, plus a
        // duplicated connect line from a coordinator reconnect: still exactly the wanted set.
        let warn = "WARN MOTION RELAY LOOKS DEAD: check the log for a `shared AOI dispatch` \
                    panic line, and that `coordinator connected to shard` was printed for every \
                    database. Restart the gateway to recover play.\n";
        let healthy: String = Topology::Sharded
            .databases()
            .iter()
            .map(|db| format!("coordinator connected to shard {db}\n"))
            .chain([warn.to_string(), warn.to_string()])
            .chain(["coordinator connected to shard lyracore\n".to_string()])
            .collect();
        std::fs::write(dev.project.log_file(Component::Gateway), &healthy).unwrap();
        assert!(
            dev.verify_topology().is_ok(),
            "quoted marker text and duplicate connects must not read as extra databases"
        );

        // (b) A previous run connected every database; this run's gateway only reached the
        // default shard. The stale lines sit before gateway_log_start and must not vouch for it.
        let stale_len = std::fs::metadata(dev.project.log_file(Component::Gateway))
            .unwrap()
            .len();
        let mut appended = healthy.clone();
        appended.push_str("coordinator connected to shard lyracore\n");
        std::fs::write(dev.project.log_file(Component::Gateway), &appended).unwrap();
        dev.gateway_log_start = stale_len;
        let error = dev.verify_topology().unwrap_err();
        assert!(
            error.to_string().contains("1 of 4 databases"),
            "a previous run's connect lines must not pass a collapsed gateway: {error}"
        );
    }

    #[test]
    fn a_complete_realm_verifies_and_a_single_database_one_is_held_to_its_own_count() {
        for topology in [Topology::Single, Topology::Sharded] {
            let tmp = TempDir::new().unwrap();
            let mut dev = DevManager::new(project(&tmp)).unwrap();
            dev.topology = topology;
            dev.verify_timeout = Duration::from_millis(50);
            std::fs::create_dir_all(&dev.project.logs_dir).unwrap();
            let log: String = topology
                .databases()
                .iter()
                .map(|db| format!("coordinator connected to shard {db}\n"))
                .collect();
            std::fs::write(dev.project.log_file(Component::Gateway), &log).unwrap();
            assert!(dev.verify_topology().is_ok(), "{}", topology.as_str());

            // The set is compared in BOTH directions. Only the default database in the log is a
            // collapse for the sharded realm; four of them is a leaked topology variable for the
            // `--single` one, which is exactly what its hard unset exists to prevent.
            std::fs::write(
                dev.project.log_file(Component::Gateway),
                "coordinator connected to shard lyracore\n",
            )
            .unwrap();
            assert_eq!(
                dev.verify_topology().is_ok(),
                topology == Topology::Single,
                "{}",
                topology.as_str()
            );

            std::fs::write(
                dev.project.log_file(Component::Gateway),
                Topology::Sharded
                    .databases()
                    .iter()
                    .map(|db| format!("coordinator connected to shard {db}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            assert_eq!(
                dev.verify_topology().is_ok(),
                topology == Topology::Sharded,
                "{}",
                topology.as_str()
            );
        }
    }

    #[test]
    fn a_single_database_fixture_that_reached_four_names_the_ones_it_should_not_have() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.topology = Topology::Single;
        dev.verify_timeout = Duration::from_millis(50);
        std::fs::create_dir_all(&dev.project.logs_dir).unwrap();
        std::fs::write(
            dev.project.log_file(Component::Gateway),
            "coordinator connected to shard lyracore\n\
             coordinator connected to shard lyracore-realm\n",
        )
        .unwrap();
        let error = dev.verify_topology().unwrap_err().to_string();
        assert!(error.contains("lyracore-realm"), "{error}");
        assert!(error.contains("never published"), "{error}");
    }

    #[test]
    fn a_gateway_that_does_not_log_its_shards_is_unverifiable_rather_than_broken() {
        // An older or renamed gateway build. Failing a working stack because this CLI could not
        // read its log would be the worse error, so it says so and carries on.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.topology = Topology::Sharded;
        dev.verify_timeout = Duration::from_millis(50);
        std::fs::create_dir_all(&dev.project.logs_dir).unwrap();
        std::fs::write(
            dev.project.log_file(Component::Gateway),
            "gateway starting: logon=127.0.0.1:3724 world=127.0.0.1:8085\n",
        )
        .unwrap();
        assert!(dev.verify_topology().is_ok());
        assert_eq!(dev.connected_shards(), None);
    }

    #[test]
    fn up_reads_the_realised_topology_back_out_of_the_gateway_it_started() {
        // End to end: the fake gateway derives its log from the ENVIRONMENT it was handed, exactly
        // as `ShardMap::from_env` does, so this is a real reading of a real (modelled) collapse
        // rather than an assertion that the right strings were exported.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        sharded_up(&mut dev, &stack, &FakeHttp::new()).unwrap();
        assert_eq!(
            dev.connected_shards().unwrap(),
            Topology::Sharded.databases(),
            "the gateway connected to every database the fixture published"
        );
    }

    #[test]
    fn the_modelled_gateway_drops_the_same_configuration_the_real_one_does() {
        // Without this the silent-collapse tests would be circular: a fake that always logged every
        // shard would pass them against a world where the collapse cannot happen. So the fake
        // derives its log from the ENVIRONMENT, with the real gateway's own quiet rules — and here
        // they are, firing at once.
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        let cmd = CommandSpec::new("gateway")
            .env("LYRACORE_DATABASE", "lyracore")
            // A second rule with a non-numeric map: logged and DROPPED, never routed.
            .env(
                "LYRACORE_SHARD_MAP",
                "1:*=lyracore-kalimdor, kalimdor:*=lyracore-kalimdor-2",
            )
            // Equal to the default database: reads as UNCONFIGURED, not as a second connection.
            .env("LYRACORE_REALM_CORE", "lyracore")
            .env("LYRACORE_WORLD_BIND", "127.0.0.1:8085");
        let log = tmp.path().join("gateway.log");
        stack.runner().spawn_logged(&cmd, &log).unwrap();

        assert_eq!(
            connected_shards(&std::fs::read_to_string(&log).unwrap()),
            vec!["lyracore", "lyracore-kalimdor"],
            "two of the four databases this config names actually connect — and every one of \
             those drops is silent"
        );
    }

    #[test]
    fn a_collapsed_gateway_is_still_recorded_so_dev_down_can_stop_it() {
        // The realm came up wrong, but it came UP. State is saved before the verdict, or the one
        // process that needs stopping is the one `dev down` no longer knows about.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.verify_timeout = Duration::from_millis(50);
        let stack = FakeStack::new().with_gateway_log("coordinator connected to shard lyracore\n");

        let error = sharded_up(&mut dev, &stack, &FakeHttp::new()).unwrap_err();
        assert!(error.to_string().contains("1 of 4 databases"), "{error}");
        assert!(dev.state.record(Component::Gateway).is_some());
        let saved = RuntimeState::load(&dev.project.state_file()).unwrap();
        assert!(saved.record(Component::Gateway).is_some(), "{saved:?}");
        assert_eq!(saved.topology(), Topology::Sharded);

        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        assert_eq!(stack.terminated().len(), 2);
    }

    #[test]
    fn status_reports_every_database_not_just_the_default_one() {
        // The highest-value half of #11: a partial publish and a collapsed topology are invisible
        // to a PID-and-port check, and this is the only place either becomes visible.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        sharded_up(&mut dev, &stack, &FakeHttp::new()).unwrap();

        let probing = FakeStack::new();
        dev.status(&probing.runner(), &stack.inspector()).unwrap();
        let probed: Vec<String> = probing
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime describe"))
            .map(|r| r.rsplit(' ').next().unwrap().to_string())
            .collect();
        assert_eq!(
            probed,
            Topology::Sharded.databases(),
            "every database gets its own published/unreachable verdict"
        );

        // A database that is not published gets its own verdict, and the others keep theirs.
        let partial = FakeStack::new().fail_on(
            &format!("-s local {}", ProjectLayout::KALIMDOR_SHARD),
            "database not found",
        );
        let connected = dev.connected_shards();
        let line = dev.database_health(
            &partial.runner(),
            &ComponentStatus::Healthy,
            ProjectLayout::KALIMDOR_SHARD,
            connected.as_deref(),
        );
        assert!(line.contains("UNREACHABLE"), "{line}");
        let ok = dev.database_health(
            &partial.runner(),
            &ComponentStatus::Healthy,
            ProjectLayout::DATABASE,
            connected.as_deref(),
        );
        assert!(ok.contains("gateway connected"), "{ok}");
    }

    #[test]
    fn a_published_database_the_gateway_never_reached_is_reported_as_such() {
        // "Published" is not "in use". A database that describes perfectly and that the gateway
        // never connected to is the silent collapse's resting state.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        dev.topology = Topology::Sharded;
        let stack = FakeStack::new();
        let line = dev.database_health(
            &stack.runner(),
            &ComponentStatus::Healthy,
            ProjectLayout::KALIMDOR_SHARD,
            Some(&["lyracore".to_string()]),
        );
        assert!(line.contains("NEVER CONNECTED"), "{line}");
        assert!(line.contains("dev logs gateway"), "actionable: {line}");
    }

    // ---- --single stays exactly what it was ----

    #[test]
    fn the_single_fixture_publishes_one_database_and_claims_exactly_it() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();

        let publishes: Vec<String> = stack
            .rendered()
            .into_iter()
            .filter(|r| r.starts_with("spacetime publish"))
            .collect();
        assert_eq!(publishes.len(), 1, "{publishes:?}");
        assert!(publishes[0].ends_with(ProjectLayout::DATABASE));
        // One claim, and it is the default database's — a claim on a database this fixture never
        // published is a write against a realm nobody is running.
        let claims = calls_to(&http, "claim_operator");
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert!(
            claims[0]
                .url
                .contains(&format!("/database/{}/call/", ProjectLayout::DATABASE)),
            "{claims:?}"
        );
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
            Topology::Single,
        )
        .unwrap();

        let cmd = gateway_command(
            &dev.project,
            &ClientBind::Loopback,
            FAKE_TOKEN,
            Topology::Single,
        );
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
            Topology::Single,
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
            Topology::Single,
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
            Topology::Single,
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
                Topology::Single,
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
            Topology::Single,
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
                Topology::Single,
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
                Topology::Single,
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
        let cmd = gateway_command(
            &project(&tmp),
            &ClientBind::Loopback,
            FAKE_TOKEN,
            Topology::Single,
        );
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
            Topology::Single,
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
    fn up_publishes_exactly_its_own_databases_and_always_with_the_two_flags() {
        for topology in [Topology::Single, Topology::Sharded] {
            let tmp = TempDir::new().unwrap();
            let mut dev = DevManager::new(project(&tmp)).unwrap();
            let stack = FakeStack::new();
            dev.up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                topology,
            )
            .unwrap();

            let publishes: Vec<String> = stack
                .rendered()
                .into_iter()
                .filter(|r| r.starts_with("spacetime publish"))
                .collect();
            assert_eq!(publishes.len(), topology.databases().len(), "{publishes:?}");
            for (rendered, database) in publishes.iter().zip(topology.databases()) {
                assert!(rendered.ends_with(database), "{rendered}");
                // The guarantees that used to belong to `scripts/publish-module.sh` are now
                // properties of the ONE command builder every publish in this CLI goes through —
                // and they hold for every shard, not just the first.
                assert!(
                    rendered.contains("--build-options=--features=debug_reducers"),
                    "{rendered}"
                );
                assert!(rendered.contains("--yes"), "{rendered}");
            }
            // Databases the fixture does NOT have, in either mode. Not "the production realm's
            // names": the fixture shares `lyracore`, `lyracore-realm` and — since #108 —
            // `lyracore-instances` with production, and what keeps the two apart is the NODE a
            // publish goes to (`-s local`, loopback:3000), never the name. These four have no
            // fixture counterpart at all: production's other world shards, a second instance-pool
            // member, and the realm-core name this fixture deliberately does not use.
            for absent in [
                "lyracore-world-1",
                "lyracore-world-2",
                "lyracore-instances-2",
                "realm-core",
            ] {
                assert!(
                    !publishes.iter().any(|r| r.ends_with(absent)),
                    "the fixture must not touch {absent}: {publishes:?}"
                );
            }
        }
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
            Topology::Single,
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
                Topology::Single,
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
        for topology in [Topology::Single, Topology::Sharded] {
            let tmp = TempDir::new().unwrap();
            let mut dev = DevManager::new(project(&tmp)).unwrap();
            dev.state
                .set(Component::Spacetime, Some(record(10, "stdb")));
            dev.state.set(Component::Gateway, Some(record(20, "gw")));
            // The stack that is already running is this mode's — `up` must not treat a re-run as a
            // mode change (see `check_topology_change`).
            dev.topology = topology;

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
                topology,
            )
            .unwrap();

            // The status report's read-only database probes are the ONE thing a second `up` may
            // run — one per database, and nothing started, built, published or re-claimed.
            let expected: Vec<String> = topology
                .databases()
                .iter()
                .map(|db| format!("spacetime describe --json -s local {db}"))
                .collect();
            assert_eq!(
                stack.rendered(),
                expected,
                "a healthy {} stack must not be restarted, rebuilt, republished or re-claimed",
                topology.as_str()
            );
        }
    }

    #[test]
    fn a_running_realm_is_never_silently_re_sharded() {
        // The sibling of the `--lan` refusal, and the same failure: a gateway reads its shard set
        // from the environment ONCE, at `Coordinator::connect`. Reporting "already up — nothing to
        // do" for a `--single` request that a four-database gateway is serving would hand back the
        // opposite of what was asked for, with a success message.
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
            Topology::Sharded,
        )
        .unwrap();

        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                Topology::Single,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("dev down"),
            "the refusal must say how to fix it: {error}"
        );
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);

        // ...and after `dev down` the same request is accepted.
        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();
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
            Topology::Single,
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
            Topology::Single,
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
                Topology::Single,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&ProjectLayout::WORLD_PORT.to_string()),
            "the refusal must name the contended port: {error}"
        );
        // core #540: External means NO recorded PID (see `classify`), so `dev down --forget` —
        // which only clears a recorded-but-reused PID — is a no-op here and sent an operator in
        // circles. The remedy shown must be one that works: the truncated-comm pkill (the binary
        // name is 16 chars; the kernel's comm is 15, so the full name matches nothing).
        assert!(
            error.to_string().contains("pkill -x lyracore-gatewa"),
            "the refusal must show the working kill command: {error}"
        );
        assert!(
            !error.to_string().contains("--forget"),
            "must not suggest --forget for a gateway with no recorded PID: {error}"
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
