//! Operator controls for Account-owned Alpha Test Tools on a named Realm-core database.

use crate::cmd::dev::reducer_url;
use crate::http::HttpClient;
use crate::proc::{ProcessInspector, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

const ENROLLMENT_QUERY: &str = "SELECT enabled FROM game_alpha_test_tools_enrollment";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ReadEnrollment,
    SetEnrollment(bool),
    Grant(String),
    Revoke(String),
}

pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    inspector: &dyn ProcessInspector,
    http: &dyn HttpClient,
    realm_core: &str,
    action: Action,
) -> Result<()> {
    validate_realm_core(realm_core)?;
    let action = normalize_action(action)?;
    let credential = crate::token::resolve_existing(runner, &project.token_file())?;
    if !inspector.serving("127.0.0.1", ProjectLayout::STDB_PORT) {
        return Err(Error::PrerequisiteMissing(
            "the local SpacetimeDB node is not answering on 127.0.0.1:3000; run `lyracore dev up` first"
                .to_string(),
        ));
    }

    match action {
        Action::ReadEnrollment => {
            let body = http
                .post_sql(
                    &sql_url(realm_core),
                    Some(credential.token()),
                    ENROLLMENT_QUERY,
                )
                .map_err(|error| operation_error(error, realm_core))?;
            let enabled = parse_enrollment(&body)?;
            println!(
                "Alpha Test Tools automatic enrollment is {} on Realm-core {realm_core}.",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        Action::SetEnrollment(enabled) => {
            call(
                http,
                credential.token(),
                realm_core,
                "set_alpha_test_tools_enrollment",
                if enabled { "[true]" } else { "[false]" },
            )?;
            println!(
                "{} Alpha Test Tools automatic enrollment on Realm-core {realm_core}.",
                if enabled { "Enabled" } else { "Disabled" }
            );
        }
        Action::Grant(account) => {
            call(
                http,
                credential.token(),
                realm_core,
                "grant_alpha_test_tools",
                &serde_json::to_string(&(account.as_str(),))?,
            )?;
            println!("Granted Alpha Test Tools to Account {account} on Realm-core {realm_core}.");
        }
        Action::Revoke(account) => {
            call(
                http,
                credential.token(),
                realm_core,
                "revoke_alpha_test_tools",
                &serde_json::to_string(&(account.as_str(),))?,
            )?;
            println!("Revoked Alpha Test Tools from Account {account} on Realm-core {realm_core}.");
        }
    }
    Ok(())
}

fn call(
    http: &dyn HttpClient,
    token: &str,
    realm_core: &str,
    reducer: &str,
    arguments: &str,
) -> Result<()> {
    http.post_json(&reducer_url(realm_core, reducer), Some(token), arguments)
        .map(|_| ())
        .map_err(|error| operation_error(error, realm_core))
}

fn operation_error(error: Error, realm_core: &str) -> Error {
    Error::Process(format!(
        "{error}\n  Alpha Test Tools operation failed on Realm-core {realm_core}; check `lyracore dev status` and the named database"
    ))
}

fn normalize_action(action: Action) -> Result<Action> {
    match action {
        Action::Grant(account) => normalize_account(&account).map(Action::Grant),
        Action::Revoke(account) => normalize_account(&account).map(Action::Revoke),
        other => Ok(other),
    }
}

fn normalize_account(account: &str) -> Result<String> {
    if account.is_empty() {
        return Err(Error::Usage(
            "`account alpha-test-tools grant|revoke` needs an Account name".to_string(),
        ));
    }
    if account.len() > 16
        || !account
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
    {
        return Err(Error::Usage(
            "Account names must be 1-16 non-control ASCII bytes".to_string(),
        ));
    }
    Ok(account.to_ascii_uppercase())
}

pub fn validate_realm_core(realm_core: &str) -> Result<()> {
    if realm_core.is_empty() {
        return Err(Error::Usage(
            "`account alpha-test-tools` needs a named Realm-core database".to_string(),
        ));
    }
    if realm_core.starts_with('-')
        || !realm_core
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(Error::Usage(format!(
            "'{realm_core}' is not a Realm-core database name"
        )));
    }
    Ok(())
}

fn sql_url(realm_core: &str) -> String {
    format!("{}/v1/database/{realm_core}/sql", ProjectLayout::stdb_uri())
}

fn parse_enrollment(body: &str) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        Error::Process(format!(
            "Realm-core returned an unreadable Alpha Test Tools enrollment result: {error}"
        ))
    })?;
    let rows = value
        .as_array()
        .and_then(|statements| statements.first())
        .and_then(|statement| statement.get("rows"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            Error::Process("Realm-core returned no Alpha Test Tools enrollment row set".to_string())
        })?;
    if rows.is_empty() {
        return Ok(true);
    }
    rows.first()
        .and_then(serde_json::Value::as_array)
        .and_then(|row| row.first())
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            Error::Process(
                "Realm-core returned a non-boolean Alpha Test Tools enrollment value".to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::{FakeHttp, MINTED_TOKEN};
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn ready(tmp: &TempDir) -> (ProjectLayout, FakeStack) {
        let project = project(tmp);
        let stack = FakeStack::new().with_port(ProjectLayout::STDB_PORT);
        crate::token::resolve_or_mint(
            &stack
                .clone()
                .fail_on("login show", "not logged in")
                .runner(),
            &FakeHttp::new(),
            &project.token_file(),
            &ProjectLayout::stdb_uri(),
        )
        .unwrap();
        (project, stack)
    }

    #[test]
    fn every_action_targets_only_the_named_realm_core() {
        let tmp = TempDir::new().unwrap();
        let (project, stack) = ready(&tmp);

        for (action, path, body) in [
            (
                Action::SetEnrollment(true),
                "set_alpha_test_tools_enrollment",
                "[true]",
            ),
            (
                Action::SetEnrollment(false),
                "set_alpha_test_tools_enrollment",
                "[false]",
            ),
            (
                Action::Grant("tester".into()),
                "grant_alpha_test_tools",
                r#"["TESTER"]"#,
            ),
            (
                Action::Revoke("TeStEr".into()),
                "revoke_alpha_test_tools",
                r#"["TESTER"]"#,
            ),
        ] {
            let http = FakeHttp::new();
            run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &http,
                "realm-one",
                action,
            )
            .unwrap();
            let requests = http.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url, reducer_url("realm-one", path));
            assert_eq!(requests[0].body, body);
            assert_eq!(requests[0].bearer.as_deref(), Some(MINTED_TOKEN));
        }
    }

    #[test]
    fn enrollment_read_uses_sql_and_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let (project, stack) = ready(&tmp);
        let http = FakeHttp::responding("/sql", r#"[{"schema":{"elements":[]},"rows":[[false]]}]"#);

        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &http,
            "realm-one",
            Action::ReadEnrollment,
        )
        .unwrap();

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, sql_url("realm-one"));
        assert_eq!(requests[0].body, ENROLLMENT_QUERY);
        assert!(!requests[0].url.contains("/call/"));
    }

    #[test]
    fn a_missing_enrollment_row_keeps_the_module_default_of_enabled() {
        assert_eq!(
            parse_enrollment(r#"[{"schema":{"elements":[]},"rows":[]}]"#).unwrap(),
            true
        );
    }

    #[test]
    fn invalid_arguments_fail_before_credentials_or_requests() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = FakeStack::new();
        let http = FakeHttp::new();

        for (realm_core, action) in [
            ("", Action::SetEnrollment(true)),
            ("--realm", Action::SetEnrollment(true)),
            ("realm-one", Action::Grant("".into())),
            ("realm-one", Action::Revoke("bad\nname".into())),
        ] {
            let error = run(
                &project,
                &stack.runner(),
                &stack.inspector(),
                &http,
                realm_core,
                action,
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        }
        assert!(stack.calls().is_empty());
        assert!(http.requests().is_empty());
    }

    #[test]
    fn a_missing_credential_or_node_fails_before_any_operation() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let no_credential = FakeStack::new()
            .with_port(ProjectLayout::STDB_PORT)
            .fail_on("login show", "not logged in");
        let http = FakeHttp::new();
        run(
            &project,
            &no_credential.runner(),
            &no_credential.inspector(),
            &http,
            "realm-one",
            Action::SetEnrollment(true),
        )
        .unwrap_err();
        assert!(http.requests().is_empty());

        let tmp = TempDir::new().unwrap();
        let (project, _) = ready(&tmp);
        let no_node = FakeStack::new();
        run(
            &project,
            &no_node.runner(),
            &no_node.inspector(),
            &http,
            "realm-one",
            Action::Grant("TEST".into()),
        )
        .unwrap_err();
        assert!(http.requests().is_empty());
    }

    #[test]
    fn the_coordinator_credential_is_never_rendered_or_sent_as_data() {
        let tmp = TempDir::new().unwrap();
        let (project, stack) = ready(&tmp);
        let http = FakeHttp::new();
        run(
            &project,
            &stack.runner(),
            &stack.inspector(),
            &http,
            "realm-one",
            Action::Grant("TEST".into()),
        )
        .unwrap();

        let request = &http.requests()[0];
        assert_eq!(request.bearer.as_deref(), Some(MINTED_TOKEN));
        assert!(!request.url.contains(MINTED_TOKEN));
        assert!(!request.body.contains(MINTED_TOKEN));
        assert!(stack
            .rendered()
            .iter()
            .all(|rendered| !rendered.contains(MINTED_TOKEN)));
    }
}
