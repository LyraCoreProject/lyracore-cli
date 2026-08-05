//! The ONE adapter holding the server project's internal package, database, script, and bind names.
//!
//! Everything the public `lyracore` command contract touches routes through the constants below, so
//! renaming the server's internals is a single-file edit here and no command surface moves with it.
//!
//! That is not theoretical: LyraCore#241 renamed the database (`spacetime-core` → `lyracore`), the
//! gateway package and binary, and every `GW_*` environment variable to `LYRACORE_*`. Absorbing it
//! meant editing these constants and the environment names in `cmd/dev.rs` — no command, flag, exit
//! code, or output format changed.

use crate::{Error, Result};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Where the CLIENT-FACING listeners (logon + world) bind.
///
/// SpacetimeDB is deliberately absent from this choice: it is loopback in *every* mode. The
/// database speaks an unauthenticated-by-default admin protocol on :3000, and `--lan` exists so a
/// second machine can run a 1.12.1 client — not so it can publish modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientBind {
    /// The default: only this machine can reach the realm.
    Loopback,
    /// A private-LAN IPv4 address of this machine, from `dev up --lan <IP>`.
    Lan(Ipv4Addr),
}

impl ClientBind {
    /// The host the logon/world listeners bind to, and the host a client must connect to.
    pub fn host(&self) -> String {
        match self {
            ClientBind::Loopback => Ipv4Addr::LOCALHOST.to_string(),
            ClientBind::Lan(ip) => ip.to_string(),
        }
    }

    /// Parse `--lan <IP>`.
    ///
    /// Only RFC1918 addresses are accepted. A public address would put an alpha game server with
    /// no rate limiting and a 2004 password hash on the internet from a `dev` command; `0.0.0.0`
    /// would do the same by accident, on every interface at once. Neither is a private LAN, and
    /// neither is something this fixture should be able to do by typo.
    pub fn parse_lan(raw: &str) -> Result<Self> {
        let ip: Ipv4Addr = raw.trim().parse().map_err(|_| {
            Error::Usage(format!(
                "--lan needs a private IPv4 address of this machine, got '{raw}'"
            ))
        })?;
        if !ip.is_private() {
            return Err(Error::Usage(format!(
                "--lan refuses {ip}: it is not a private-LAN address. Use one of this machine's \
                 own addresses in 10.0.0.0/8, 172.16.0.0/12, or 192.168.0.0/16 (`ip addr` / \
                 `ifconfig`). Loopback is already the default, and this fixture must never be \
                 published to a public address."
            )));
        }
        Ok(ClientBind::Lan(ip))
    }

    /// A restored `client_host` from `state.json`. Anything that is not a private address — a
    /// hand-edited state file, or the loopback default — reads back as loopback.
    pub fn from_recorded(host: &str) -> Self {
        match host.parse::<Ipv4Addr>() {
            Ok(ip) if ip.is_private() => ClientBind::Lan(ip),
            _ => ClientBind::Loopback,
        }
    }
}

/// A component the CLI can own a process for. The string form is also the log file stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Spacetime,
    Gateway,
}

impl Component {
    pub const ALL: [Component; 2] = [Component::Spacetime, Component::Gateway];

    pub fn as_str(&self) -> &'static str {
        match self {
            Component::Spacetime => "spacetime",
            Component::Gateway => "gateway",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Component::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The port whose answering means "this component is serving".
    pub fn health_port(&self) -> u16 {
        match self {
            Component::Spacetime => ProjectLayout::STDB_PORT,
            Component::Gateway => ProjectLayout::WORLD_PORT,
        }
    }

    /// The host that port must be probed on. The database is loopback in every mode; the gateway
    /// is wherever `--lan` bound it — probing 127.0.0.1 for a LAN-bound gateway reports a healthy
    /// process as permanently "starting".
    pub fn health_host(&self, bind: &ClientBind) -> String {
        match self {
            Component::Spacetime => std::net::Ipv4Addr::LOCALHOST.to_string(),
            Component::Gateway => bind.host(),
        }
    }
}

pub struct ProjectLayout {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl ProjectLayout {
    // ---- the #241 rename seam: internal names live here and nowhere else ----

    /// The single seeded database the local fixture publishes and talks to.
    pub const DATABASE: &'static str = "lyracore";
    /// The SpacetimeDB server alias. `publish-module.sh` pins `-s local`; we never re-select it.
    pub const STDB_SERVER: &'static str = "local";
    pub const GATEWAY_PACKAGE: &'static str = "lyracore-gateway";
    pub const GATEWAY_BIN: &'static str = "target/debug/lyracore-gateway";

    /// The SpacetimeDB module: the directory `spacetime publish -p` is pointed at, and the Cargo
    /// package `preflight` builds under the deploy feature set.
    pub const MODULE_DIR: &'static str = "module";
    pub const MODULE_PACKAGE: &'static str = "lyracore-module";
    /// The Cargo feature `publish` bakes in. `module/src/debug.rs` is only ever compiled by a
    /// publish (or by `preflight`), and a plain build omits it — which makes `publish` report a
    /// FALSE "Removed table" breaking change and abort.
    pub const DEPLOY_FEATURES: &'static str = "--features=debug_reducers";

    /// The workspace manifest and the toolchain file `preflight` reads the pinned versions out of.
    pub const RUST_TOOLCHAIN: &'static str = "rust-toolchain.toml";
    pub const SCRIPTS_DIR: &'static str = "scripts";

    /// The pinned wire-harness release this checkout consumes (#246).
    pub const WIRE_HARNESS_PIN: &'static str = ".wire-harness-rev";
    /// Paths INSIDE a harness checkout. `dev smoke` drives the generic login smoke through the
    /// harness's own adapter seam; a release that also carries a suite entrypoint is preferred.
    pub const HARNESS_SMOKE_SEAM: &'static str = "adapters/lyracore/wire.sh";
    pub const HARNESS_SUITE_SCRIPT: &'static str = "adapters/lyracore/run-suite.sh";
    pub const HARNESS_CLIENT_BIN: &'static str = "vanilla-wire";
    /// The fixture account and character the login smoke signs in as.
    pub const SMOKE_ACCOUNT: &'static str = "TEST";
    pub const SMOKE_CHARACTER: &'static str = "Ginger";

    // The database is loopback in every mode. Only the two client-facing ports follow the
    // `ClientBind` chosen by `dev up [--lan IP]`.
    pub const STDB_PORT: u16 = 3000;
    pub const LOGON_PORT: u16 = 3724;
    pub const WORLD_PORT: u16 = 8085;

    pub fn stdb_listen() -> String {
        format!("127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn stdb_uri() -> String {
        format!("http://127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn logon_bind(bind: &ClientBind) -> String {
        format!("{}:{}", bind.host(), Self::LOGON_PORT)
    }
    pub fn world_bind(bind: &ClientBind) -> String {
        format!("{}:{}", bind.host(), Self::WORLD_PORT)
    }
    /// What the realm list must advertise, so a client that reached the logon tier over the LAN is
    /// sent to a world address it can also reach (the seeded `game_realm` row says loopback).
    pub fn realm_address(bind: &ClientBind) -> String {
        Self::world_bind(bind)
    }

    // ---- layout discovery ----

    /// Walk up from the current directory to the workspace root, so `lyracore` works from any
    /// subdirectory of a checkout.
    pub fn discover() -> Result<Self> {
        let start = std::env::current_dir()?;
        let mut dir = start.as_path();
        loop {
            if Self::is_workspace_root(dir) {
                return Self::from_root(dir);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => {
                    return Err(Error::ProjectLayout(format!(
                        "not inside a {} checkout (no workspace Cargo.toml above {})",
                        Self::DATABASE,
                        start.display()
                    )))
                }
            }
        }
    }

    fn is_workspace_root(dir: &Path) -> bool {
        std::fs::read_to_string(dir.join("Cargo.toml"))
            .map(|manifest| manifest.contains("[workspace]"))
            .unwrap_or(false)
    }

    pub fn from_root(root: &Path) -> Result<Self> {
        if !Self::is_workspace_root(root) {
            return Err(Error::ProjectLayout(format!(
                "{} is not a workspace root (its Cargo.toml has no [workspace])",
                root.display()
            )));
        }
        let state_dir = root.join(".lyracore");
        Ok(Self {
            root: root.to_path_buf(),
            logs_dir: state_dir.join("logs"),
            state_dir,
        })
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// The coordinator credential this CLI minted from the local node (#297), if it ever had to.
    ///
    /// Inside the git-ignored state directory, next to `state.json` — a credential that belongs to
    /// this checkout's fixture and to nothing else, and that must never become a commit.
    pub fn token_file(&self) -> PathBuf {
        self.state_dir.join("coordinator-token")
    }

    pub fn log_file(&self, component: Component) -> PathBuf {
        self.logs_dir.join(format!("{}.log", component.as_str()))
    }

    pub fn gateway_bin(&self) -> PathBuf {
        self.root.join(Self::GATEWAY_BIN)
    }

    pub fn module_dir(&self) -> PathBuf {
        self.root.join(Self::MODULE_DIR)
    }

    pub fn module_manifest(&self) -> PathBuf {
        self.module_dir().join("Cargo.toml")
    }

    pub fn module_sources(&self) -> PathBuf {
        self.module_dir().join("src")
    }

    pub fn rust_toolchain_file(&self) -> PathBuf {
        self.root.join(Self::RUST_TOOLCHAIN)
    }

    pub fn scripts_dir(&self) -> PathBuf {
        self.root.join(Self::SCRIPTS_DIR)
    }

    pub fn wire_harness_pin(&self) -> PathBuf {
        self.root.join(Self::WIRE_HARNESS_PIN)
    }

    /// Where pinned harness releases are cached — inside the git-ignored state directory, so a
    /// harness checkout can never appear in the server repo's `git status`.
    pub fn harness_cache(&self) -> PathBuf {
        self.state_dir.join("wire-harness")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.logs_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_directory_without_a_manifest_is_not_a_project() {
        let tmp = TempDir::new().unwrap();
        assert!(ProjectLayout::from_root(tmp.path()).is_err());
    }

    #[test]
    fn a_non_workspace_manifest_is_rejected() {
        // A bare `[package]` Cargo.toml is any old crate, not this checkout — the predecessor's
        // "does Cargo.toml exist" test accepted every Rust project on the machine.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(ProjectLayout::from_root(tmp.path()).is_err());
    }

    #[test]
    fn state_and_logs_hang_off_a_single_ignored_directory() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let layout = ProjectLayout::from_root(tmp.path()).unwrap();
        assert_eq!(layout.state_file(), tmp.path().join(".lyracore/state.json"));
        // The one credential on disk lives in the same ignored directory (#297).
        assert_eq!(
            layout.token_file(),
            tmp.path().join(".lyracore/coordinator-token")
        );
        assert_eq!(
            layout.log_file(Component::Gateway),
            tmp.path().join(".lyracore/logs/gateway.log")
        );
    }

    // ---- the `--lan` bind contract ----

    #[test]
    fn lan_accepts_the_three_private_ranges_only() {
        for ok in ["192.168.1.50", "10.0.0.7", "172.16.4.4", "172.31.255.254"] {
            assert!(
                ClientBind::parse_lan(ok).is_ok(),
                "{ok} is a private address"
            );
        }
        // A public address, the wildcard, loopback, a link-local, a hostname and an IPv6 literal.
        // `--lan 0.0.0.0` in particular is the one-character typo that would expose the fixture on
        // every interface, so it must be a usage error rather than a "well, it binds".
        for refused in [
            "8.8.8.8",
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "172.32.0.1",
            "my-desktop.local",
            "::1",
            "",
        ] {
            let error = ClientBind::parse_lan(refused).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "--lan {refused} must be a usage error"
            );
        }
    }

    #[test]
    fn only_the_client_facing_ports_follow_the_lan_address() {
        let lan = ClientBind::parse_lan("192.168.1.50").unwrap();
        assert_eq!(ProjectLayout::logon_bind(&lan), "192.168.1.50:3724");
        assert_eq!(ProjectLayout::world_bind(&lan), "192.168.1.50:8085");
        assert_eq!(ProjectLayout::realm_address(&lan), "192.168.1.50:8085");
        // The database never leaves loopback, whatever `--lan` says.
        assert_eq!(ProjectLayout::stdb_listen(), "127.0.0.1:3000");
        assert_eq!(ProjectLayout::stdb_uri(), "http://127.0.0.1:3000");
        assert_eq!(
            Component::Spacetime.health_host(&lan),
            "127.0.0.1",
            "the database is probed on loopback in every mode"
        );
        assert_eq!(Component::Gateway.health_host(&lan), "192.168.1.50");
    }

    #[test]
    fn the_default_bind_is_loopback_everywhere() {
        let bind = ClientBind::Loopback;
        assert_eq!(ProjectLayout::logon_bind(&bind), "127.0.0.1:3724");
        assert_eq!(ProjectLayout::world_bind(&bind), "127.0.0.1:8085");
    }

    #[test]
    fn a_recorded_host_round_trips_and_junk_falls_back_to_loopback() {
        let lan = ClientBind::parse_lan("10.1.2.3").unwrap();
        assert_eq!(ClientBind::from_recorded(&lan.host()), lan);
        for junk in ["", "127.0.0.1", "8.8.8.8", "not-an-address"] {
            assert_eq!(ClientBind::from_recorded(junk), ClientBind::Loopback);
        }
    }

    #[test]
    fn components_round_trip_through_their_names() {
        for component in Component::ALL {
            assert_eq!(Component::parse(component.as_str()), Some(component));
        }
        assert_eq!(Component::parse("realm-core"), None);
    }
}
