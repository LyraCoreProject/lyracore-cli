//! The coordinator's auth token — the one credential the fixture cannot run without.
//!
//! `game_account` and `game_session` are PRIVATE module tables. The gateway's coordinator
//! connection subscribes to both, and `provision_account` is `require_operator`-gated, so an
//! anonymous connection cannot read an account, cannot answer an SRP6 challenge, and cannot
//! provision anything. What makes the connection privileged is `LYRACORE_COORDINATOR_TOKEN`
//! carrying the identity that claimed the operator.
//!
//! Without it the gateway does not fail fast: it starts, warns, and then dies ~15s later on
//! "coordinator subscriptions not applied within 15s", which reads like a node problem rather
//! than a credential problem.
//!
//! ## Where the credential comes from (#297)
//!
//! In order, and the order is the whole design:
//!
//! 1. **`.lyracore/coordinator-token`** — a server-issued token this CLI minted on an earlier run.
//!    It wins, because it is the identity that already called `claim_operator`: `claim_operator`
//!    is idempotent for the SAME identity and refuses a different one, so preferring anything else
//!    once this file exists would lock the checkout out of its own database.
//! 2. **`spacetime login show --token`** — the dev-machine case. If the contributor already uses
//!    SpacetimeDB, their existing identity is reused and nothing new is minted or stored.
//! 3. **`POST /v1/identity` on the local node** — a SERVER-ISSUED identity, minted from the node
//!    `dev up` just started and persisted at 0600 for step 1.
//!
//! Step 3 is what makes the quickstart anonymous. `spacetime login` (2.5.0) offers only the
//! spacetimedb.com browser flow, so requiring it would put a third-party account signup in front of
//! `git clone && ./lyracore dev up` — and a server-issued token is exactly as privileged here: the
//! node issues it, the module trusts whoever claimed the operator, and this CLI claims the operator
//! with the very token it just minted.
//!
//! The login token is asked of the `spacetime` CLI rather than parsed out of `cli.toml`, because
//! the config file's location is the CLI's business and differs per platform — the production
//! recipe in `docs/danger-zones.md` §3 greps `~/.config/spacetime/cli.toml` with `grep -oP`, which
//! is both Linux-only twice over (the path and the PCRE flag) and exactly what this CLI must not do.

use crate::http::{self, HttpClient};
use crate::proc::{CommandSpec, ProcessRunner};
use crate::{Error, Result};
use std::path::Path;

/// The variable the gateway reads it from (`gateway/src/config.rs`).
pub const TOKEN_VAR: &str = "LYRACORE_COORDINATOR_TOKEN";

/// Shortest thing we will believe is a token. Real ones are JWTs, i.e. hundreds of characters;
/// this only has to reject a stray word from a reworded banner or a truncated file.
const MIN_TOKEN_LEN: usize = 16;

/// The node endpoint that mints a server-issued identity.
const IDENTITY_PATH: &str = "/v1/identity";

/// Where a credential came from. Besides diagnostics it decides one thing: whether
/// `claim_operator` can be called through the `spacetime` CLI (which acts as the LOGIN identity)
/// or must be called with this token directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Minted by an earlier run and read back from `.lyracore/coordinator-token`.
    Persisted,
    /// The `spacetime` CLI's own login.
    SpacetimeLogin,
    /// Minted from the local node during this run.
    ServerIssued,
}

/// A coordinator credential. Its `Debug` is redacted, so it cannot reach a log line by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    token: String,
    pub source: Source,
}

impl Credential {
    pub fn token(&self) -> &str {
        &self.token
    }

    /// True when the `spacetime` CLI is NOT this identity, so anything privileged must be done
    /// with the token rather than by shelling out to `spacetime call`.
    pub fn is_server_issued(&self) -> bool {
        matches!(self.source, Source::Persisted | Source::ServerIssued)
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("source", &self.source)
            .field("token", &"<redacted>")
            .finish()
    }
}

pub fn token_command() -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("login")
        .arg("show")
        .arg("--token")
}

/// `dev up`: the full ladder, minting from `node_uri` as the last resort.
///
/// `store` is `.lyracore/coordinator-token`, inside the git-ignored state directory.
pub fn resolve_or_mint(
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
    store: &Path,
    node_uri: &str,
) -> Result<Credential> {
    if let Some(credential) = existing(runner, store) {
        return Ok(credential);
    }
    // Nothing local and no login: mint an identity from the node this `dev up` just started.
    // Announced, because a contributor should be able to see WHY a credential file appeared.
    println!(
        "· no SpacetimeDB login found — minting a local identity from {node_uri} (no \
         spacetimedb.com account needed)..."
    );
    let token = mint(http, node_uri)?;
    persist(store, &token)?;
    Ok(Credential {
        token,
        source: Source::ServerIssued,
    })
}

/// `account create`: the same ladder WITHOUT the mint.
///
/// Minting here would be actively wrong: a fresh identity has not claimed the operator, so
/// `provision_account` would refuse it — after the password had already been read. The refusal
/// names the command that does mint and claim.
pub fn resolve_existing(runner: &dyn ProcessRunner, store: &Path) -> Result<Credential> {
    existing(runner, store).ok_or_else(|| {
        Error::PrerequisiteMissing(format!(
            "no coordinator credential — {} does not exist and `spacetime login show --token` \
             printed no token. Run `lyracore dev up` first: it mints a server-issued identity from \
             the local node and claims it as the operator (no spacetimedb.com account needed).",
            store.display()
        ))
    })
}

/// Steps 1 and 2 of the ladder: a credential that already exists somewhere.
fn existing(runner: &dyn ProcessRunner, store: &Path) -> Option<Credential> {
    if let Some(token) = read_store(store) {
        return Some(Credential {
            token,
            source: Source::Persisted,
        });
    }
    // A `spacetime` CLI that is missing, broken, or logged out is not an error here — it is just
    // an absent credential, and the caller decides whether that is fatal.
    let output = runner.run_and_wait(&token_command()).ok()?;
    parse(&output).map(|token| Credential {
        token,
        source: Source::SpacetimeLogin,
    })
}

/// Ask the local node for a server-issued identity: `POST /v1/identity` -> `{identity, token}`.
pub fn mint(http: &dyn HttpClient, node_uri: &str) -> Result<String> {
    let url = format!("{node_uri}{IDENTITY_PATH}");
    let body = http.post_json(&url, None, "").map_err(|e| {
        Error::PrerequisiteMissing(format!(
            "could not mint a local identity from the SpacetimeDB node ({e}). The node must be \
             running before a coordinator credential can be issued — check `lyracore dev status` \
             and `lyracore dev logs spacetime`."
        ))
    })?;

    // The body carries the token, so nothing here may quote it — not even on the failure path.
    http::json_field(&body, "token")
        .filter(|token| token.len() >= MIN_TOKEN_LEN)
        .ok_or_else(|| {
            Error::PrerequisiteMissing(format!(
                "{url} did not return an identity token. That endpoint is how a local SpacetimeDB \
                 node issues one; a different service answering on that port would explain it."
            ))
        })
}

/// Read a persisted token, treating an unreadable or implausible file as absent.
fn read_store(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?.trim().to_string();
    (token.len() >= MIN_TOKEN_LEN).then_some(token)
}

/// Write the minted token so the NEXT run is the same identity.
///
/// 0600 from creation: this is the one place in the CLI where a credential touches the disk, and a
/// checkout lives in a shared machine's home directory as often as not.
fn persist(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    // `mode()` applies only when the file is CREATED, so a 0644 one left by a hand-edit (or an
    // older CLI) would keep its permissions through the rewrite. Set them explicitly too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Pull the token out of the CLI's human-readable banner:
///
/// ```text
/// You are logged in as <identity>
/// Your auth token (don't share this!) is <token>
/// ```
///
/// The token is the last field of the line that mentions the auth token, which survives a
/// reworded sentence around it. Anything that does not look like a token at all reads as absent,
/// so "not logged in" falls through to the mint instead of becoming a bogus credential.
pub fn parse(output: &str) -> Option<String> {
    let token = output
        .lines()
        .find(|line| line.contains("auth token"))?
        .split_whitespace()
        .last()?;
    (token.len() >= MIN_TOKEN_LEN).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::{FakeHttp, MINTED_TOKEN};
    use crate::proc::fake::{FakeStack, FAKE_TOKEN};
    use tempfile::TempDir;

    /// Verbatim from `spacetime login show --token` on 2.7.1.
    const REAL_BANNER: &str = "You are logged in as some-identity\n\
                               Your auth token (don't share this!) is eyJhbGciOiJFUzI1NiJ9.PAYLOAD.SIG\n";

    const NODE: &str = "http://127.0.0.1:3000";

    fn store(tmp: &TempDir) -> std::path::PathBuf {
        tmp.path().join(".lyracore/coordinator-token")
    }

    /// A machine with no SpacetimeDB login at all — the fresh-host case #297 is about.
    fn logged_out() -> FakeStack {
        FakeStack::new().fail_on("login show", "You are not logged in")
    }

    #[test]
    fn the_token_is_read_out_of_the_real_banner() {
        assert_eq!(
            parse(REAL_BANNER).as_deref(),
            Some("eyJhbGciOiJFUzI1NiJ9.PAYLOAD.SIG")
        );
    }

    #[test]
    fn a_logged_out_cli_yields_no_token_rather_than_a_word_from_its_banner() {
        for banner in [
            "You are not logged in\n",
            "",
            // The line is there but the value is missing or junk — a short trailing word must not
            // be handed to the gateway as a credential.
            "Your auth token (don't share this!) is\n",
            "Your auth token (don't share this!) is none\n",
        ] {
            assert_eq!(parse(banner), None, "{banner:?} must not yield a token");
        }
    }

    // ---- the resolution ladder ----

    #[test]
    fn an_existing_spacetime_login_is_preferred_over_minting_a_second_identity() {
        let tmp = TempDir::new().unwrap();
        let stack = FakeStack::new();
        let http = FakeHttp::new();

        let credential = resolve_or_mint(&stack.runner(), &http, &store(&tmp), NODE).unwrap();

        assert_eq!(credential.source, Source::SpacetimeLogin);
        assert_eq!(credential.token(), FAKE_TOKEN);
        assert!(
            http.requests().is_empty(),
            "a logged-in machine must not be given a second identity: {:?}",
            http.requests()
        );
        assert!(
            !store(&tmp).exists(),
            "the contributor's own login must not be copied into the checkout"
        );
    }

    #[test]
    fn a_logged_out_host_mints_a_server_issued_identity_from_the_local_node() {
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();

        let credential =
            resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap();

        assert_eq!(credential.source, Source::ServerIssued);
        assert_eq!(credential.token(), MINTED_TOKEN);
        // Minted from the node, unauthenticated, and with nothing in the URL.
        let mints = http.mints();
        assert_eq!(mints.len(), 1, "{mints:?}");
        assert_eq!(mints[0].url, "http://127.0.0.1:3000/v1/identity");
        assert_eq!(mints[0].bearer, None);
    }

    #[test]
    fn a_second_run_reuses_the_same_identity_rather_than_minting_a_new_one() {
        // The lock-out this prevents: `claim_operator` is idempotent for the same identity and
        // refuses a different one, so a fresh identity per run would leave the second `dev up`
        // unable to claim — and unable to provision through an operator it does not own.
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();

        let first = resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap();
        let second = resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap();

        assert_eq!(first.token(), second.token());
        assert_eq!(second.source, Source::Persisted);
        assert_eq!(
            http.mints().len(),
            1,
            "the node was asked for a second identity"
        );
    }

    #[test]
    fn a_persisted_identity_outranks_a_later_spacetime_login() {
        // A contributor who logs in AFTER a `dev up` has already claimed the operator: the login
        // is a different identity, and using it would hit "operator only" on every provision.
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();
        resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap();

        let credential =
            resolve_or_mint(&FakeStack::new().runner(), &http, &store(&tmp), NODE).unwrap();

        assert_eq!(credential.source, Source::Persisted);
        assert_eq!(credential.token(), MINTED_TOKEN);
    }

    #[test]
    fn the_ladder_is_persisted_then_login_then_mint() {
        // The order in one place, so a reordering has to break a test that says why.
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();

        // Nothing anywhere -> mint.
        assert_eq!(
            resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE)
                .unwrap()
                .source,
            Source::ServerIssued
        );
        // Persisted present -> persisted, whatever the login says.
        assert_eq!(
            resolve_or_mint(&FakeStack::new().runner(), &http, &store(&tmp), NODE)
                .unwrap()
                .source,
            Source::Persisted
        );
        // Persisted gone, login present -> login.
        std::fs::remove_file(store(&tmp)).unwrap();
        assert_eq!(
            resolve_or_mint(&FakeStack::new().runner(), &http, &store(&tmp), NODE)
                .unwrap()
                .source,
            Source::SpacetimeLogin
        );
    }

    // ---- the secret contract ----

    #[cfg(unix)]
    #[test]
    fn the_persisted_token_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();
        resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap();

        let mode = std::fs::metadata(store(&tmp)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential file is {mode:o}, must be 600");
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_existing_world_readable_file_is_tightened_when_rewritten() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "short").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // "short" is not a plausible token, so this run mints and rewrites the file.
        resolve_or_mint(&logged_out().runner(), &FakeHttp::new(), &path, NODE).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential file is {mode:o}, must be 600");
    }

    #[test]
    fn resolving_never_renders_the_credential_it_gets_back() {
        let stack = FakeStack::new();
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::new();
        let credential = resolve_or_mint(&stack.runner(), &http, &store(&tmp), NODE).unwrap();

        // One read-only query, and the token is not in it — the value arrives on the child's
        // stdout, so nothing loggable ever carried it.
        assert_eq!(
            stack.rendered(),
            vec!["spacetime login show --token".to_string()]
        );
        for rendered in stack.rendered() {
            assert!(
                !rendered.contains(credential.token()),
                "leaked into: {rendered}"
            );
        }
        // ...and the redacted Debug is what a stray `{:?}` would print.
        let debug = format!("{credential:?}");
        assert!(!debug.contains(credential.token()), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    #[test]
    fn a_minted_credential_is_not_rendered_either() {
        let tmp = TempDir::new().unwrap();
        let stack = logged_out();
        let http = FakeHttp::new();
        let credential = resolve_or_mint(&stack.runner(), &http, &store(&tmp), NODE).unwrap();

        for rendered in stack.rendered() {
            assert!(
                !rendered.contains(credential.token()),
                "leaked into: {rendered}"
            );
        }
        for request in http.requests() {
            assert!(!request.url.contains(credential.token()), "{}", request.url);
            assert!(
                !request.body.contains(credential.token()),
                "{}",
                request.body
            );
        }
        assert!(!format!("{credential:?}").contains(credential.token()));
    }

    // ---- failures ----

    #[test]
    fn a_node_that_cannot_mint_is_a_prerequisite_error_that_says_where_to_look() {
        let tmp = TempDir::new().unwrap();
        let http = FakeHttp::failing("service unavailable");
        let error = resolve_or_mint(&logged_out().runner(), &http, &store(&tmp), NODE).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(error.to_string().contains("SpacetimeDB node"), "{error}");
        assert!(error.to_string().contains("dev logs spacetime"), "{error}");
        assert!(!store(&tmp).exists(), "a failed mint must persist nothing");
    }

    #[test]
    fn account_create_refuses_rather_than_minting_an_identity_that_is_not_the_operator() {
        let tmp = TempDir::new().unwrap();
        let error = resolve_existing(&logged_out().runner(), &store(&tmp)).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(
            error.to_string().contains("lyracore dev up"),
            "the refusal must name the command that mints and claims: {error}"
        );
        assert!(
            error.to_string().contains("coordinator-token"),
            "and the file it looked for: {error}"
        );
    }

    #[test]
    fn account_create_uses_whatever_dev_up_persisted() {
        let tmp = TempDir::new().unwrap();
        resolve_or_mint(&logged_out().runner(), &FakeHttp::new(), &store(&tmp), NODE).unwrap();

        let credential = resolve_existing(&logged_out().runner(), &store(&tmp)).unwrap();
        assert_eq!(credential.source, Source::Persisted);
        assert_eq!(credential.token(), MINTED_TOKEN);
    }

    #[test]
    fn a_truncated_credential_file_is_treated_as_absent() {
        // Half a token is worse than none: the gateway would start and die 15s later.
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "eyJ\n").unwrap();

        assert_eq!(read_store(&path), None);
        assert_eq!(
            resolve_or_mint(&FakeStack::new().runner(), &FakeHttp::new(), &path, NODE)
                .unwrap()
                .source,
            Source::SpacetimeLogin
        );
    }

    #[test]
    fn a_persisted_token_round_trips_without_its_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let path = store(&tmp);
        persist(&path, "eyJhbGciOiJFUzI1NiJ9.PAYLOAD.SIG").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
        assert_eq!(
            read_store(&path).as_deref(),
            Some("eyJhbGciOiJFUzI1NiJ9.PAYLOAD.SIG")
        );
    }

    #[test]
    fn a_mint_response_without_a_token_is_refused() {
        struct Empty;
        impl HttpClient for Empty {
            fn post_json(&self, _url: &str, _bearer: Option<&str>, _body: &str) -> Result<String> {
                Ok(r#"{"identity":"c200"}"#.to_string())
            }
        }
        let error = mint(&Empty, NODE).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not return an identity token"),
            "{error}"
        );
    }
}
