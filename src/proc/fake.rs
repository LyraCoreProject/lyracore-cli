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
    SecretStdin { spec: CommandSpec, secret: Vec<u8> },
    Spawn { spec: CommandSpec, log: PathBuf },
    Terminate(u32),
}

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    processes: HashMap<u32, String>,
    ports: HashSet<u16>,
    next_pid: u32,
    failures: HashMap<String, String>,
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
        self.0.lock().unwrap().processes.insert(pid, identity.to_string());
        self
    }

    /// A port already being served, by something this CLI did not start.
    pub fn with_port(self, port: u16) -> Self {
        self.0.lock().unwrap().ports.insert(port);
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
                Call::Wait(spec) | Call::SecretStdin { spec, .. } | Call::Spawn { spec, .. } => {
                    spec.render()
                }
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

/// Which port a spawned command starts serving, so a fake spawn behaves like a real one.
fn port_for(render: &str) -> Option<u16> {
    if render.contains("spacetime start") {
        Some(ProjectLayout::STDB_PORT)
    } else if render.contains("gateway") {
        Some(ProjectLayout::WORLD_PORT)
    } else {
        None
    }
}

impl ProcessRunner for FakeProcessRunner {
    fn run_and_wait(&self, cmd: &CommandSpec) -> Result<String> {
        self.record(Call::Wait(cmd.clone()), &cmd.render())?;
        Ok(String::new())
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

        let mut inner = self.0.lock().unwrap();
        inner.next_pid += 1;
        let pid = inner.next_pid;
        inner.processes.insert(pid, format!("fake-start {render}"));
        if let Some(port) = port_for(&render) {
            inner.ports.insert(port);
        }
        Ok(pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        self.record(Call::Terminate(pid), "kill")?;
        self.0.lock().unwrap().processes.remove(&pid);
        Ok(())
    }
}

pub struct FakeProcessInspector(Arc<Mutex<Inner>>);

impl ProcessInspector for FakeProcessInspector {
    fn identity(&self, pid: u32) -> Option<String> {
        self.0.lock().unwrap().processes.get(&pid).cloned()
    }

    fn port_serving(&self, port: u16) -> bool {
        self.0.lock().unwrap().ports.contains(&port)
    }
}
