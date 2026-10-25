use crate::http::HttpClient;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};
use wow_srp::normalized_string::NormalizedString;
use wow_srp::server::SrpVerifier;
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSource {
    Stdin,
    Tty,
}

pub fn create(
    project: &ProjectLayout,
    user: &str,
    source: PasswordSource,
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
) -> Result<()> {
    create_with_password(project, user, runner, http, || read_password(source))
}

fn create_with_password(
    project: &ProjectLayout,
    user: &str,
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
    password: impl FnOnce() -> Result<Zeroizing<Vec<u8>>>,
) -> Result<()> {
    if user.trim().is_empty() {
        return Err(Error::Usage("account create requires a username".into()));
    }
    let username = NormalizedString::new(user)
        .map_err(|_| Error::Usage("username must contain 1-16 non-control ASCII bytes".into()))?;
    let credential = crate::token::resolve_existing(runner, &project.token_file())?;
    let topology = project.local_topology()?;
    let password = password()?;
    validate(&password)?;
    let password = std::str::from_utf8(&password)
        .ok()
        .and_then(|password| NormalizedString::new(password).ok())
        .ok_or_else(|| Error::Usage("password must contain 1-16 non-control ASCII bytes".into()))?;
    let verifier = SrpVerifier::from_username_and_password(username.clone(), password);
    let body = Zeroizing::new(
        serde_json::json!([
            username.as_ref(),
            verifier.salt(),
            verifier.password_verifier()
        ])
        .to_string(),
    );
    let mut destinations = vec![ProjectLayout::DATABASE];
    destinations.extend(topology.realm_core());
    for database in destinations {
        http.post_json(
            &super::dev::reducer_url(database, "provision_account"),
            Some(credential.token()),
            &body,
        )
        .map_err(|error| super::dev::operator_call_failure(project, database, error))?;
    }
    println!("Provisioned Account '{}'.", username.as_ref());
    Ok(())
}

pub fn read_password(source: PasswordSource) -> Result<Zeroizing<Vec<u8>>> {
    let password = match source {
        PasswordSource::Stdin => read_line_secret(&mut std::io::stdin().lock())?,
        PasswordSource::Tty => read_password_from_tty()?,
    };
    validate(&password)?;
    Ok(password)
}

pub(crate) fn validate(password: &[u8]) -> Result<()> {
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

fn read_line_secret(reader: &mut impl Read) -> Result<Zeroizing<Vec<u8>>> {
    // Reserve up front: a growing Vec could free an un-wiped copy of the secret.
    let mut secret = Zeroizing::new(Vec::with_capacity(MAX_PASSWORD_BYTES + 2));
    let mut byte = Zeroizing::new([0_u8; 1]);
    let mut ended_with_lf = false;
    loop {
        match reader.read(&mut *byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => {
                ended_with_lf = true;
                break;
            }
            Ok(_) => {
                if secret.len() > MAX_PASSWORD_BYTES + 1 {
                    break;
                }
                secret.push(byte[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    if ended_with_lf && secret.last() == Some(&b'\r') {
        *secret.last_mut().expect("just checked") = 0;
        secret.pop();
    }
    Ok(secret)
}

fn read_password_from_tty() -> Result<Zeroizing<Vec<u8>>> {
    let tty = std::fs::File::open("/dev/tty").map_err(|_| {
        Error::Usage(
            "no terminal available for a password prompt — pipe it in with `--password-stdin`"
                .to_string(),
        )
    })?;

    eprint!("Account password: ");
    std::io::stderr().flush()?;

    set_tty_echo(false).map_err(|_| {
        Error::Process("could not disable terminal echo; use --password-stdin".to_string())
    })?;
    let result = read_line_secret(&mut BufReader::new(tty));
    let restored = set_tty_echo(true);
    eprintln!();

    restored?;
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

#[cfg(test)]
fn first_line(input: &str) -> Zeroizing<Vec<u8>> {
    read_line_secret(&mut std::io::Cursor::new(input.as_bytes())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeHttp;
    use crate::proc::fake::{FakeStack, FAKE_TOKEN};
    use tempfile::TempDir;

    #[test]
    fn native_account_provisioning_sends_only_srp_values_to_the_single_shard() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        let stack = FakeStack::new();
        let http = FakeHttp::new();
        create_with_password(&project, "alice", &stack.runner(), &http, || {
            Ok(Zeroizing::new(b"hunter2".to_vec()))
        })
        .unwrap();
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .url
            .ends_with("/lyracore/call/provision_account"));
        assert_eq!(requests[0].bearer.as_deref(), Some(FAKE_TOKEN));
        let values: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(values[0], "ALICE");
        let salt: [u8; 32] = serde_json::from_value(values[1].clone()).unwrap();
        let actual: [u8; 32] = serde_json::from_value(values[2].clone()).unwrap();
        let proof = SrpVerifier::from_database_values(
            NormalizedString::new("ALICE").unwrap(),
            actual,
            salt,
        )
        .into_proof();
        let client = wow_srp::client::SrpClientChallenge::new(
            NormalizedString::new("ALICE").unwrap(),
            NormalizedString::new("HUNTER2").unwrap(),
            wow_srp::GENERATOR,
            wow_srp::LARGE_SAFE_PRIME_LITTLE_ENDIAN,
            wow_srp::PublicKey::from_le_bytes(*proof.server_public_key()).unwrap(),
            salt,
        );
        proof
            .into_server(
                wow_srp::PublicKey::from_le_bytes(*client.client_public_key()).unwrap(),
                *client.client_proof(),
            )
            .unwrap();
        assert!(!requests[0].body.contains("hunter2"));
        assert!(stack
            .rendered()
            .iter()
            .all(|cmd| !cmd.contains("gateway") && !cmd.contains("hunter2")));
    }

    #[test]
    fn missing_credentials_fail_before_reading_the_password() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = FakeStack::new().fail_on("login show", "not logged in");
        let http = FakeHttp::new();
        let error = create_with_password(&project, "TEST", &stack.runner(), &http, || {
            panic!("password must not be read")
        })
        .unwrap_err();
        assert!(error.to_string().contains("lyracore dev up"));
        assert!(http.requests().is_empty());
    }

    #[test]
    fn invalid_password_is_never_sent_or_echoed() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let http = FakeHttp::new();
        let error =
            create_with_password(&project, "TEST", &FakeStack::new().runner(), &http, || {
                Ok(Zeroizing::new(b"private\tsecret".to_vec()))
            })
            .unwrap_err();
        assert!(!error.to_string().contains("private"));
        assert!(http.requests().is_empty());
    }

    #[test]
    fn a_password_line_stops_at_the_newline_and_strips_crlf() {
        assert_eq!(&*first_line("hunter2\n"), b"hunter2");
        assert_eq!(&*first_line("hunter2\r\n"), b"hunter2");
        assert_eq!(&*first_line("hunter2"), b"hunter2");
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
        assert!(
            !error.contains(&long),
            "rejection echoed the password: {error}"
        );
    }

    #[test]
    fn invalid_username_fails_before_reading_the_password() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = FakeStack::new();
        let http = FakeHttp::new();
        let error = create_with_password(&project, "", &stack.runner(), &http, || {
            panic!("password must not be read")
        })
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.calls().is_empty());
        assert!(http.requests().is_empty());
    }
}
