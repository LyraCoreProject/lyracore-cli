//! `lyracore client sync` — pack this checkout's client content into the operator's own client.
//!
//! A thin wrapper. The actual packer is `lyracore-importer --pack-client <client Data/ dir>
//! --apply` (`importer/src/pack_client.rs`, core repo): it builds `patch-3.MPQ` from
//! `client-patch/` plus every enabled Package's `client/`, installs the addons into
//! `Interface/AddOns/`, and clears the `WDB/` cache. Collision and licensing-firewall failures
//! happen inside the importer, before anything is written to the client. This command only
//! resolves the configured client path, builds the pinned importer binary, and runs it — the same
//! seam `import world`/`import vmaps` already use.
//!
//! There is no managed-content ledger here and no deletion: a disabled or removed Package's addon
//! that a previous sync installed is left in place. The importer prints a best-effort warning when
//! it finds one (a per-addon provenance marker it wrote on a previous `--apply`); this command adds
//! nothing to that beyond streaming its output.

use crate::cmd::import;
use crate::config::Config;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub fn sync(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    let config = Config::load(&project.config_file())?;
    let raw = config.client_data.ok_or_else(|| {
        Error::Usage(
            "no client-data path configured. Run `lyracore config set client-data PATH` first."
                .to_string(),
        )
    })?;
    let path = configured_client_data(project, &raw)?;
    println!("client data: {}", path.display());

    println!("building the importer (cargo build --bin lyracore-importer)");
    runner
        .run_and_wait(&import::build_importer_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "building the importer failed. The importer builds with the checkout's own \
                 pinned toolchain — `lyracore doctor` checks the prerequisites.\n  ({e})"
            ))
        })?;

    println!(
        "packing patch-3.MPQ and this checkout's addons into {}",
        path.display()
    );
    runner
        .run_streaming(&pack_command(project, &path))
        .map_err(|e| {
            Error::Process(format!(
                "client sync failed. Nothing is written to your client until client-patch/ and \
                 every enabled Package's client content pack without a collision or a \
                 licensing-firewall hit — see the importer's own diagnosis above.\n  ({e})"
            ))
        })?;

    println!(
        "client sync complete. Restart the client (MPQ changes) or /reload (addons) to apply."
    );
    Ok(())
}

fn configured_client_data(project: &ProjectLayout, raw: &str) -> Result<PathBuf> {
    let configured = Path::new(raw);
    // Config normally contains the canonical path written by `config set`, but older or manually
    // edited files may be relative. Stored paths are checkout config, so resolve those from the
    // checkout root — the same cwd the importer receives — rather than from the caller's subdir.
    let resolved = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        project.root.join(configured)
    };
    // The optional model/wmo notes returned here describe world-import navigation work. A client
    // sync performs none, so validate the installation without printing those unrelated notes.
    import::inspect_client_data(&resolved).map_err(|error| {
        Error::State(format!(
            "configured client-data path '{raw}' is no longer valid: {error}\n  Re-set it with \
             `lyracore config set client-data PATH`."
        ))
    })?;
    Ok(std::fs::canonicalize(&resolved).unwrap_or(resolved))
}

fn pack_command(project: &ProjectLayout, client_data: &Path) -> CommandSpec {
    CommandSpec::new(project.importer_bin().to_string_lossy().to_string())
        .arg("--pack-client")
        .arg(client_data.to_string_lossy().to_string())
        .arg("--apply")
        .cwd(project.root.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn checkout(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    /// A directory that passes for a 1.12.1 client's Data/.
    fn client_data(tmp: &TempDir) -> PathBuf {
        let data = tmp.path().join("wow/Data");
        std::fs::create_dir_all(&data).unwrap();
        for name in ["dbc.MPQ", "terrain.MPQ", "model.MPQ", "wmo.MPQ"] {
            std::fs::write(data.join(name), "").unwrap();
        }
        data
    }

    fn configured(project: &ProjectLayout, path: &Path) {
        Config {
            client_data: Some(path.to_string_lossy().to_string()),
        }
        .save(&project.config_file())
        .unwrap();
    }

    #[test]
    fn without_a_configured_path_it_refuses_naming_the_remedy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let error = sync(&project, &FakeStack::new().runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(
            error.to_string().contains("config set client-data"),
            "{error}"
        );
    }

    #[test]
    fn a_configured_path_builds_the_importer_then_packs_with_apply() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        configured(&project, &data);

        let stack = FakeStack::new();
        sync(&project, &stack.runner()).unwrap();

        let calls = stack.rendered();
        let build = calls
            .iter()
            .position(|c| c.contains("cargo build") && c.contains("lyracore-importer"))
            .expect("the importer was never built");
        let pack = calls
            .iter()
            .position(|c| c.contains("--pack-client") && c.contains("--apply"))
            .expect("the packer never ran");
        assert!(
            build < pack,
            "the importer must be built before it packs: {calls:?}"
        );
        assert!(
            calls[pack].contains(&data.to_string_lossy().to_string()),
            "{}",
            calls[pack]
        );
    }

    #[test]
    fn a_stale_configured_path_is_refused_with_imports_own_diagnosis() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        configured(&project, &tmp.path().join("nope"));

        let error = sync(&project, &FakeStack::new().runner()).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        let message = error.to_string();
        assert!(message.contains("no such directory"), "{message}");
        assert!(message.contains("config set client-data"), "{message}");
        assert!(!message.contains("--client-data"), "{message}");
    }

    #[test]
    fn an_invalid_configured_directory_names_the_supported_remedy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = tmp.path().join("invalid/Data");
        std::fs::create_dir_all(&data).unwrap();
        configured(&project, &data);

        let error = sync(&project, &FakeStack::new().runner()).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("missing dbc.MPQ"), "{message}");
        assert!(message.contains("config set client-data"), "{message}");
        assert!(!message.contains("--client-data"), "{message}");
    }

    #[test]
    fn a_relative_configured_path_is_resolved_from_the_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        configured(&project, Path::new("wow/Data"));

        let stack = FakeStack::new();
        sync(&project, &stack.runner()).unwrap();

        let pack = stack
            .rendered()
            .into_iter()
            .find(|call| call.contains("--pack-client"))
            .expect("the packer never ran");
        assert!(pack.contains(&data.to_string_lossy().to_string()), "{pack}");
    }

    #[test]
    fn a_failed_pack_is_reported_not_swallowed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        configured(&project, &data);

        let stack = FakeStack::new().fail_on("--pack-client", "addon name collision: AutoLoot");

        let error = sync(&project, &stack.runner()).unwrap_err();

        assert!(error.to_string().contains("AutoLoot"), "{error}");
    }
}
