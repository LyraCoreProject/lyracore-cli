//! `lyracore character gm NAME true|false` — grant or revoke GM level for a character.
//!
//! The server side already exists: `Character.gm_level: u8` and the operator-gated
//! `set_gm_level(character_name, level)` reducer (`module/src/gm.rs`) unlock the full `.commands`
//! dot-command kit at any nonzero level. This is the missing CLI verb — `true` maps to level 3,
//! `false` to level 0.
//!
//! Characters live on WORLD shards only, never realm-core, so this walks
//! [`Topology::world_shards`](crate::project::Topology::world_shards) — the same list
//! `account create`'s topology-awareness (`cmd/account.rs`) is built on — trying each in order
//! until one recognises the name. `set_gm_level` is `require_operator`-gated, so the call goes
//! over the SAME bearer-token HTTP path `dev up`'s `claim_operator` uses, never
//! `spacetime call`: the credential that claimed the operator may not be the `spacetime` CLI's own
//! identity.

use crate::cmd::dev::reducer_url;
use crate::http::HttpClient;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::state::RuntimeState;
use crate::{Error, Result};

pub fn gm(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
    name: &str,
    enabled: bool,
) -> Result<()> {
    let level = if enabled { 3 } else { 0 };
    let credential = crate::token::resolve_existing(runner, &project.token_file())?;
    let shards = RuntimeState::load(&project.state_file())?.topology().world_shards();
    let arguments = format!("[{},{level}]", json_string(name));

    for shard in &shards {
        match http.post_json(
            &reducer_url(shard, "set_gm_level"),
            Some(credential.token()),
            &arguments,
        ) {
            Ok(_) => {
                println!("✓ set gm_level {level} for '{name}' on {shard}");
                return Ok(());
            }
            Err(e) if e.to_string().contains("no player named") => continue,
            Err(e) if e.to_string().contains("operator not claimed") => {
                return Err(Error::Process(format!(
                    "{e}\n  the operator identity is not claimed on {shard} — run `lyracore dev \
                     up` first (it mints and claims one automatically)."
                )));
            }
            Err(e) if e.to_string().contains("operator only") => {
                return Err(Error::Process(format!(
                    "{e}\n  {shard} is claimed by a DIFFERENT identity than the one in {} — \
                     delete that file and re-run `lyracore dev up` to fall back to the identity \
                     that IS the operator there.",
                    project.token_file().display()
                )));
            }
            Err(e) => {
                return Err(Error::Process(format!(
                    "{e}\n  — is the stack up? check `lyracore dev status`"
                )))
            }
        }
    }
    Err(Error::Process(format!(
        "no player named '{name}' on any world shard"
    )))
}

/// A Rust string as a JSON string literal — the same reasoning as `project::json_string`: one
/// argument goes through this, and hand-escaping a character name would be the second place to
/// forget an edge case.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeHttp;
    use crate::proc::fake::FakeStack;
    use crate::project::Topology;
    use crate::state::RuntimeState;
    use tempfile::TempDir;

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    /// A checkout with a persisted coordinator credential, so `resolve_existing` succeeds without
    /// touching the `spacetime` CLI.
    fn with_credential(project: &ProjectLayout) {
        crate::token::resolve_or_mint(
            &FakeStack::new().fail_on("login show", "not logged in").runner(),
            &FakeHttp::new(),
            &project.token_file(),
            "http://127.0.0.1:3000",
        )
        .unwrap();
    }

    fn set_topology(project: &ProjectLayout, topology: Topology) {
        RuntimeState {
            topology: topology.as_str().to_string(),
            ..Default::default()
        }
        .save(&project.state_file())
        .unwrap();
    }

    #[test]
    fn true_maps_to_level_3_and_false_to_level_0() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);

        let http = FakeHttp::new();
        gm(&project, &FakeStack::new().runner(), &http, "Ginger", true).unwrap();
        assert_eq!(http.requests()[0].body, r#"["Ginger",3]"#);

        let http = FakeHttp::new();
        gm(&project, &FakeStack::new().runner(), &http, "Ginger", false).unwrap();
        assert_eq!(http.requests()[0].body, r#"["Ginger",0]"#);
    }

    #[test]
    fn a_name_with_special_characters_is_json_escaped_not_concatenated() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);

        let http = FakeHttp::new();
        gm(
            &project,
            &FakeStack::new().runner(),
            &http,
            "Weird\"Name",
            true,
        )
        .unwrap();
        assert_eq!(http.requests()[0].body, r#"["Weird\"Name",3]"#);
    }

    #[test]
    fn the_shard_walk_follows_world_shards_in_topology_order_and_never_touches_realm_core() {
        for topology in [Topology::Single, Topology::Sharded] {
            let tmp = TempDir::new().unwrap();
            let project = project(&tmp);
            with_credential(&project);
            set_topology(&project, topology);

            let http = FakeHttp::failing("no player named 'Nobody' anywhere");
            let error = gm(&project, &FakeStack::new().runner(), &http, "Nobody", true)
                .unwrap_err()
                .to_string();
            assert!(error.contains("no player named"), "{error}");

            let urls: Vec<String> = http.requests().into_iter().map(|r| r.url).collect();
            let expected: Vec<String> = topology
                .world_shards()
                .iter()
                .map(|s| reducer_url(s, "set_gm_level"))
                .collect();
            assert_eq!(urls, expected, "{topology:?}");
            assert!(
                !urls.iter().any(|u| u.contains(ProjectLayout::REALM_CORE)),
                "{urls:?}"
            );
        }
    }

    #[test]
    fn a_miss_on_the_first_shard_tries_the_next_one() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);
        set_topology(&project, Topology::Sharded);

        // Refuse only the default shard's own URL — `ProjectLayout::DATABASE` ("lyracore") is a
        // PREFIX of every other shard's name ("lyracore-kalimdor", "lyracore-realm"), so the
        // needle has to be the exact path segment or it refuses all of them.
        let needle = format!("/database/{}/call/", ProjectLayout::DATABASE);
        let http = FakeHttp::refusing(&needle, "no player named 'Ginger' here");
        gm(&project, &FakeStack::new().runner(), &http, "Ginger", true).unwrap();

        let urls: Vec<String> = http.requests().into_iter().map(|r| r.url).collect();
        assert_eq!(
            urls,
            vec![
                reducer_url(ProjectLayout::DATABASE, "set_gm_level"),
                reducer_url(ProjectLayout::KALIMDOR_SHARD, "set_gm_level"),
            ]
        );
    }

    #[test]
    fn a_miss_on_every_shard_aggregates_into_one_error() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);
        set_topology(&project, Topology::Sharded);

        let http = FakeHttp::failing("no player named 'Nobody' here");
        let error = gm(&project, &FakeStack::new().runner(), &http, "Nobody", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no player named 'Nobody' on any world shard"), "{error}");
        assert_eq!(http.requests().len(), Topology::Sharded.world_shards().len());
    }

    #[test]
    fn operator_not_claimed_is_distinguished_from_a_miss_and_stops_immediately() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);
        set_topology(&project, Topology::Sharded);

        let http = FakeHttp::failing("operator not claimed");
        let error = gm(&project, &FakeStack::new().runner(), &http, "Ginger", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("dev up"), "{error}");
        // A refusal that is not a miss must stop the walk rather than trying every shard.
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn operator_only_is_distinguished_from_a_miss_and_names_the_remedy() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);
        set_topology(&project, Topology::Sharded);

        let http = FakeHttp::failing("operator only");
        let error = gm(&project, &FakeStack::new().runner(), &http, "Ginger", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("DIFFERENT identity"), "{error}");
        assert!(error.contains("dev up"), "{error}");
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn an_unrecognized_failure_gets_an_actionable_hint_and_stops_immediately() {
        // A connection-refused (or any other error this function does not specifically
        // recognise) must not be swallowed into a bare "no player named" retry loop — the
        // operator needs something to check, and the module's own message is kept.
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);
        set_topology(&project, Topology::Sharded);

        let http = FakeHttp::failing("connection refused");
        let error = gm(&project, &FakeStack::new().runner(), &http, "Ginger", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("connection refused"), "{error}");
        assert!(error.contains("lyracore dev status"), "{error}");
        assert_eq!(http.requests().len(), 1, "must not try every shard");
    }

    #[test]
    fn the_token_reaches_the_request_as_a_bearer_and_never_as_an_argument() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        with_credential(&project);

        let stack = FakeStack::new();
        let http = FakeHttp::new();
        gm(&project, &stack.runner(), &http, "Ginger", true).unwrap();

        let request = &http.requests()[0];
        assert!(request.bearer.is_some());
        assert!(!request.url.contains(request.bearer.as_deref().unwrap()));
        assert!(!request.body.contains(request.bearer.as_deref().unwrap()));
        for rendered in stack.rendered() {
            assert!(
                !rendered.contains(request.bearer.as_deref().unwrap()),
                "leaked into: {rendered}"
            );
        }
    }

    #[test]
    fn no_credential_at_all_refuses_and_makes_no_request() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();

        let error = gm(&project, &stack.runner(), &http, "Ginger", true).unwrap_err();
        assert!(error.to_string().contains("lyracore dev up"), "{error}");
        assert!(http.requests().is_empty());
    }
}
