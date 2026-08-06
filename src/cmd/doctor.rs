//! `lyracore doctor` — are the prerequisites for `dev up` present?
//!
//! Exits nonzero only for *launch-blocking* failures. A busy port or a missing WASM target is a
//! warning: informative, but not a reason to fail a script that only wanted the report.

use crate::cmd::import;
use crate::config::Config;
use crate::project::ProjectLayout;
use crate::Result;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The SpacetimeDB version LyraCore is pinned to. `doctor` asks it as a **floor** (`>=`) — it only
/// answers "can you get `dev up` off the ground"; `preflight` is the one that demands an EXACT match,
/// and it reads the pin out of the checkout's own `module/Cargo.toml` rather than from this constant.
/// Keep this in step with that pin (LyraCore's `module/Cargo.toml` + `gateway/Cargo.toml`).
pub const REQUIRED_SPACETIME: Version = Version(2, 7, 1);

#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Pass { label: String, detail: String },
    Warn { label: String, guidance: String },
    Fail { label: String, guidance: String },
}

impl Check {
    fn pass(label: &str, detail: impl Into<String>) -> Self {
        Check::Pass {
            label: label.to_string(),
            detail: detail.into(),
        }
    }
    fn warn(label: &str, guidance: impl Into<String>) -> Self {
        Check::Warn {
            label: label.to_string(),
            guidance: guidance.into(),
        }
    }
    fn fail(label: &str, guidance: impl Into<String>) -> Self {
        Check::Fail {
            label: label.to_string(),
            guidance: guidance.into(),
        }
    }

    pub fn is_blocking(&self) -> bool {
        matches!(self, Check::Fail { .. })
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Pull the first `x.y.z` out of a `--version` banner. Tool banners carry commit hashes and
/// several version numbers, so this takes the first dotted triple and ignores the rest.
pub fn parse_version(text: &str) -> Option<Version> {
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() >= 3 {
            if let (Ok(a), Ok(b), Ok(c)) = (
                parts[0].parse::<u32>(),
                parts[1].parse::<u32>(),
                parts[2].parse::<u32>(),
            ) {
                return Some(Version(a, b, c));
            }
        }
    }
    None
}

fn version_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

pub fn run(layout: &Result<ProjectLayout>) -> Vec<Check> {
    vec![
        check_layout(layout),
        check_rust(layout.as_ref().ok()),
        check_tool(
            "Cargo",
            "cargo",
            "Cargo ships with Rust — see https://rustup.rs/",
        ),
        check_spacetime(),
        check_spacetime_server(),
        check_wasm_target(),
        check_ports(),
        check_client_data(layout.as_ref().ok()),
    ]
}

/// Read the checkout's declared minimum Rust version out of its workspace manifest.
///
/// The requirement is the project's, not this CLI's: hardcoding a number here would be a second
/// place to forget to bump, and it would be wrong for every other checkout.
pub fn required_rust(manifest: &str) -> Option<Version> {
    let line = manifest
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("rust-version"))?;
    let value = line.split('"').nth(1)?;
    let mut parts = value.split('.').map(str::parse::<u32>);
    // `rust-version` is legally two components ("1.93") as well as three.
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => Some(Version(major, minor, patch)),
        (Some(Ok(major)), Some(Ok(minor)), None) => Some(Version(major, minor, 0)),
        _ => None,
    }
}

fn check_rust(project: Option<&ProjectLayout>) -> Check {
    let Some(text) = version_output("rustc", &["--version"]) else {
        return Check::fail(
            "Rust",
            "`rustc` not found — install Rust from https://rustup.rs/",
        );
    };
    let (Some(found), Some(required)) = (
        parse_version(&text),
        project
            .and_then(|p| std::fs::read_to_string(p.root.join("Cargo.toml")).ok())
            .as_deref()
            .and_then(required_rust),
    ) else {
        // Either the banner or the manifest was unreadable. `rustc` exists, which is the
        // launch-blocking half; report what we have rather than inventing a requirement.
        return Check::pass("Rust", text.trim().to_string());
    };

    if found >= required {
        return Check::pass("Rust", format!("{found} (requires {required})"));
    }
    // Genuinely launch-blocking: the workspace will not compile, so `dev up` cannot build the
    // gateway or the module.
    Check::fail(
        "Rust",
        format!(
            "found {found}, but this checkout needs {required} — `rustup update` (the pinned \
             toolchain in rust-toolchain.toml is installed automatically by any cargo command \
             run inside the checkout, so this usually means rustup is not managing this rustc)"
        ),
    )
}

fn check_layout(layout: &Result<ProjectLayout>) -> Check {
    match layout {
        Ok(project) => Check::pass("project layout", project.root.display().to_string()),
        Err(e) => Check::fail("project layout", e.to_string()),
    }
}

fn check_tool(label: &str, program: &str, guidance: &str) -> Check {
    match version_output(program, &["--version"]) {
        Some(text) => Check::pass(label, text.trim().to_string()),
        None => Check::fail(label, format!("`{program}` not found — {guidance}")),
    }
}

fn check_spacetime() -> Check {
    let Some(text) = version_output("spacetime", &["--version"]) else {
        return Check::fail(
            "SpacetimeDB",
            format!(
                "`spacetime` not found — install {REQUIRED_SPACETIME} from https://spacetimedb.com/install"
            ),
        );
    };
    match parse_version(&text) {
        Some(found) if found >= REQUIRED_SPACETIME => {
            Check::pass("SpacetimeDB", format!("{found} (requires {REQUIRED_SPACETIME})"))
        }
        Some(found) => Check::fail(
            "SpacetimeDB",
            format!(
                "found {found}, but {REQUIRED_SPACETIME} is required — upgrade with \
                 `spacetime version upgrade` (or reinstall from https://spacetimedb.com/install)"
            ),
        ),
        None => Check::warn(
            "SpacetimeDB",
            format!("could not read a version from `spacetime --version`; expected {REQUIRED_SPACETIME}"),
        ),
    }
}

/// The spacetime CLI's own configuration file.
///
/// Per-USER and shared by every SpacetimeDB project on the machine — there is no per-checkout
/// override, and `spacetime sql` takes no environment variable for the server, only `-s`. That is
/// why a setting made for someone else's project reaches into this one.
fn spacetime_cli_config() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".config/spacetime/cli.toml"))
}

/// The `(nickname, host)` that `default_server` resolves to in a spacetime `cli.toml`.
///
/// A line scanner, not a TOML parser: this crate carries no TOML dependency, and the three keys
/// needed (`default_server`, plus the `nickname`/`host` pair inside each `[[server_configs]]`) are
/// written one per line by the CLI that owns the file. The `ecdsa_public_key` heredoc rides
/// through harmlessly — no base64 line starts with `host` or `nickname`.
///
/// Ceiling: an inline-table spelling of `server_configs` reads as "nothing resolved", which the
/// caller reports as unknown rather than as a mismatch. This never invents a verdict out of a
/// parse it did not manage — a doctor check that cries wolf on a file it misread is worse than one
/// that stays quiet.
pub fn default_server_host(toml: &str) -> Option<(String, String)> {
    let mut default: Option<String> = None;
    let mut servers: Vec<(String, String)> = Vec::new();
    let (mut in_server, mut nickname, mut host) = (false, None, None);

    let mut flush = |nickname: &mut Option<String>, host: &mut Option<String>| {
        if let (Some(n), Some(h)) = (nickname.take(), host.take()) {
            servers.push((n, h));
        }
    };
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut nickname, &mut host);
            in_server = line.starts_with("[[server_configs]]");
            continue;
        }
        if let Some(value) = quoted_value(line, "default_server") {
            default = Some(value);
        }
        if in_server {
            if let Some(value) = quoted_value(line, "nickname") {
                nickname = Some(value);
            }
            if let Some(value) = quoted_value(line, "host") {
                host = Some(value);
            }
        }
    }
    flush(&mut nickname, &mut host);

    let default = default?;
    servers.into_iter().find(|(n, _)| *n == default)
}

/// `key = "value"` on one line, or `None`. The `=` is required immediately after the key (modulo
/// whitespace) so `host` does not also match a `hostname` key.
fn quoted_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// Does this `host` name the loopback node `dev up` runs? Accepts the spellings the spacetime CLI
/// stores (bare `host:port`) as well as a scheme-qualified one, since users hand-edit this file.
fn targets_local_node(host: &str) -> bool {
    let host = host.trim().trim_end_matches('/');
    let host = host
        .strip_prefix("http://")
        .or_else(|| host.strip_prefix("https://"))
        .unwrap_or(host);
    let Some(name) = host.strip_suffix(&format!(":{}", ProjectLayout::STDB_PORT)) else {
        return false;
    };
    matches!(name, "127.0.0.1" | "localhost" | "0.0.0.0" | "::1" | "[::1]")
}

/// Does the spacetime CLI's `default_server` point at the node this checkout uses?
///
/// Never launch-blocking, and deliberately so: `dev up`, `publish` and the importer binary all pass
/// `-s local` explicitly, so they are immune to whatever the default is. The one thing that is not
/// is `import-world.sh`'s post-run verification queries, which call bare `spacetime sql`. Pointed at
/// a different node those cannot read a single row, and the import ends in a wall of FAIL lines for
/// content that landed perfectly well — worse, the low-floor assertions PASS on the connection
/// error's own output, so the run reads as a partial regression rather than as a broken connection.
fn check_spacetime_server() -> Check {
    const LABEL: &str = "default server";
    let Some(path) = spacetime_cli_config() else {
        return Check::pass(LABEL, "no HOME set — spacetime's config was not looked for");
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // A fresh machine has not written one yet; spacetime's built-in default is used. Nothing
        // to warn about, and nothing read, so say only what is true.
        return Check::pass(LABEL, format!("{} not written yet", path.display()));
    };
    let Some((nickname, host)) = default_server_host(&text) else {
        return Check::pass(
            LABEL,
            format!("no default_server resolved in {}", path.display()),
        );
    };
    if targets_local_node(&host) {
        return Check::pass(LABEL, format!("{nickname} → {host}"));
    }
    misdirected_default(&nickname, &host)
}

/// The warning for a `default_server` pointing somewhere other than this checkout's node. Split out
/// so a test can read the text without depending on the developer's own `cli.toml`.
fn misdirected_default(nickname: &str, host: &str) -> Check {
    Check::warn(
        "default server",
        format!(
            "spacetime's default server is `{nickname}` ({host}), not {listen} — `dev up` and \
             `publish` are unaffected (they pass `-s {alias}`), but `lyracore import`'s \
             verification queries call bare `spacetime sql` and will read the wrong node, ending \
             the run in FAILs for content that imported fine. Fix with `spacetime server \
             set-default {alias}` — but note this file is per-user and shared with your other \
             SpacetimeDB projects.",
            listen = ProjectLayout::stdb_listen(),
            alias = ProjectLayout::STDB_SERVER,
        ),
    )
}

fn check_wasm_target() -> Check {
    match version_output("rustup", &["target", "list", "--installed"]) {
        Some(text) if text.contains("wasm32-unknown-unknown") => {
            Check::pass("WASM target", "wasm32-unknown-unknown installed")
        }
        // A warning, not a failure: `publish-module.sh` builds the module and will report the
        // missing target itself, and a rustup-less toolchain may still have it.
        Some(_) => Check::warn(
            "WASM target",
            "wasm32-unknown-unknown is missing — run `rustup target add wasm32-unknown-unknown`",
        ),
        None => Check::warn(
            "WASM target",
            "could not query rustup; ensure wasm32-unknown-unknown is installed",
        ),
    }
}

fn check_ports() -> Check {
    let busy: Vec<u16> = [
        ProjectLayout::STDB_PORT,
        ProjectLayout::LOGON_PORT,
        ProjectLayout::WORLD_PORT,
    ]
    .into_iter()
    .filter(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_err())
    .collect();

    if busy.is_empty() {
        return Check::pass(
            "ports",
            format!(
                "{}, {}, {} free",
                ProjectLayout::STDB_PORT,
                ProjectLayout::LOGON_PORT,
                ProjectLayout::WORLD_PORT
            ),
        );
    }
    // Busy ports are frequently our own running stack, so this can never be blocking.
    Check::warn(
        "ports",
        format!(
            "{} already in use — `lyracore dev status` will say whether that is your own stack",
            busy.iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

/// Is a 1.12.1 client mapped, and does it still look right? Never launch-blocking: a client is
/// needed only for `lyracore import`, and `dev up`'s seed fixture needs none at all.
fn check_client_data(project: Option<&ProjectLayout>) -> Check {
    let Some(project) = project else {
        return Check::warn(
            "client data",
            "not set — only needed for `lyracore import`; set with `lyracore config set \
             client-data <path>`",
        );
    };
    let config = match Config::load(&project.config_file()) {
        Ok(config) => config,
        Err(e) => {
            return Check::warn(
                "client data",
                format!(
                    "{e} — re-set with `lyracore config set client-data <path>`"
                ),
            )
        }
    };
    let Some(raw) = config.client_data else {
        return Check::warn(
            "client data",
            "not set — only needed for `lyracore import`; set with `lyracore config set \
             client-data <path>`",
        );
    };
    match import::inspect_client_data(Path::new(&raw)) {
        Ok(_notes) => Check::pass("client data", raw),
        Err(e) => Check::warn(
            "client data",
            format!("{e} — re-set with `lyracore config set client-data <path>`"),
        ),
    }
}

pub fn report(checks: &[Check]) -> bool {
    for check in checks {
        match check {
            Check::Pass { label, detail } => println!("  ✓ {label:<16} {detail}"),
            Check::Warn { label, guidance } => println!("  ⚠ {label:<16} {guidance}"),
            Check::Fail { label, guidance } => println!("  ✗ {label:<16} {guidance}"),
        }
    }
    let blocking = checks.iter().filter(|c| c.is_blocking()).count();
    println!();
    if blocking == 0 {
        println!("doctor: ready for `lyracore dev up`.");
    } else {
        println!("doctor: {blocking} blocking problem(s) — `lyracore dev up` will not work yet.");
    }
    blocking > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    #[test]
    fn versions_are_read_out_of_real_banners() {
        assert_eq!(
            parse_version("spacetimedb tool version 2.7.1; spacetime-lib version 2.7.1"),
            Some(Version(2, 7, 1))
        );
        assert_eq!(
            parse_version("rustc 1.80.1 (abc 2024-08-08)"),
            Some(Version(1, 80, 1))
        );
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn the_required_spacetimedb_version_is_enforced_in_the_right_direction() {
        assert!(Version(2, 7, 1) >= REQUIRED_SPACETIME);
        assert!(Version(2, 7, 2) >= REQUIRED_SPACETIME);
        assert!(Version(3, 0, 0) >= REQUIRED_SPACETIME);
        assert!(Version(2, 7, 0) < REQUIRED_SPACETIME);
        assert!(Version(2, 5, 0) < REQUIRED_SPACETIME);
        assert!(Version(1, 9, 9) < REQUIRED_SPACETIME);
    }

    #[test]
    fn the_rust_requirement_is_read_from_the_checkout_not_hardcoded() {
        assert_eq!(
            required_rust("[workspace.package]\nrust-version = \"1.93.0\"\n"),
            Some(Version(1, 93, 0))
        );
        // Two-component form, and a manifest that simply does not declare one.
        assert_eq!(
            required_rust("rust-version = \"1.85\"\n"),
            Some(Version(1, 85, 0))
        );
        assert_eq!(required_rust("[package]\nname = \"x\"\n"), None);
        assert_eq!(required_rust("rust-version = \"stable\"\n"), None);
    }

    #[test]
    fn a_rustc_older_than_the_checkout_requires_is_launch_blocking() {
        // The comparison itself, without depending on which rustc happens to be running the test.
        assert!(Version(1, 92, 0) < Version(1, 93, 0));
        assert!(Version(1, 93, 0) >= Version(1, 93, 0));
        // And the real check never blocks on a checkout it could not read a requirement from.
        assert!(!check_rust(None).is_blocking(), "rustc is present in CI");
    }

    #[test]
    fn only_hard_failures_block_a_launch() {
        assert!(Check::fail("x", "y").is_blocking());
        assert!(!Check::warn("x", "y").is_blocking());
        assert!(!Check::pass("x", "y").is_blocking());
    }

    #[test]
    fn a_busy_port_is_a_warning_not_a_failure() {
        // Binding our own stack's ports must never make `doctor` exit nonzero.
        assert!(!check_ports().is_blocking());
    }

    #[test]
    fn a_broken_layout_is_the_blocking_failure() {
        let broken = Err(crate::Error::ProjectLayout("not a checkout".to_string()));
        assert!(check_layout(&broken).is_blocking());
    }

    // ---- the spacetime default-server check ----

    /// The real shape the spacetime CLI writes, heredoc and all — the parser has to survive it.
    const REAL_CLI_TOML: &str = r#"
default_server = "self-hosted"
spacetimedb_token = "ey.redacted"

[[server_configs]]
nickname = "maincloud"
host = "maincloud.spacetimedb.com"
protocol = "https"

[[server_configs]]
nickname = "local"
host = "127.0.0.1:3000"
protocol = "http"

[[server_configs]]
nickname = "self-hosted"
host = "127.0.0.1:3001"
protocol = "http"
ecdsa_public_key = """
-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEm+jebVpyB5H2HKjZWSZjnZYips6I
SymPuOuWJw2rsEea8mztm8/KOQEzzCl/h3Ed8WuUZ7kTmr3mkYHlFgeTPg==
-----END PUBLIC KEY-----
"""
"#;

    #[test]
    fn the_default_server_is_resolved_through_its_nickname_to_a_host() {
        assert_eq!(
            default_server_host(REAL_CLI_TOML),
            Some(("self-hosted".to_string(), "127.0.0.1:3001".to_string()))
        );
    }

    #[test]
    fn the_last_server_block_in_the_file_is_not_dropped() {
        // `self-hosted` above is the final block AND the one that matters — a flush that only ran
        // on the next `[` would resolve nothing and the check would go quiet exactly when it is
        // needed. Pinned separately from the happy path so a refactor cannot lose only this.
        let trailing = "default_server = \"only\"\n[[server_configs]]\nnickname = \"only\"\nhost = \"127.0.0.1:3000\"\n";
        assert_eq!(
            default_server_host(trailing),
            Some(("only".to_string(), "127.0.0.1:3000".to_string()))
        );
    }

    #[test]
    fn a_default_naming_no_configured_server_resolves_to_nothing() {
        let orphan = "default_server = \"ghost\"\n[[server_configs]]\nnickname = \"local\"\nhost = \"127.0.0.1:3000\"\n";
        assert_eq!(default_server_host(orphan), None);
        // And a file with no default at all.
        assert_eq!(
            default_server_host("[[server_configs]]\nnickname = \"local\"\nhost = \"x:3000\"\n"),
            None
        );
    }

    #[test]
    fn a_key_that_merely_starts_with_host_is_not_read_as_host() {
        assert_eq!(quoted_value("hostname = \"nope\"", "host"), None);
        assert_eq!(
            quoted_value("host = \"127.0.0.1:3000\"", "host"),
            Some("127.0.0.1:3000".to_string())
        );
    }

    #[test]
    fn only_a_host_on_the_projects_own_port_counts_as_local() {
        for good in [
            "127.0.0.1:3000",
            "localhost:3000",
            "0.0.0.0:3000",
            "http://127.0.0.1:3000",
            "http://127.0.0.1:3000/",
        ] {
            assert!(targets_local_node(good), "{good} should count as local");
        }
        // 3001 is the case this whole check exists for: same loopback host, wrong node.
        for bad in [
            "127.0.0.1:3001",
            "maincloud.spacetimedb.com",
            "192.168.1.50:3000",
            "127.0.0.1",
        ] {
            assert!(!targets_local_node(bad), "{bad} should not count as local");
        }
    }

    #[test]
    fn a_misdirected_default_server_warns_and_never_blocks_a_launch() {
        // The real check reads the developer's own config, so assert the property that must hold
        // for every machine: this can inform, but it must never fail `dev up`.
        assert!(!check_spacetime_server().is_blocking());
    }

    #[test]
    fn the_warning_names_the_offender_the_fix_and_what_it_actually_breaks() {
        let (nickname, host) = default_server_host(REAL_CLI_TOML).expect("fixture must resolve");
        assert!(!targets_local_node(&host), "the fixture is the bad case");

        let Check::Warn { guidance, .. } = misdirected_default(&nickname, &host) else {
            panic!("a misdirected default must warn");
        };
        // What is wrong, where it points, and the exact command that repairs it.
        assert!(guidance.contains("self-hosted"), "{guidance}");
        assert!(guidance.contains("127.0.0.1:3001"), "{guidance}");
        assert!(
            guidance.contains("spacetime server set-default local"),
            "{guidance}"
        );
        // The two things a reader most needs: which command breaks, and that the fix is global.
        assert!(guidance.contains("lyracore import"), "{guidance}");
        assert!(guidance.contains("per-user"), "{guidance}");
    }

    // ---- the client-data check ----

    #[test]
    fn client_data_unset_warns_with_the_set_hint_and_never_blocks() {
        let tmp = TempDir::new().unwrap();
        let project = workspace(&tmp);
        let check = check_client_data(Some(&project));
        assert!(!check.is_blocking());
        match check {
            Check::Warn { guidance, .. } => assert!(
                guidance.contains("lyracore config set client-data"),
                "{guidance}"
            ),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn client_data_missing_project_layout_also_warns_unset_rather_than_panicking() {
        assert!(!check_client_data(None).is_blocking());
    }

    #[test]
    fn client_data_invalid_warns_with_the_specific_problem_and_the_reset_hint() {
        let tmp = TempDir::new().unwrap();
        let project = workspace(&tmp);
        crate::config::Config {
            client_data: Some(tmp.path().join("nope").to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();

        let check = check_client_data(Some(&project));
        assert!(!check.is_blocking());
        match check {
            Check::Warn { guidance, .. } => {
                assert!(guidance.contains("no such directory"), "{guidance}");
                assert!(
                    guidance.contains("lyracore config set client-data"),
                    "{guidance}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn client_data_valid_passes_and_shows_the_path() {
        let tmp = TempDir::new().unwrap();
        let project = workspace(&tmp);
        let data = tmp.path().join("wow/Data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["dbc.MPQ", "terrain.MPQ"] {
            std::fs::write(data.join(name), "").unwrap();
        }
        crate::config::Config {
            client_data: Some(data.to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();

        match check_client_data(Some(&project)) {
            Check::Pass { detail, .. } => {
                assert!(detail.contains("wow/Data"), "{detail}");
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn every_failure_carries_guidance() {
        let layout = ProjectLayout::discover();
        for check in run(&layout) {
            if let Check::Fail { label, guidance } | Check::Warn { label, guidance } = check {
                assert!(
                    !guidance.trim().is_empty(),
                    "{label} reported a problem with no guidance"
                );
            }
        }
    }
}
