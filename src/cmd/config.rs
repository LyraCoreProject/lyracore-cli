//! `lyracore config` / `lyracore config set client-data PATH`.
//!
//! Bare `config` reports the persisted values — currently the one, the 1.12.1 client's `Data/`
//! directory `import`'s fallback chain (`cmd/import.rs`) reads and writes. `config set client-data
//! PATH` validates with the same diagnostics `import` uses, then canonicalizes and saves — the
//! explicit form of what `import`'s interactive prompt already does once an answer validates.

use crate::cmd::import;
use crate::config::Config;
use crate::project::ProjectLayout;
use crate::Result;
use std::path::Path;

pub fn show(project: &ProjectLayout) -> Result<()> {
    let config = Config::load(&project.config_file())?;
    match config.client_data {
        Some(path) => println!("  client-data   {path}"),
        None => println!("  client-data   (unset)"),
    }
    Ok(())
}

pub fn set_client_data(project: &ProjectLayout, raw: &str) -> Result<()> {
    let path = Path::new(raw);
    for note in import::inspect_client_data(path)? {
        println!("{note}");
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mut config = Config::load(&project.config_file())?;
    config.client_data = Some(canonical.to_string_lossy().to_string());
    config.save(&project.config_file())?;

    println!("✓ client-data set to {}", canonical.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn checkout(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn valid_client_data(tmp: &TempDir) -> std::path::PathBuf {
        let data = tmp.path().join("wow/Data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["dbc.MPQ", "terrain.MPQ", "model.MPQ", "wmo.MPQ"] {
            std::fs::write(data.join(name), "").unwrap();
        }
        data
    }

    #[test]
    fn bare_config_reports_unset_before_anything_is_saved() {
        let tmp = TempDir::new().unwrap();
        // `show` writes to stdout, not to a buffer this test can capture — assert it does not
        // error instead, and cover the value it would have printed via `Config::load` directly.
        let project = checkout(&tmp);
        assert!(show(&project).is_ok());
        assert_eq!(Config::load(&project.config_file()).unwrap().client_data, None);
    }

    #[test]
    fn set_client_data_validates_canonicalizes_and_persists() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = valid_client_data(&tmp);

        set_client_data(&project, &data.to_string_lossy()).unwrap();

        let canonical = std::fs::canonicalize(&data).unwrap();
        let config = Config::load(&project.config_file()).unwrap();
        assert_eq!(
            config.client_data.as_deref(),
            Some(canonical.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn set_client_data_refuses_a_bad_path_with_the_same_diagnostics_as_import() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let bad = tmp.path().join("nope");

        let error = set_client_data(&project, &bad.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no such directory"), "{error}");
        assert!(
            Config::load(&project.config_file()).unwrap().client_data.is_none(),
            "a refused path must not be persisted"
        );
    }

    #[test]
    fn setting_a_new_path_replaces_whatever_was_configured_before() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        Config {
            client_data: Some("/games/stale/Data".to_string()),
        }
        .save(&project.config_file())
        .unwrap();
        let data = valid_client_data(&tmp);

        set_client_data(&project, &data.to_string_lossy()).unwrap();

        let canonical = std::fs::canonicalize(&data).unwrap();
        let config = Config::load(&project.config_file()).unwrap();
        assert_eq!(
            config.client_data.as_deref(),
            Some(canonical.to_string_lossy().as_ref())
        );
    }
}
