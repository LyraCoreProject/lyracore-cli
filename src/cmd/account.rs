//! `lyracore account create USER [--password-stdin]`.
//!
//! The password never becomes a process argument. It is read into a wiped buffer and handed to
//! `gateway provision USER --password-stdin`, which computes the SRP6 salt/verifier itself — so
//! neither this CLI nor the child ever exposes it through argv, and `ps` shows only the username.

use crate::project::ProjectLayout;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::{Error, Result};
use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};
use zeroize::Zeroizing;

/// Same bound the gateway enforces, so an over-long password is rejected before it is sent.
const MAX_PASSWORD_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSource {
    /// `--password-stdin`: read one line from stdin (scripts, CI, `pass show … |`).
    Stdin,
    /// Interactive: prompt on the TTY with echo disabled.
    Tty,
}

pub fn create(
    project: &ProjectLayout,
    user: &str,
    source: PasswordSource,
    runner: &dyn ProcessRunner,
) -> Result<()> {
    if user.trim().is_empty() {
        return Err(Error::Usage("account create requires a username".to_string()));
    }

    let password = match source {
        PasswordSource::Stdin => read_line_secret(&mut std::io::stdin().lock())?,
        PasswordSource::Tty => read_password_from_tty()?,
    };
    validate(&password)?;

    let gateway = project.gateway_bin();
    if !gateway.exists() {
        return Err(Error::PrerequisiteMissing(format!(
            "{} is not built — run `lyracore dev up` first",
            ProjectLayout::GATEWAY_BIN
        )));
    }

    runner.run_with_secret_stdin(&provision_command(project, user), &password)?;
    println!("✓ provisioned account '{}'.", user.to_uppercase());
    Ok(())
}

/// The provisioning invocation. `user` is an argument; the password is not, and cannot be —
/// `CommandSpec` has no way to carry one.
fn provision_command(project: &ProjectLayout, user: &str) -> CommandSpec {
    CommandSpec::new(project.gateway_bin().to_string_lossy().to_string())
        .arg("provision")
        .arg(user)
        .arg("--password-stdin")
        .env("LYRACORE_DATABASE", ProjectLayout::DATABASE)
        .env("LYRACORE_SPACETIMEDB_URL", ProjectLayout::stdb_uri())
        .env_remove("LYRACORE_SHARD_MAP")
        .env_remove("LYRACORE_SHARD_MAP_FILE")
        .env_remove("LYRACORE_REALM_CORE")
        .env_remove("LYRACORE_REGION_SHARDS")
}

fn validate(password: &[u8]) -> Result<()> {
    if password.is_empty() {
        return Err(Error::Usage(
            "the password was empty — nothing was provisioned".to_string(),
        ));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        // Length only. Never echo the value.
        return Err(Error::Usage(format!(
            "the password is {} bytes; the 1.12.1 client allows at most {MAX_PASSWORD_BYTES}",
            password.len()
        )));
    }
    Ok(())
}

/// Read one line, stopping at LF, into a buffer that is wiped on drop.
fn read_line_secret(reader: &mut impl Read) -> Result<Zeroizing<Vec<u8>>> {
    // Reserve up front: a growing Vec could free an un-wiped copy of the secret.
    let mut secret = Zeroizing::new(Vec::with_capacity(MAX_PASSWORD_BYTES + 2));
    let mut byte = Zeroizing::new([0_u8; 1]);
    loop {
        match reader.read(&mut *byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if secret.len() > MAX_PASSWORD_BYTES + 1 {
                    // Keep reading no further; the length check rejects it.
                    break;
                }
                secret.push(byte[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    if secret.last() == Some(&b'\r') {
        *secret.last_mut().expect("just checked") = 0;
        secret.pop();
    }
    Ok(secret)
}

/// Prompt on the controlling terminal with echo off.
///
/// ponytail: `stty` rather than a termios crate — two subprocesses beat a new dependency, and the
/// echo state is restored even when the read fails. Upgrade path: if this ever needs to survive a
/// SIGINT mid-prompt, take the termios dependency and restore from a signal handler.
fn read_password_from_tty() -> Result<Zeroizing<Vec<u8>>> {
    let tty = std::fs::File::open("/dev/tty").map_err(|_| {
        Error::Usage(
            "no terminal available for a password prompt — pipe it in with `--password-stdin`"
                .to_string(),
        )
    })?;

    eprint!("Password for the new account: ");
    std::io::stderr().flush()?;

    let echo_was_off = set_tty_echo(false).is_ok();
    let result = read_line_secret(&mut BufReader::new(tty));
    if echo_was_off {
        let _ = set_tty_echo(true);
    }
    eprintln!();

    if !echo_was_off {
        return Err(Error::Process(
            "could not disable terminal echo; refusing to read a password that would be visible"
                .to_string(),
        ));
    }
    result
}

fn set_tty_echo(on: bool) -> Result<()> {
    let tty = std::fs::File::open("/dev/tty")?;
    let status = Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(Stdio::from(tty))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Process("stty failed".to_string()))
    }
}

/// Consume a whole reader as lines — used only by tests below.
#[cfg(test)]
fn first_line(input: &str) -> Zeroizing<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(input.as_bytes().to_vec());
    read_line_secret(&mut cursor).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeStack};
    use tempfile::TempDir;

    const SECRET: &str = "hunter2";

    fn project_with_gateway(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let bin = tmp.path().join(ProjectLayout::GATEWAY_BIN);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    // ---- the secret contract ----

    #[test]
    fn the_password_is_never_a_command_argument() {
        let tmp = TempDir::new().unwrap();
        let cmd = provision_command(&project_with_gateway(&tmp), "TEST");

        assert!(cmd.args().contains(&"--password-stdin".to_string()));
        assert!(
            cmd.args().iter().all(|a| a != SECRET),
            "the password must never appear in argv"
        );
        assert!(
            !cmd.render().contains(SECRET),
            "the password must never appear in a rendered command"
        );
        assert!(cmd.render().contains("TEST"), "the username is not secret");
    }

    #[test]
    fn the_password_reaches_the_child_only_over_stdin() {
        let tmp = TempDir::new().unwrap();
        let project = project_with_gateway(&tmp);
        let stack = FakeStack::new();

        stack
            .runner()
            .run_with_secret_stdin(&provision_command(&project, "TEST"), SECRET.as_bytes())
            .unwrap();

        match stack.calls().as_slice() {
            [Call::SecretStdin { spec, secret }] => {
                assert_eq!(secret, SECRET.as_bytes());
                assert!(!spec.render().contains(SECRET));
            }
            other => panic!("expected exactly one stdin-fed call, got {other:?}"),
        }
        // Nothing rendered — i.e. nothing loggable — carries the secret.
        for rendered in stack.rendered() {
            assert!(!rendered.contains(SECRET), "leaked into a log line: {rendered}");
        }
    }

    #[test]
    fn a_failing_provision_does_not_put_the_password_in_the_error() {
        let tmp = TempDir::new().unwrap();
        let project = project_with_gateway(&tmp);
        let stack = FakeStack::new()
            .fail_on("provision", "invalid password: expected 1-16 non-control ASCII bytes");

        let error = stack
            .runner()
            .run_with_secret_stdin(&provision_command(&project, "TEST"), SECRET.as_bytes())
            .unwrap_err();

        assert!(
            !error.to_string().contains(SECRET),
            "the error message leaked the password: {error}"
        );
    }

    // ---- reading ----

    #[test]
    fn a_password_line_stops_at_the_newline_and_strips_crlf() {
        assert_eq!(&*first_line("hunter2\n"), b"hunter2");
        assert_eq!(&*first_line("hunter2\r\n"), b"hunter2");
        assert_eq!(&*first_line("hunter2"), b"hunter2");
        // A second line is not part of the password.
        assert_eq!(&*first_line("hunter2\nrest\n"), b"hunter2");
    }

    #[test]
    fn spaces_inside_a_password_are_preserved() {
        assert_eq!(&*first_line("two words\n"), b"two words");
    }

    #[test]
    fn empty_and_overlong_passwords_are_refused() {
        assert!(validate(b"").is_err());
        assert!(validate(b"0123456789abcdefg").is_err());
        assert!(validate(b"0123456789abcdef").is_ok());
    }

    #[test]
    fn an_overlong_password_is_not_echoed_in_its_own_rejection() {
        let long = "p".repeat(64);
        let error = validate(long.as_bytes()).unwrap_err().to_string();
        assert!(!error.contains(&long), "rejection echoed the password: {error}");
    }

    #[test]
    fn a_blank_username_is_a_usage_error() {
        let tmp = TempDir::new().unwrap();
        let project = project_with_gateway(&tmp);
        let stack = FakeStack::new();
        let error = create(&project, "  ", PasswordSource::Stdin, &stack.runner()).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.calls().is_empty(), "nothing should have been run");
    }
}
