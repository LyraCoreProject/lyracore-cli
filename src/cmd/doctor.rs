//! `lyracore doctor` — are the prerequisites for `dev up` present?
//!
//! Exits nonzero only for *launch-blocking* failures. A busy port or a missing WASM target is a
//! warning: informative, but not a reason to fail a script that only wanted the report.

use crate::project::ProjectLayout;
use crate::Result;
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
        check_wasm_target(),
        check_ports(),
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
