//! CLI-owned runtime state: `.lyracore/state.json`.
//!
//! Only processes this CLI started are recorded here. A SpacetimeDB the contributor was already
//! running is used but never claimed, so `dev down` cannot stop something it did not start.

use crate::project::Component;
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
    /// Recorded for diagnostics and so a renamed database (#241) is visible in stale state.
    #[serde(default)]
    pub database: String,
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
            database: "spacetime-core".to_string(),
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
