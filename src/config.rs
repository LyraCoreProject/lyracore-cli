//! Persisted CLI config: `.lyracore/config.json`.
//!
//! Mirrors `state/mod.rs`'s `RuntimeState` pattern — a missing file is an empty default, and an
//! unrecognised field is ignored rather than refused, so a config file written by an older or
//! newer CLI always loads.
//!
//! Currently holds one thing: the 1.12.1 client's `Data/` directory, once `lyracore import` or
//! `lyracore config set client-data` has validated one (`cmd/config.rs`, `cmd/import.rs`).

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub client_data: Option<String>,
}

impl Config {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn config_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let config = Config {
            client_data: Some("/games/WoW-1.12.1/Data".to_string()),
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }

    #[test]
    fn a_missing_config_file_is_an_empty_default_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn an_unrecognised_field_is_tolerated_rather_than_refused() {
        // A config file written by a newer CLI with a field this one does not know yet must still
        // load — the same contract `RuntimeState` gives a pre-#11 `state.json`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"client_data":"/games/wow/Data","some_future_field":"whatever"}"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.client_data.as_deref(), Some("/games/wow/Data"));
    }

    #[test]
    fn a_config_with_no_client_data_field_at_all_loads_as_unset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    #[test]
    fn a_corrupt_config_file_is_an_error_not_a_silent_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
