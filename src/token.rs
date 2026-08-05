//! The coordinator's auth token — the one credential the fixture cannot run without.
//!
//! `game_account` and `game_session` are PRIVATE module tables. The gateway's coordinator
//! connection subscribes to both, and `provision_account` is `require_operator`-gated, so an
//! anonymous connection cannot read an account, cannot answer an SRP6 challenge, and cannot
//! provision anything. What makes the connection privileged is `LYRACORE_COORDINATOR_TOKEN`
//! carrying the identity that published the module and claimed the operator — which, for this
//! fixture, is the `spacetime` CLI's own identity.
//!
//! Without it the gateway does not fail fast: it starts, warns, and then dies ~15s later on
//! "coordinator subscriptions not applied within 15s", which reads like a node problem rather
//! than a credential problem.
//!
//! The token is asked of the CLI itself rather than parsed out of `cli.toml`, because the config
//! file's location is the CLI's business and differs per platform — the production recipe in
//! `docs/danger-zones.md` §3 greps `~/.config/spacetime/cli.toml` with `grep -oP`, which is both
//! Linux-only twice over (the path and the PCRE flag) and exactly what this CLI must not do.

use crate::proc::{CommandSpec, ProcessRunner};
use crate::{Error, Result};

/// The variable the gateway reads it from (`gateway/src/config.rs`).
pub const TOKEN_VAR: &str = "LYRACORE_COORDINATOR_TOKEN";

/// Shortest thing we will believe is a token. Real ones are JWTs, i.e. hundreds of characters;
/// this only has to reject a stray word from a reworded banner.
const MIN_TOKEN_LEN: usize = 16;

pub fn token_command() -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("login")
        .arg("show")
        .arg("--token")
}

/// Ask the `spacetime` CLI for the token of the identity it acts as.
pub fn resolve(runner: &dyn ProcessRunner) -> Result<String> {
    // The child prints the token on STDOUT; `SubprocessFailed` carries only the rendered command
    // and the child's STDERR, so a failure here cannot quote a token — there is none to quote.
    let output = runner.run_and_wait(&token_command()).map_err(|e| {
        Error::PrerequisiteMissing(format!(
            "could not read the SpacetimeDB auth token ({e}).\n  The gateway needs it to read the \
             private game_account/game_session tables and to provision accounts. Run `spacetime \
             login` as the identity that publishes this database, then try again."
        ))
    })?;

    parse(&output).ok_or_else(|| {
        Error::PrerequisiteMissing(
            "`spacetime login show --token` printed no auth token — you are probably not logged \
             in. Run `spacetime login` as the identity that publishes this database (it is the \
             one `dev up` claims as the operator), then try again."
                .to_string(),
        )
    })
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
/// so "not logged in" becomes an actionable error instead of a bogus credential the gateway would
/// spend 15 seconds failing to use.
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
    use crate::proc::fake::FakeStack;

    /// Verbatim from `spacetime login show --token` on 2.7.1.
    const REAL_BANNER: &str = "You are logged in as some-identity\n\
                               Your auth token (don't share this!) is eyJhbGciOiJFUzI1NiJ9.PAYLOAD.SIG\n";

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

    #[test]
    fn resolving_asks_the_cli_once_and_never_renders_what_it_gets_back() {
        let stack = FakeStack::new();
        let token = resolve(&stack.runner()).unwrap();

        assert_eq!(token, crate::proc::fake::FAKE_TOKEN);
        // One read-only query, and the token is not in it — the value arrives on the child's
        // stdout, so nothing loggable ever carried it.
        assert_eq!(
            stack.rendered(),
            vec!["spacetime login show --token".to_string()]
        );
        for rendered in stack.rendered() {
            assert!(!rendered.contains(&token), "leaked into: {rendered}");
        }
    }

    #[test]
    fn a_failing_lookup_is_a_prerequisite_error_that_says_what_to_run() {
        let stack = FakeStack::new().fail_on("login show", "command not found");
        let error = resolve(&stack.runner()).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(error.to_string().contains("spacetime login"), "{error}");
    }
}
