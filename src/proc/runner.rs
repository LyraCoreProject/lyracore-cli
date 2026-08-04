use super::CommandSpec;
use crate::{Error, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

pub trait ProcessRunner {
    /// Run to completion, returning stdout.
    fn run_and_wait(&self, cmd: &CommandSpec) -> Result<String>;

    /// Run to completion, feeding `secret` to the child's stdin. The secret is never an argument,
    /// never rendered, and never logged.
    fn run_with_secret_stdin(&self, cmd: &CommandSpec, secret: &[u8]) -> Result<String>;

    /// Start a background process with stdout+stderr appended to `log`, returning its PID.
    fn spawn_logged(&self, cmd: &CommandSpec, log: &Path) -> Result<u32>;

    /// Ask a process to stop (SIGTERM). Callers MUST validate process identity first.
    fn terminate(&self, pid: u32) -> Result<()>;
}

pub struct RealProcessRunner;

impl RealProcessRunner {
    fn finish(cmd: &CommandSpec, output: std::process::Output) -> Result<String> {
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        Err(Error::SubprocessFailed {
            command: cmd.render(),
            code: output.status.code().unwrap_or(1),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

impl ProcessRunner for RealProcessRunner {
    fn run_and_wait(&self, cmd: &CommandSpec) -> Result<String> {
        let output = cmd.build().stdin(Stdio::null()).output()?;
        Self::finish(cmd, output)
    }

    fn run_with_secret_stdin(&self, cmd: &CommandSpec, secret: &[u8]) -> Result<String> {
        let mut child = cmd
            .build()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                Error::Process("child did not expose a stdin pipe for the password".to_string())
            })?;
            stdin.write_all(secret)?;
            // The reader stops at the newline; dropping the pipe then signals EOF.
            stdin.write_all(b"\n")?;
        }
        let output = child.wait_with_output()?;
        Self::finish(cmd, output)
    }

    fn spawn_logged(&self, cmd: &CommandSpec, log: &Path) -> Result<u32> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(log)?;
        let errors = file.try_clone()?;
        let child = cmd
            .build()
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(errors))
            .spawn()?;
        Ok(child.id())
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        // POSIX `kill` rather than a libc dependency; default signal is TERM.
        let status = std::process::Command::new("kill")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Process(format!("could not signal PID {pid}")))
        }
    }
}
