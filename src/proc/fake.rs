//! Test doubles: a tiny in-memory stack shared by a runner and an inspector.
//!
//! These record intent and never touch the machine — the predecessor's "FakeProcessRunner" called
//! `spawn()` for real, so a unit test could launch a gateway.
//!
//! The runner and inspector share one state, so spawning a component actually makes its PID
//! visible and its port answer. That is what lets `up` tests exercise the real sequence instead of
//! a world where every port is magically already open.

use super::{CommandSpec, ProcessInspector, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Wait(CommandSpec),
    Stream(CommandSpec),
    SecretStdin { spec: CommandSpec, secret: Vec<u8> },
    Spawn { spec: CommandSpec, log: PathBuf },
    Terminate(u32),
}

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    processes: HashMap<u32, String>,
    /// Answering endpoints, as `host:port` — a fake gateway bound to a LAN address answers there
    /// and nowhere else, exactly like the real one.
    endpoints: HashSet<String>,
    /// Which spawned PID opened which endpoint, so terminating it closes the listener like a
    /// real one. (A pre-existing endpoint has no PID here and survives, also like a real one.)
    listeners: HashMap<u32, String>,
    next_pid: u32,
    failures: HashMap<String, String>,
    /// Canned stdout, keyed by a substring of the rendered command. Overrides the defaults below.
    stdouts: HashMap<String, String>,
    /// A spawned gateway's whole log file, in place of the one derived from its environment. For
    /// the case the derivation cannot express: a build that never logs its shard connections.
    log_override: Option<String>,
    /// A spawn whose render matches `needle` (0) models a version-manager shim's `exec` (#431):
    /// `identity()` reports `before` (1) on its first read for that PID and `after` (2) on every
    /// read after. See [`FakeStack::with_shim_exec`].
    shim: Option<(String, String, String)>,
    /// How many times `identity()` has been read for each PID currently modelling a shim.
    shim_reads: HashMap<u32, u32>,
    /// The endpoint a shimmed PID would have opened at spawn time, held back until its identity
    /// settles (its second read) — modelling how the shim's `exec` and the port opening land
    /// close together in practice, not before the parent's first identity poll.
    shim_pending_endpoint: HashMap<u32, String>,
}

/// A fake machine: which processes exist, which ports answer, and what was run.
#[derive(Clone, Default)]
pub struct FakeStack(Arc<Mutex<Inner>>);

impl FakeStack {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            next_pid: 4000,
            ..Default::default()
        })))
    }

    /// A process that already exists — e.g. one recorded by an earlier `dev up`.
    pub fn with_process(self, pid: u32, identity: &str) -> Self {
        self.0
            .lock()
            .unwrap()
            .processes
            .insert(pid, identity.to_string());
        self
    }

    /// Model the exact race #431 is shaped around: a spawn whose render contains `needle` is a
    /// version-manager shim, not the final process. `identity()` reports `before` (the shim) on
    /// the first read for that PID and `after` (the settled, post-`exec` process) on every read
    /// after that — and the endpoint it would open does not appear until that second read.
    ///
    /// Without this, `spawn_logged` inserts both the identity and the endpoint synchronously, so
    /// `wait_for_port`'s liveness poll never runs at all and the identity-changed branch it
    /// exists to guard is untested — which is why the bug shipped.
    pub fn with_shim_exec(self, needle: &str, before: &str, after: &str) -> Self {
        self.0.lock().unwrap().shim =
            Some((needle.to_string(), before.to_string(), after.to_string()));
        self
    }

    /// A loopback port already being served, by something this CLI did not start.
    pub fn with_port(self, port: u16) -> Self {
        self.with_endpoint("127.0.0.1", port)
    }

    /// The same, on a named host — for the `--lan` binds.
    pub fn with_endpoint(self, host: &str, port: u16) -> Self {
        self.0
            .lock()
            .unwrap()
            .endpoints
            .insert(format!("{host}:{port}"));
        self
    }

    /// Answer any command whose rendered form contains `needle` with this stdout.
    pub fn with_stdout(self, needle: &str, stdout: &str) -> Self {
        self.0
            .lock()
            .unwrap()
            .stdouts
            .insert(needle.to_string(), stdout.to_string());
        self
    }

    /// Write this, verbatim, as the log of any gateway spawned on this stack — instead of the one
    /// derived from the topology variables it was handed.
    pub fn with_gateway_log(self, text: &str) -> Self {
        self.0.lock().unwrap().log_override = Some(text.to_string());
        self
    }

    /// Make any command whose rendered form contains `needle` fail.
    pub fn fail_on(self, needle: &str, message: &str) -> Self {
        self.0
            .lock()
            .unwrap()
            .failures
            .insert(needle.to_string(), message.to_string());
        self
    }

    pub fn runner(&self) -> FakeProcessRunner {
        FakeProcessRunner(self.0.clone())
    }

    pub fn inspector(&self) -> FakeProcessInspector {
        FakeProcessInspector(self.0.clone())
    }

    pub fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }

    /// Everything rendered — what a log line or an error message could have contained.
    pub fn rendered(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|call| match call {
                Call::Wait(spec)
                | Call::Stream(spec)
                | Call::SecretStdin { spec, .. }
                | Call::Spawn { spec, .. } => spec.render(),
                Call::Terminate(pid) => format!("kill {pid}"),
            })
            .collect()
    }

    pub fn terminated(&self) -> Vec<u32> {
        self.calls()
            .iter()
            .filter_map(|call| match call {
                Call::Terminate(pid) => Some(*pid),
                _ => None,
            })
            .collect()
    }
}

pub struct FakeProcessRunner(Arc<Mutex<Inner>>);

impl FakeProcessRunner {
    fn record(&self, call: Call, render: &str) -> Result<()> {
        let mut inner = self.0.lock().unwrap();
        inner.calls.push(call);
        for (needle, message) in &inner.failures {
            if render.contains(needle.as_str()) {
                return Err(Error::SubprocessFailed {
                    command: render.to_string(),
                    code: 1,
                    message: message.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Which endpoint a spawned command starts serving, so a fake spawn behaves like a real one.
///
/// The gateway's is read out of the bind it was actually given, not assumed to be loopback —
/// otherwise every `--lan` test would pass against a fake that cannot represent the bug.
fn endpoint_for(cmd: &CommandSpec) -> Option<String> {
    let render = cmd.render();
    if render.contains("spacetime start") {
        return Some(format!("127.0.0.1:{}", ProjectLayout::STDB_PORT));
    }
    if render.contains("gateway") {
        return cmd.env_value("LYRACORE_WORLD_BIND").map(str::to_string);
    }
    None
}

/// What a spawned gateway writes to its log — modelled from the ENVIRONMENT it was handed, the
/// same way the real one derives it (`ShardMap::from_env` → `Coordinator::connect`).
///
/// This is the point of the fake, not decoration. The failure #11 is shaped around is that a wrong
/// topology variable produces a gateway that starts, binds, answers, and serves ONE database — so a
/// fake that always logged four shards would make every silent-collapse test pass against a world
/// where the collapse cannot happen. Reproduced here, rule for rule:
///
/// - a shard-map rule that is not `<map|*>:<bucket|*>=<db>` is DROPPED (`ShardMap::parse` logs and
///   skips it), and so is `<map>=<db>` shorthand's malformed cousin;
/// - `LYRACORE_REALM_CORE` that is blank, absent, or EQUAL to the default database reads as
///   unconfigured (`with_realm_core`);
/// - `LYRACORE_REGION_SHARDS` drops the default database, realm-core, and anything a rule already
///   names (`with_region_shards`);
/// - an empty `LYRACORE_SHARD_MAP` is still SET, and therefore suppresses `LYRACORE_SHARD_MAP_FILE`.
///
/// Every one of those is silent in the real gateway's behaviour and visible only as a database
/// missing from this list.
fn gateway_log(cmd: &CommandSpec) -> String {
    let default_db = cmd.env_value("LYRACORE_DATABASE").unwrap_or("lyracore");
    let mut dbs = vec![default_db.to_string()];

    // `LYRACORE_SHARD_MAP` wins over `_FILE` even when empty; the fake never reads a file.
    for raw in cmd
        .env_value("LYRACORE_SHARD_MAP")
        .unwrap_or("")
        .split(['\n', ',', ';'])
    {
        let rule = raw.split('#').next().unwrap_or("").trim();
        if rule.is_empty() {
            continue;
        }
        // `<map|*>:<bucket|*>=<db>`, or the `<map|*>=<db>` shorthand.
        let Some((selector, db)) = rule.split_once('=') else {
            continue; // malformed: logged and dropped
        };
        let (map, bucket) = selector.split_once(':').unwrap_or((selector, "*"));
        let parses = |field: &str| field == "*" || field.parse::<u64>().is_ok();
        if db.trim().is_empty() || !parses(map.trim()) || !parses(bucket.trim()) {
            continue; // malformed: logged and dropped
        }
        let db = db.trim().to_string();
        if !dbs.contains(&db) {
            dbs.push(db);
        }
    }

    // Blank, absent or default-equal all mean "unconfigured".
    let realm_core = cmd
        .env_value("LYRACORE_REALM_CORE")
        .map(str::trim)
        .filter(|n| !n.is_empty() && *n != default_db);

    for raw in cmd
        .env_value("LYRACORE_REGION_SHARDS")
        .unwrap_or("")
        .split(['\n', ',', ';'])
    {
        let name = raw.split('#').next().unwrap_or("").trim();
        // The default is already connected; realm-core owns no characters and is refused outright.
        if name.is_empty() || name == default_db || Some(name) == realm_core {
            continue;
        }
        if !dbs.iter().any(|d| d == name) {
            dbs.push(name.to_string());
        }
    }
    dbs.extend(realm_core.map(str::to_string));

    let mut log = format!(
        "gateway starting: logon={} world={} db={default_db}\n",
        cmd.env_value("LYRACORE_LOGON_BIND").unwrap_or("?"),
        cmd.env_value("LYRACORE_WORLD_BIND").unwrap_or("?"),
    );
    for db in dbs {
        log.push_str(&format!("coordinator connected to shard {db}\n"));
    }
    log
}

/// The auth token a `FakeStack`'s `spacetime` CLI hands out. A distinctive literal, so a test can
/// assert it is absent from everything rendered, logged or serialized.
pub const FAKE_TOKEN: &str = "eyJmYWtl.TOKEN-must-never-be-rendered.SIG";

/// The versions a `FakeStack`'s toolchain reports. A fixture checkout that pins these is a machine
/// whose tools match — the ordinary case; a fixture pinning anything else models the drift that
/// `lyracore preflight`'s check 0 exists to catch.
pub const FAKE_RUST_VERSION: &str = "1.93.0";
pub const FAKE_SPACETIME_VERSION: &str = "2.7.1";

/// Canned stdout for the commands this CLI actually reads output from.
///
/// A `FakeStack` models a machine whose `spacetime` CLI is logged in and whose toolchain matches
/// the checkout, because that is the ordinary case; `fail_on(…)` and `with_stdout(…)` model the
/// others.
fn canned_stdout(render: &str) -> String {
    if render.contains("login show") {
        format!("You are logged in as fake-identity\nYour auth token (don't share this!) is {FAKE_TOKEN}\n")
    } else if render.starts_with("rustc --version") {
        format!("rustc {FAKE_RUST_VERSION} (0000000 2026-01-01)\n")
    } else if render.starts_with("spacetime --version") {
        format!(
            "spacetimedb tool version {FAKE_SPACETIME_VERSION}; spacetime-lib version \
             {FAKE_SPACETIME_VERSION}\n"
        )
    } else {
        String::new()
    }
}

/// `spacetime generate` writes a module's bindings into its `--out-dir`. A fake that only recorded
/// the call would leave the RLS-identifier check (which READS those bindings) unexercisable.
fn materialize_generated_bindings(cmd: &CommandSpec) {
    let args = cmd.args();
    let Some(index) = args.iter().position(|a| a == "--out-dir") else {
        return;
    };
    let Some(out) = args.get(index + 1) else {
        return;
    };
    let out = Path::new(out);
    if std::fs::create_dir_all(out).is_err() {
        return;
    }
    let _ = std::fs::write(
        out.join("game_character_table.rs"),
        "use super::character_type::Character;\n\
         /// Table handle for the table `game_character`.\n",
    );
    let _ = std::fs::write(
        out.join("character_type.rs"),
        "pub struct Character {\n    pub guid: u64,\n    pub owner_identity: Identity,\n}\n",
    );
}

impl ProcessRunner for FakeProcessRunner {
    fn run_and_wait(&self, cmd: &CommandSpec) -> Result<String> {
        let render = cmd.render();
        self.record(Call::Wait(cmd.clone()), &render)?;
        if render.contains("spacetime generate") {
            materialize_generated_bindings(cmd);
        }
        let canned = {
            let inner = self.0.lock().unwrap();
            inner
                .stdouts
                .iter()
                .find(|(needle, _)| render.contains(needle.as_str()))
                .map(|(_, stdout)| stdout.clone())
        };
        Ok(canned.unwrap_or_else(|| canned_stdout(&render)))
    }

    fn run_capturing_stderr(&self, cmd: &CommandSpec) -> Result<String> {
        // A fake has one output stream; which of a real tool's two a banner lands on is exactly
        // what the real runner's separate implementation exists to paper over.
        self.run_and_wait(cmd)
    }

    fn run_streaming(&self, cmd: &CommandSpec) -> Result<()> {
        self.record(Call::Stream(cmd.clone()), &cmd.render())
    }

    fn run_with_secret_stdin(&self, cmd: &CommandSpec, secret: &[u8]) -> Result<String> {
        self.record(
            Call::SecretStdin {
                spec: cmd.clone(),
                secret: secret.to_vec(),
            },
            &cmd.render(),
        )?;
        Ok(String::new())
    }

    fn spawn_logged(&self, cmd: &CommandSpec, log: &Path) -> Result<u32> {
        let render = cmd.render();
        self.record(
            Call::Spawn {
                spec: cmd.clone(),
                log: log.to_path_buf(),
            },
            &render,
        )?;

        // A spawned gateway writes a log, because reading that log back is how `dev up` checks the
        // realised topology. `log_override` models the one case the derivation cannot: a gateway
        // build that does not log its shard connections at all.
        if render.contains("gateway") {
            let text = self
                .0
                .lock()
                .unwrap()
                .log_override
                .clone()
                .unwrap_or_else(|| gateway_log(cmd));
            if let Some(parent) = log.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(log, text);
        }

        let endpoint = endpoint_for(cmd);
        let mut inner = self.0.lock().unwrap();
        inner.next_pid += 1;
        let pid = inner.next_pid;

        let is_shim = inner
            .shim
            .as_ref()
            .is_some_and(|(needle, ..)| render.contains(needle.as_str()));
        if is_shim {
            // Identity and endpoint are held back — `FakeProcessInspector::identity` supplies
            // them once the shim has "settled", per `with_shim_exec`.
            inner.shim_reads.insert(pid, 0);
            if let Some(endpoint) = endpoint {
                inner.shim_pending_endpoint.insert(pid, endpoint);
            }
        } else {
            inner.processes.insert(pid, format!("fake-start {render}"));
            if let Some(endpoint) = endpoint {
                inner.endpoints.insert(endpoint.clone());
                inner.listeners.insert(pid, endpoint);
            }
        }
        Ok(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        self.record(Call::Terminate(pid), "kill")?;
        let mut inner = self.0.lock().unwrap();
        inner.processes.remove(&pid);
        inner.shim_reads.remove(&pid);
        inner.shim_pending_endpoint.remove(&pid);
        if let Some(endpoint) = inner.listeners.remove(&pid) {
            inner.endpoints.remove(&endpoint);
        }
        Ok(())
    }
}

pub struct FakeProcessInspector(Arc<Mutex<Inner>>);

impl ProcessInspector for FakeProcessInspector {
    fn identity(&self, pid: u32) -> Option<String> {
        let mut inner = self.0.lock().unwrap();
        if let Some(reads) = inner.shim_reads.get_mut(&pid) {
            *reads += 1;
            let settled = *reads > 1;
            let (_, before, after) = inner
                .shim
                .clone()
                .expect("a PID in shim_reads implies with_shim_exec was called");
            if !settled {
                return Some(before);
            }
            if let Some(endpoint) = inner.shim_pending_endpoint.remove(&pid) {
                inner.listeners.insert(pid, endpoint.clone());
                inner.endpoints.insert(endpoint);
            }
            return Some(after);
        }
        inner.processes.get(&pid).cloned()
    }

    fn serving(&self, host: &str, port: u16) -> bool {
        self.0
            .lock()
            .unwrap()
            .endpoints
            .contains(&format!("{host}:{port}"))
    }
}
