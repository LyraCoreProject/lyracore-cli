//! CLI-owned runtime state: `.lyracore/state.json`.
//!
//! Only processes this CLI started are recorded here. A SpacetimeDB the contributor was already
//! running is used but never claimed, so `dev down` cannot stop something it did not start.

use crate::project::{ClientBind, Component, Topology};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// A PID plus the identity that proves it is still the same process (see `proc::inspect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: u32,
    pub identity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    #[serde(default)]
    pub spacetime: Option<ProcessRecord>,
    #[serde(default)]
    pub gateway: Option<ProcessRecord>,
    /// The DEFAULT WORLD shard. Recorded for diagnostics and so a renamed database (#241) is
    /// visible in stale state; the rest of the topology is derived from `topology` below.
    #[serde(default)]
    pub database: String,
    /// Which topology the running stack was brought up as (#11) — `"sharded"` or `"single"`.
    ///
    /// Recorded for the same reason `client_host` is: `status`, `logs` and `account create` must
    /// describe the stack that is actually running, not the one the default would build today. A
    /// pre-#11 state file has no field, which reads back as the default (see
    /// [`Topology::from_recorded`]).
    #[serde(default)]
    pub topology: String,
    /// The host the running gateway's client-facing listeners were bound to by the `dev up` that
    /// started it. Absent or unparseable = loopback, which is also what every pre-`--lan` state
    /// file reads back as.
    #[serde(default)]
    pub client_host: String,
}

impl RuntimeState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| Error::State(format!("{} is corrupt: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn record(&self, component: Component) -> Option<&ProcessRecord> {
        match component {
            Component::Spacetime => self.spacetime.as_ref(),
            Component::Gateway => self.gateway.as_ref(),
        }
    }

    /// How the recorded stack's client-facing listeners are bound.
    pub fn bind(&self) -> ClientBind {
        ClientBind::from_recorded(&self.client_host)
    }

    /// Which database topology the recorded stack was brought up as.
    pub fn topology(&self) -> Topology {
        Topology::from_recorded(&self.topology)
    }

    pub fn set(&mut self, component: Component, record: Option<ProcessRecord>) {
        match component {
            Component::Spacetime => self.spacetime = record,
            Component::Gateway => self.gateway = record,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn state_round_trips_process_records() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");

        let mut state = RuntimeState::default();
        state.set(
            Component::Gateway,
            Some(ProcessRecord {
                pid: 5678,
                identity: "Mon Aug 4 10:00:00 2026 gateway".to_string(),
            }),
        );
        state.save(&path).unwrap();

        assert_eq!(RuntimeState::load(&path).unwrap(), state);
    }

    #[test]
    fn state_written_before_lan_existed_reads_back_as_loopback() {
        // A `state.json` from #251 has no `client_host` at all. It must load, not fail, and the
        // stack it describes must be treated as the loopback one it is.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(
            &path,
            r#"{"spacetime":null,"gateway":{"pid":42,"identity":"ours"},"database":"lyracore"}"#,
        )
        .unwrap();

        let state = RuntimeState::load(&path).unwrap();
        assert_eq!(state.record(Component::Gateway).unwrap().pid, 42);
        assert_eq!(state.bind(), ClientBind::Loopback);
        // ...and it also predates #11's topology field, which reads back as the new default.
        assert_eq!(state.topology(), Topology::Sharded);
    }

    #[test]
    fn a_recorded_topology_survives_a_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let state = RuntimeState {
            topology: Topology::Single.as_str().to_string(),
            ..Default::default()
        };
        state.save(&path).unwrap();
        assert_eq!(
            RuntimeState::load(&path).unwrap().topology(),
            Topology::Single
        );
    }

    #[test]
    fn a_recorded_lan_host_survives_a_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let state = RuntimeState {
            client_host: "192.168.1.50".to_string(),
            ..Default::default()
        };
        state.save(&path).unwrap();
        assert_eq!(
            RuntimeState::load(&path).unwrap().bind(),
            ClientBind::parse_lan("192.168.1.50").unwrap()
        );
    }

    #[test]
    fn a_missing_state_file_is_an_empty_state_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let state = RuntimeState::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(state.record(Component::Spacetime), None);
        assert_eq!(state.record(Component::Gateway), None);
    }

    #[test]
    fn serialized_state_carries_no_password_field() {
        // The acceptance contract: nothing secret is ever persisted. State holds PIDs and
        // identities only, so a leak would have to be a new field — which this catches.
        let mut state = RuntimeState {
            database: "lyracore".to_string(),
            ..Default::default()
        };
        state.set(
            Component::Spacetime,
            Some(ProcessRecord {
                pid: 1,
                identity: "id".to_string(),
            }),
        );
        let json = serde_json::to_string(&state).unwrap().to_lowercase();
        for forbidden in ["password", "secret", "token", "verifier", "passwd"] {
            assert!(
                !json.contains(forbidden),
                "serialized state must not mention {forbidden}: {json}"
            );
        }
    }
}
