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
use std::path::{Path, PathBuf};

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

    /// The loopback port whose answering means "this component is serving".
    pub fn health_port(&self) -> u16 {
        match self {
            Component::Spacetime => ProjectLayout::STDB_PORT,
            Component::Gateway => ProjectLayout::WORLD_PORT,
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
    pub const PUBLISH_SCRIPT: &'static str = "scripts/publish-module.sh";

    // Loopback-only binds. The fixture is a contributor's own machine, never a LAN listener
    // (`--lan` is parent #228's, after #225/#246).
    pub const STDB_PORT: u16 = 3000;
    pub const LOGON_PORT: u16 = 3724;
    pub const WORLD_PORT: u16 = 8085;

    pub fn stdb_listen() -> String {
        format!("127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn stdb_uri() -> String {
        format!("http://127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn logon_bind() -> String {
        format!("127.0.0.1:{}", Self::LOGON_PORT)
    }
    pub fn world_bind() -> String {
        format!("127.0.0.1:{}", Self::WORLD_PORT)
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

    pub fn log_file(&self, component: Component) -> PathBuf {
        self.logs_dir.join(format!("{}.log", component.as_str()))
    }

    pub fn gateway_bin(&self) -> PathBuf {
        self.root.join(Self::GATEWAY_BIN)
    }

    pub fn publish_script(&self) -> PathBuf {
        self.root.join(Self::PUBLISH_SCRIPT)
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
        assert_eq!(
            layout.log_file(Component::Gateway),
            tmp.path().join(".lyracore/logs/gateway.log")
        );
    }

    #[test]
    fn components_round_trip_through_their_names() {
        for component in Component::ALL {
            assert_eq!(Component::parse(component.as_str()), Some(component));
        }
        assert_eq!(Component::parse("realm-core"), None);
    }
}
