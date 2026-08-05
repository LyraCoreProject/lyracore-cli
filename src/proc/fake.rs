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

/// The auth token a `FakeStack`'s `spacetime` CLI hands out. A distinctive literal, so a test can
/// assert it is absent from everything rendered, logged or serialized.
pub const FAKE_TOKEN: &str = "eyJmYWtl.TOKEN-must-never-be-rendered.SIG";

/// The versions a `FakeStack`'s toolchain reports. A fixture checkout that pins these is a machine
/// whose tools match — the ordinary case; a fixture pinning anything else models the drift that
/// `lyracore preflight`'s check 0 exists to catch.
pub const FAKE_RUST_VERSION: &str = "1.93.0";
pub const FAKE_SPACETIME_VERSION: &str = "2.5.0";

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

        let endpoint = endpoint_for(cmd);
        let mut inner = self.0.lock().unwrap();
        inner.next_pid += 1;
        let pid = inner.next_pid;
        inner.processes.insert(pid, format!("fake-start {render}"));
        if let Some(endpoint) = endpoint {
            inner.endpoints.insert(endpoint.clone());
            inner.listeners.insert(pid, endpoint);
        }
        Ok(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        self.record(Call::Terminate(pid), "kill")?;
        let mut inner = self.0.lock().unwrap();
        inner.processes.remove(&pid);
        if let Some(endpoint) = inner.listeners.remove(&pid) {
            inner.endpoints.remove(&endpoint);
        }
        Ok(())
    }
}

pub struct FakeProcessInspector(Arc<Mutex<Inner>>);

impl ProcessInspector for FakeProcessInspector {
    fn identity(&self, pid: u32) -> Option<String> {
        self.0.lock().unwrap().processes.get(&pid).cloned()
    }

    fn serving(&self, host: &str, port: u16) -> bool {
        self.0
            .lock()
            .unwrap()
            .endpoints
            .contains(&format!("{host}:{port}"))
    }
}
