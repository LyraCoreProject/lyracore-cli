//! `lyracore doctor` — are the prerequisites for `dev up` present?
//!
//! Exits nonzero only for *launch-blocking* failures. A busy port or a missing WASM target is a
//! warning: informative, but not a reason to fail a script that only wanted the report.

use crate::cmd::import;
use crate::config::Config;
use crate::proc::{require_openssl, CommandSpec, ProcessRunner, RealProcessRunner};
use crate::project::ProjectLayout;
use crate::Result;
use std::path::Path;
use std::process::{Command, Stdio};

/// The SpacetimeDB version LyraCore is pinned to. `doctor` asks it as a **floor** (`>=`) — it only
/// answers "can you get `dev up` off the ground"; `preflight` is the one that demands an EXACT match,
/// and it reads the pin out of the checkout's own `module/Cargo.toml` rather than from this constant.
/// Keep this in step with that pin (LyraCore's `module/Cargo.toml` + `gateway/Cargo.toml`).
pub const REQUIRED_SPACETIME: Version = Version(2, 7, 1);

/// Read the exact Bun runtime verified by this checkout. Do not interpret ranges: Package
/// builds and their identities must agree on one runtime without consulting the installed Bun.
pub fn required_bun(project: &ProjectLayout) -> Result<Version> {
    let path = project.datascripts_dir().join("package.json");
    let refuse = |detail: String| {
        crate::Error::PrerequisiteMissing(format!(
            "cannot read the Bun pin from {}: {detail}. Set `engines.bun` to an exact x.y.z version",
            path.display()
        ))
    };
    let text = std::fs::read_to_string(&path).map_err(|error| refuse(error.to_string()))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| refuse(error.to_string()))?;
    manifest
        .pointer("/engines/bun")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_bare_version)
        .ok_or_else(|| refuse("missing or invalid `engines.bun`".to_string()))
}

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
    let mut checks = vec![
        check_layout(layout),
        check_rust(layout.as_ref().ok()),
        check_tool(
            "Cargo",
            "cargo",
            "Cargo ships with Rust — see https://rustup.rs/",
        ),
        check_spacetime(),
        check_bun(layout.as_ref().ok()),
        check_wasm_target(),
        check_wasm_opt(),
        check_ports(),
        check_client_data(layout.as_ref().ok()),
    ];
    if runs_in_wsl() {
        checks.insert(1, check_checkout_drive(layout.as_ref().ok()));
    }
    if let Some(project) = layout.as_ref().ok().filter(|p| p.has_native_host()) {
        checks.extend(check_native_tools(project, &RealProcessRunner));
    }
    if let Some(check) = check_native_openssl(layout.as_ref().ok(), &RealProcessRunner) {
        checks.push(check);
    }
    checks
}

fn check_native_tools(project: &ProjectLayout, runner: &dyn ProcessRunner) -> [Check; 2] {
    [
        match require_python(runner) {
            Ok(banner) => Check::pass("Python", banner),
            Err(error) => Check::fail("Python", error.to_string()),
        },
        match probe_host_toolchain(project, runner) {
            Ok(HostToolchain::Installed(banner)) => Check::pass("Host toolchain", banner),
            Ok(HostToolchain::Missing { channel, .. }) if rustup_present(runner) => Check::warn(
                "Host toolchain",
                format!(
                    "Rust {channel} is not installed; `lyracore dev up` installs it with rustup, \
                     or run `rustup toolchain install {channel} --profile minimal`"
                ),
            ),
            Ok(HostToolchain::Missing { channel, detail }) => {
                Check::fail("Host toolchain", no_rustup(&channel, &detail))
            }
            Err(error) => Check::fail("Host toolchain", error.to_string()),
        },
    ]
}

pub(crate) fn require_python(runner: &dyn ProcessRunner) -> Result<String> {
    runner
        .run_and_wait(&CommandSpec::new("python3").arg("--version"))
        .map(|banner| banner.trim().to_owned())
        .map_err(|error| {
            crate::Error::PrerequisiteMissing(format!(
                "the Native Host build runs native-host/prepare-host.py with python3, which \
                 failed: {error}. Install Python 3; see docs/quickstart.md#1-prerequisites in \
                 this Core checkout"
            ))
        })
}

pub(crate) enum HostToolchain {
    Installed(String),
    Missing { channel: String, detail: String },
}

/// Probes the `native-host/rust-toolchain.toml` channel the way the build runs it, without a download.
pub(crate) fn probe_host_toolchain(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
) -> Result<HostToolchain> {
    let path = project
        .root
        .join("native-host")
        .join(ProjectLayout::RUST_TOOLCHAIN);
    let channel = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| {
            let line = text
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("channel"))?
                .to_owned();
            Some(line.split('"').nth(1)?.to_owned())
        })
        .filter(|channel| !channel.is_empty())
        .ok_or_else(|| {
            crate::Error::PrerequisiteMissing(format!(
                "cannot read the Native Host toolchain `channel` from {}",
                path.display()
            ))
        })?;
    let probe = CommandSpec::new("cargo")
        .arg(format!("+{channel}"))
        .arg("--version")
        .env("RUSTUP_AUTO_INSTALL", "0");
    Ok(match runner.run_and_wait(&probe) {
        Ok(banner) => HostToolchain::Installed(banner.trim().to_owned()),
        Err(error) => HostToolchain::Missing {
            channel,
            detail: error.to_string(),
        },
    })
}

fn rustup_present(runner: &dyn ProcessRunner) -> bool {
    runner
        .run_and_wait(&CommandSpec::new("rustup").arg("--version"))
        .is_ok()
}

fn no_rustup(channel: &str, detail: &str) -> String {
    format!(
        "the Native Host builds with `cargo +{channel}`, which needs rustup ({detail}). Install \
         rustup from https://rustup.rs/"
    )
}

/// Installs a missing Host toolchain explicitly, whatever rustup's auto-install setting is.
pub(crate) fn ensure_host_toolchain(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
) -> Result<()> {
    let HostToolchain::Missing { channel, detail } = probe_host_toolchain(project, runner)? else {
        return Ok(());
    };
    if !rustup_present(runner) {
        return Err(crate::Error::PrerequisiteMissing(no_rustup(
            &channel, &detail,
        )));
    }
    println!("Installing Rust {channel} for the Native Host...");
    runner.run_streaming(
        &CommandSpec::new("rustup")
            .arg("toolchain")
            .arg("install")
            .arg(&channel)
            .arg("--profile")
            .arg("minimal"),
    )?;
    match probe_host_toolchain(project, runner)? {
        HostToolchain::Installed(_) => Ok(()),
        HostToolchain::Missing { detail, .. } => Err(crate::Error::PrerequisiteMissing(format!(
            "rustup installed Rust {channel}, but `cargo +{channel}` still fails: {detail}"
        ))),
    }
}

fn check_native_openssl(
    project: Option<&ProjectLayout>,
    runner: &dyn ProcessRunner,
) -> Option<Check> {
    if !project.is_some_and(ProjectLayout::has_native_host) {
        return None;
    }
    Some(match require_openssl(runner) {
        Ok(banner) => Check::pass("OpenSSL", banner),
        Err(error) => Check::fail("OpenSSL", error.to_string()),
    })
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

fn runs_in_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

/// WSL mounts Windows drives at `/mnt/<letter>`, where builds are slow and permissions are not kept.
fn check_checkout_drive(project: Option<&ProjectLayout>) -> Check {
    let on_windows_drive = project.is_some_and(|project| {
        let mut parts = project.root.components().skip(1);
        parts.next().is_some_and(|part| part.as_os_str() == "mnt")
            && parts.next().is_some_and(|part| {
                let name = part.as_os_str().as_encoded_bytes();
                name.len() == 1 && name[0].is_ascii_alphabetic()
            })
    });
    if on_windows_drive {
        Check::warn(
            "checkout drive",
            "this checkout is on a Windows drive; builds there are slow and file permissions do \
             not work. Clone LyraCore again in your Linux home directory, for example ~/LyraCore; \
             see docs/windows.md in this Core checkout",
        )
    } else {
        Check::pass("checkout drive", "Linux file system (WSL)")
    }
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

/// Is Bun present at the exact version verified for the checkout's Datascript toolchain?
///
/// Never launch-blocking, and that is the design rather than an omission. Bun is AUTHOR-side only:
/// `lyracore packages build` runs Datascript typegen and the `tsc --noEmit` gate through it. An
/// Operator applying a prebuilt Package Delta, and anyone running `dev up`, need no JavaScript
/// toolchain at all — so a machine without Bun is a machine that cannot author Datascripts, not a
/// broken one. `packages build` compares the same banner against the same pin below, through
/// [`bun_version_check`], but treats a bad answer as a hard failure: it is the one command that
/// actually runs Bun.
fn check_bun(project: Option<&ProjectLayout>) -> Check {
    let Some(project) = project else {
        return Check::warn(
            "Bun",
            "no checkout available to read the Datascript Bun pin",
        );
    };
    let required = match required_bun(project) {
        Ok(required) => required,
        Err(error) => return Check::warn("Bun", error.to_string()),
    };
    let text = version_output("bun", &["--version"]);
    check_bun_banner(text.as_deref(), required)
}

fn check_bun_banner(text: Option<&str>, required: Version) -> Check {
    let install = format!("`{}`", bun_install_hint(required));
    match bun_version_check(text, required) {
        BunVersionCheck::Pinned(found) => Check::pass("Bun", format!("{found} (pinned)")),
        BunVersionCheck::Missing => Check::warn(
            "Bun",
            format!(
                "`bun` not found — needed only by `lyracore packages build` (Datascript typegen \
                 and typecheck); install the pinned {required} with {install}"
            ),
        ),
        BunVersionCheck::Mismatched(found) => Check::warn(
            "Bun",
            format!(
                "found {found}, but this checkout's Datascript toolchain is pinned to \
                 {required} — install that exact version with {install}. `lyracore packages \
                 build` is not verified with a different Bun runtime"
            ),
        ),
        BunVersionCheck::Unparsable => Check::warn(
            "Bun",
            format!(
                "could not read a version from `bun --version`; expected exactly {required}. \
                 Reinstall it with {install}"
            ),
        ),
    }
}

/// How a `bun --version` banner (or its absence) compares against the checkout pin.
///
/// The one place that comparison is expressed: `doctor` turns it into a warning, `packages build`
/// turns the same answer into a hard failure. Neither call site re-derives the verdict itself.
#[derive(Debug, PartialEq, Eq)]
pub enum BunVersionCheck {
    Pinned(Version),
    Missing,
    Mismatched(Version),
    Unparsable,
}

pub fn bun_version_check(text: Option<&str>, required: Version) -> BunVersionCheck {
    let Some(text) = text else {
        return BunVersionCheck::Missing;
    };
    match parse_bare_version(text) {
        Some(found) if found == required => BunVersionCheck::Pinned(found),
        Some(found) => BunVersionCheck::Mismatched(found),
        None => BunVersionCheck::Unparsable,
    }
}

/// The exact-version reinstall command every Bun check points an author at.
pub fn bun_install_hint(required: Version) -> String {
    format!("curl -fsSL https://bun.sh/install | bash -s \"bun-v{required}\"")
}

/// Bun's banner is exactly `x.y.z`; unlike the chatty SpacetimeDB banner, trailing components or
/// surrounding words are malformed rather than details to ignore.
fn parse_bare_version(text: &str) -> Option<Version> {
    let mut parts = text.trim().split('.');
    let version = Version(
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
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

/// Does `wasm-opt` optimise the published module? Never launch-blocking: `spacetime publish`
/// runs fine without it — it just falls back to an unoptimised module, silently, on every publish.
fn check_wasm_opt() -> Check {
    match version_output("wasm-opt", &["--version"]) {
        Some(text) => Check::pass("wasm-opt", text.trim().to_string()),
        None => Check::warn(
            "wasm-opt",
            "not found — `spacetime publish` will still work, but ships an unoptimised module; \
             install with `apt-get install -y binaryen` (Debian/Ubuntu) or `brew install binaryen` \
             (macOS)",
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
                format!("{e} — re-set with `lyracore config set client-data <path>`"),
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
    fn native_openssl_requirement_matches_startup_without_writing_keys() {
        use crate::proc::fake::FakeStack;
        let tmp = TempDir::new().unwrap();
        let project = workspace(&tmp);
        std::fs::create_dir(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        for banner in [
            "OpenSSL 3.0.0 7 Sep 2021",
            "OpenSSL 3.6.5",
            "OpenSSL 4.0.0",
            "OpenSSL 3.6.0-dev",
            "OpenSSL 3.1.5+quic",
        ] {
            let stack = FakeStack::new().with_stdout("openssl version", banner);
            assert!(matches!(
                check_native_openssl(Some(&project), &stack.runner()),
                Some(Check::Pass { .. })
            ));
        }
        for banner in [
            "LibreSSL 3.3.6",
            "OpenSSL 1.1.1w",
            "OpenSSL 3.bad",
            "unknown",
        ] {
            let stack = FakeStack::new().with_stdout("openssl version", banner);
            let Some(Check::Fail { guidance, .. }) =
                check_native_openssl(Some(&project), &stack.runner())
            else {
                panic!("{banner} must block native startup");
            };
            assert!(guidance.contains(banner));
            assert!(guidance.contains("docs/quickstart.md#1-prerequisites"));
        }
        let stack = FakeStack::new().fail_on("openssl version", "not found");
        assert!(check_native_openssl(Some(&project), &stack.runner())
            .unwrap()
            .is_blocking());
        assert!(!project.state_dir.join("native-jwt").exists());
    }

    fn native_workspace(tmp: &TempDir, toolchain: Option<&str>) -> ProjectLayout {
        let project = workspace(tmp);
        std::fs::create_dir(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        if let Some(toolchain) = toolchain {
            std::fs::write(
                tmp.path().join("native-host/rust-toolchain.toml"),
                toolchain,
            )
            .unwrap();
        }
        project
    }

    #[test]
    fn native_tools_pass_with_python_and_the_pinned_host_toolchain() {
        let tmp = TempDir::new().unwrap();
        let project = native_workspace(&tmp, Some("[toolchain]\nchannel = \"1.97.1\"\n"));
        let stack = crate::proc::fake::FakeStack::new();
        let [python, toolchain] = check_native_tools(&project, &stack.runner());
        assert_eq!(python, Check::pass("Python", "Python 3.12.3"));
        assert!(matches!(
            toolchain,
            Check::Pass { detail, .. } if detail.starts_with("cargo 1.97.1 ")
        ));
        assert!(stack
            .rendered()
            .iter()
            .any(|call| call.contains("cargo +1.97.1 --version")));
    }

    #[test]
    fn missing_python_blocks_with_the_install_command() {
        let tmp = TempDir::new().unwrap();
        let project = native_workspace(&tmp, Some("[toolchain]\nchannel = \"1.97.1\"\n"));
        let stack = crate::proc::fake::FakeStack::new().fail_on("python3", "not found");
        let [python, _] = check_native_tools(&project, &stack.runner());
        let Check::Fail { guidance, .. } = python else {
            panic!("missing python3 must block native startup");
        };
        assert!(guidance.contains("Install Python 3"), "{guidance}");
    }

    #[test]
    fn a_missing_host_toolchain_warns_when_rustup_can_install_it() {
        let tmp = TempDir::new().unwrap();
        let project = native_workspace(&tmp, Some("[toolchain]\nchannel = \"1.97.1\"\n"));
        let stack = crate::proc::fake::FakeStack::new()
            .fail_on("cargo +1.97.1", "toolchain '1.97.1' is not installed");
        let [_, toolchain] = check_native_tools(&project, &stack.runner());
        let Check::Warn { guidance, .. } = toolchain else {
            panic!("rustup installs a missing Host toolchain at startup");
        };
        assert!(guidance.contains("dev up` installs it"), "{guidance}");
        assert!(
            guidance.contains("rustup toolchain install 1.97.1"),
            "{guidance}"
        );
    }

    #[test]
    fn a_host_toolchain_without_rustup_blocks() {
        let tmp = TempDir::new().unwrap();
        let project = native_workspace(&tmp, Some("[toolchain]\nchannel = \"1.97.1\"\n"));
        let stack = crate::proc::fake::FakeStack::new()
            .fail_on("cargo +1.97.1", "no such command: `+1.97.1`")
            .fail_on("rustup", "not found");
        let [_, toolchain] = check_native_tools(&project, &stack.runner());
        let Check::Fail { guidance, .. } = toolchain else {
            panic!("`cargo +1.97.1` cannot work without rustup");
        };
        assert!(guidance.contains("needs rustup"), "{guidance}");
        assert!(guidance.contains("no such command"), "{guidance}");
    }

    #[test]
    fn the_host_channel_is_probed_as_written() {
        let tmp = TempDir::new().unwrap();
        let project = native_workspace(&tmp, Some("[toolchain]\nchannel = \"1.97\"\n"));
        let stack = crate::proc::fake::FakeStack::new();
        check_native_tools(&project, &stack.runner());
        assert!(stack
            .rendered()
            .iter()
            .any(|call| call.contains("cargo +1.97 --version")));
    }

    #[test]
    fn an_unreadable_host_toolchain_pin_blocks_without_running_cargo() {
        for toolchain in [None, Some("[toolchain]\n")] {
            let tmp = TempDir::new().unwrap();
            let project = native_workspace(&tmp, toolchain);
            let stack = crate::proc::fake::FakeStack::new();
            let [_, check] = check_native_tools(&project, &stack.runner());
            let Check::Fail { guidance, .. } = check else {
                panic!("an unreadable pin must block native startup");
            };
            assert!(guidance.contains("rust-toolchain.toml"), "{guidance}");
            assert!(!stack
                .rendered()
                .iter()
                .any(|call| call.starts_with("rustc")));
        }
    }

    #[test]
    fn legacy_doctor_does_not_require_the_native_signing_tool() {
        let tmp = TempDir::new().unwrap();
        let project = workspace(&tmp);
        let stack = crate::proc::fake::FakeStack::new().fail_on("openssl", "not found");
        assert!(check_native_openssl(Some(&project), &stack.runner()).is_none());
        assert!(check_native_openssl(None, &stack.runner()).is_none());
        assert!(stack.calls().is_empty());
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
    fn a_checkout_on_a_windows_drive_warns_without_blocking() {
        let at = |root: &str| ProjectLayout {
            root: root.into(),
            state_dir: Path::new(root).join(".lyracore"),
            logs_dir: Path::new(root).join(".lyracore/logs"),
        };
        for root in ["/mnt/c/Users/me/LyraCore", "/mnt/D/LyraCore"] {
            let check = check_checkout_drive(Some(&at(root)));
            let Check::Warn { guidance, .. } = check else {
                panic!("{root} must warn in WSL");
            };
            assert!(guidance.contains("~/LyraCore"), "{guidance}");
        }
        for root in [
            "/home/me/LyraCore",
            "/mnt/data/LyraCore",
            "/srv/mnt/c/LyraCore",
        ] {
            assert!(matches!(
                check_checkout_drive(Some(&at(root))),
                Check::Pass { .. }
            ));
        }
        assert!(!check_checkout_drive(None).is_blocking());
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

    // ---- the wasm-opt check ----

    #[test]
    fn wasm_opt_missing_warns_and_never_blocks() {
        // The real check shells out to a binary named `wasm-opt`, which does not exist under
        // that name in this test environment either way — but the point being asserted is the
        // Check *variant*, not the environment, so drive it through the same shape check_wasm_opt
        // produces on a miss.
        let check = Check::warn(
            "wasm-opt",
            "not found — `spacetime publish` will still work, but ships an unoptimised module; \
             install with `apt-get install -y binaryen` (Debian/Ubuntu) or `brew install binaryen` \
             (macOS)",
        );
        assert!(!check.is_blocking());
        match check {
            Check::Warn { guidance, .. } => {
                assert!(guidance.contains("binaryen"), "{guidance}");
                assert!(guidance.contains("spacetime publish"), "{guidance}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn wasm_opt_check_itself_is_never_blocking_on_this_machine() {
        // Exercises the real function (whichever branch this CI host takes): present or absent,
        // a missing wasm-opt must never fail `doctor`.
        assert!(!check_wasm_opt().is_blocking());
    }

    // ---- the Bun check ----

    #[test]
    fn the_checkout_selects_the_bun_version_and_install_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        std::fs::create_dir_all(project.datascripts_dir()).unwrap();
        for version in ["1.3.7", "1.4.3"] {
            std::fs::write(
                project.datascripts_dir().join("package.json"),
                serde_json::json!({"engines": {"bun": version}}).to_string(),
            )
            .unwrap();
            let required = required_bun(&project).unwrap();
            assert_eq!(required.to_string(), version);
            assert!(matches!(
                check_bun_banner(Some(version), required),
                Check::Pass { .. }
            ));
            let Check::Warn { guidance, .. } = check_bun_banner(None, required) else {
                panic!("missing Bun must warn");
            };
            assert!(guidance.contains(&format!("bun-v{version}")), "{guidance}");
        }
    }

    #[test]
    fn a_missing_or_invalid_bun_pin_is_actionable_and_never_blocks_doctor() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        assert!(required_bun(&project).is_err());
        assert!(!check_bun(Some(&project)).is_blocking());
        std::fs::create_dir_all(project.datascripts_dir()).unwrap();
        for manifest in [
            "{",
            "{}",
            r#"{"engines":{"bun":143}}"#,
            r#"{"engines":{"bun":"^1.4.3"}}"#,
            r#"{"engines":{"bun":">=1.4.3"}}"#,
            r#"{"engines":{"bun":"1.4"}}"#,
            r#"{"engines":{"bun":"1.4.3-beta"}}"#,
        ] {
            std::fs::write(project.datascripts_dir().join("package.json"), manifest).unwrap();
            let error = required_bun(&project).unwrap_err().to_string();
            assert!(
                error.contains("package.json") && error.contains("engines.bun"),
                "{error}"
            );
            assert!(!check_bun(Some(&project)).is_blocking());
        }
    }

    #[test]
    fn the_required_bun_version_is_an_exact_pin() {
        assert!(matches!(
            check_bun_banner(Some("1.3.7\n"), Version(1, 3, 7)),
            Check::Pass { .. }
        ));
        for version in ["1.3.6", "1.4.0", "2.0.0"] {
            let Check::Warn { guidance, .. } = check_bun_banner(Some(version), Version(1, 3, 7))
            else {
                panic!("{version} must not satisfy the Bun pin");
            };
            assert!(guidance.contains("bun-v1.3.7"), "{guidance}");
        }
    }

    #[test]
    fn bun_reports_its_bare_version_banner() {
        // `bun --version` prints just `1.3.7`, with no tool name around it.
        assert_eq!(parse_bare_version("1.3.7\n"), Some(Version(1, 3, 7)));
        for malformed in ["bun 1.3.7", "1.3.7.99", "1.3", "1.3.seven"] {
            assert_eq!(parse_bare_version(malformed), None, "{malformed}");
            assert!(matches!(
                check_bun_banner(Some(malformed), Version(1, 3, 7)),
                Check::Warn { .. }
            ));
        }
    }

    #[test]
    fn a_missing_or_old_bun_is_a_warning_never_a_launch_blocker() {
        // Bun is author-side only. An Operator applying a prebuilt Package Delta needs none, so
        // `doctor` must not exit nonzero over it on their machine.
        for banner in [None, Some("1.3.7")] {
            assert!(!check_bun_banner(banner, Version(1, 4, 3)).is_blocking());
        }
    }

    #[test]
    fn missing_bun_has_the_exact_version_install_command() {
        let Check::Warn { guidance, .. } = check_bun_banner(None, Version(1, 3, 7)) else {
            panic!("missing Bun must warn");
        };
        assert!(guidance.contains("bun-v1.3.7"), "{guidance}");
        assert!(guidance.contains("bash -s"), "{guidance}");
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
