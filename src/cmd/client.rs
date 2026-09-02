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
use crate::cmd::packages::{self, stamp};
use crate::config::Config;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use serde::Serialize;
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

/// `lyracore client pack --out DIR [--zip]` — build the distributable Client Artifact.
///
/// Unlike `sync`, this never touches the configured client-data path: it drives the importer's
/// `--pack-out` mode, which packs only package-authored content (`client-patch/` and every enabled
/// Package's `client/`) and refuses a baseline-derived input on its own. `Config` is loaded here
/// only to refuse an `--out` aimed at the operator's own client; nothing else reads it.
///
/// The manifest file name doubles as the ownership marker: an `--out` this command wrote before is
/// the only kind it will clear and repack. Anything else it finds non-empty is refused, named, and
/// left untouched.
const MANIFEST_FILE: &str = "lyracore-client-pack.json";

#[derive(Debug, Serialize)]
struct Manifest {
    format: u32,
    packed_at: String,
    core_revision: String,
    packages: Vec<PackageManifestEntry>,
    /// Relative paths of every file under the artifact, sorted, excluding the manifest itself.
    contents: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PackageManifestEntry {
    name: String,
    source_kind: String,
    source: String,
    revision: String,
    content_identity: String,
}

pub fn pack(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    out: &str,
    zip: bool,
) -> Result<()> {
    let out = resolve_out(project, out);
    refuse_reserved_out(project, &out)?;

    let ours = out.join(MANIFEST_FILE).is_file();
    if out.is_dir() && !ours && dir_has_entries(&out)? {
        return Err(Error::Usage(format!(
            "{} already exists, is not empty, and has no {MANIFEST_FILE} from a previous `client \
             pack`. Refusing to overwrite a directory this command did not create. Pack into an \
             empty or new directory, or point --out at that prior artifact.",
            out.display()
        )));
    }
    let zip_path = zip_path_for(&out);
    if zip && zip_path.exists() && !ours {
        return Err(Error::Usage(format!(
            "{} already exists and {} was not a previous `client pack` artifact. Refusing to \
             overwrite a zip this command did not create.",
            zip_path.display(),
            out.display()
        )));
    }

    prepare_out(&out, ours)?;

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
        "packing package-authored client content into {}",
        out.display()
    );
    runner
        .run_streaming(&pack_out_command(project, &out))
        .map_err(|e| {
            Error::Process(format!(
                "client pack failed. {} is not a complete artifact until every package-authored \
                 MPQ file and addon packs without a collision or a licensing-firewall hit — see \
                 the importer's own diagnosis above.\n  ({e})",
                out.display()
            ))
        })?;

    let manifest = build_manifest(project, runner, &out)?;
    write_manifest(&out, &manifest)?;

    if zip {
        println!("archiving {} -> {}", out.display(), zip_path.display());
        runner
            .run_and_wait(&zip_command(&out, &zip_path))
            .map_err(|e| {
                Error::Process(format!(
                    "creating {} failed: could not run the `zip` binary ({e}). Install `zip` and \
                     re-run with --zip, or drop --zip and archive {} yourself.",
                    zip_path.display(),
                    out.display()
                ))
            })?;
    }

    report(&out, &manifest, zip.then_some(zip_path.as_path()));
    Ok(())
}

/// Resolve `--out` against the checkout root when it is relative, canonicalizing when the path
/// already exists. A path that does not exist yet cannot canonicalize, so it is compared and
/// created in its resolved-but-not-canonical form, the same fallback `client sync` uses for the
/// configured client-data path.
fn resolve_out(project: &ProjectLayout, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.root.join(path)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// Refuse an `--out` aimed at the operator's own client or at this checkout's Package inventory.
/// The only place `pack` reads `Config` — never to validate or use the client-data path, only to
/// name it in this one refusal.
fn refuse_reserved_out(project: &ProjectLayout, out: &Path) -> Result<()> {
    let config = Config::load(&project.config_file())?;
    if let Some(raw) = config.client_data {
        let path = Path::new(&raw);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            project.root.join(path)
        };
        let client_data = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        if out.starts_with(&client_data) {
            return Err(Error::Usage(format!(
                "--out {} is inside the configured client-data path {}. `client pack` builds a \
                 distributable artifact and must never write into your own client. Point --out \
                 somewhere else.",
                out.display(),
                client_data.display()
            )));
        }
    }
    let packages_dir = project.packages_dir();
    if out.starts_with(&packages_dir) {
        return Err(Error::Usage(format!(
            "--out {} is inside {}. `client pack` writes a distributable artifact, not Package \
             source — point --out outside the checkout's Package inventory.",
            out.display(),
            packages_dir.display()
        )));
    }
    Ok(())
}

fn dir_has_entries(dir: &Path) -> Result<bool> {
    Ok(std::fs::read_dir(dir)?.next().is_some())
}

/// A missing or empty `out` is created (a no-op if it already exists and is empty). An `out` this
/// command owns (`ours`) is emptied first — the ownership refusal above already proved it is safe
/// to clear.
fn prepare_out(out: &Path, ours: bool) -> Result<()> {
    if !out.is_dir() {
        std::fs::create_dir_all(out)?;
        return Ok(());
    }
    if !ours {
        return Ok(());
    }
    for entry in std::fs::read_dir(out)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn pack_out_command(project: &ProjectLayout, out: &Path) -> CommandSpec {
    CommandSpec::new(project.importer_bin().to_string_lossy().to_string())
        .arg("--pack-out")
        .arg(out.to_string_lossy().to_string())
        .cwd(project.root.clone())
}

fn zip_path_for(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".zip");
    PathBuf::from(name)
}

fn zip_command(out: &Path, zip_path: &Path) -> CommandSpec {
    CommandSpec::new("zip")
        .arg("-r")
        .arg(zip_path.to_string_lossy().to_string())
        .arg(".")
        .cwd(out.to_path_buf())
}

/// The manifest: every enabled Package's provenance and a fresh content identity, plus what the
/// importer actually left under `out`. Read fresh from disk rather than assumed, so the manifest
/// never claims content the pack step did not produce.
fn build_manifest(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    out: &Path,
) -> Result<Manifest> {
    Ok(Manifest {
        format: 1,
        packed_at: stamp::utc_rfc3339(stamp::now_unix()),
        core_revision: core_revision(project, runner),
        packages: package_entries(project)?,
        contents: contents(out)?,
    })
}

/// `git rev-parse HEAD` at the checkout root, through the runner. `"unknown"` rather than a failed
/// pack: the commit this checkout is on is provenance, not a precondition for the artifact itself.
fn core_revision(project: &ProjectLayout, runner: &dyn ProcessRunner) -> String {
    let head = CommandSpec::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .cwd(project.root.clone());
    runner
        .run_and_wait(&head)
        .map(|sha| sha.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Every enabled Package, with a content identity computed now rather than read from its stamp —
/// the manifest should say what actually shipped, not what was true at install time. A Package
/// with no stamp (hand-created, or older than `packages add`) records the three provenance fields
/// empty rather than failing the pack over unrelated missing history.
fn package_entries(project: &ProjectLayout) -> Result<Vec<PackageManifestEntry>> {
    packages::inventory(project)?
        .into_iter()
        .filter(|package| package.state == packages::PackageState::Enabled)
        .map(|package| {
            let content_identity = stamp::content_identity(&package.dir)?;
            let (source_kind, source, revision) = match package.stamp {
                Some(recorded) => (recorded.source_kind, recorded.source, recorded.revision),
                None => (String::new(), String::new(), String::new()),
            };
            Ok(PackageManifestEntry {
                name: package.name.as_str().to_string(),
                source_kind,
                source,
                revision,
                content_identity,
            })
        })
        .collect()
}

/// Every file under `out`, sorted, excluding the manifest — `packages::tree::collect`'s traversal
/// already returns entries in sorted relative-path order, so filtering it to files preserves that.
fn contents(out: &Path) -> Result<Vec<String>> {
    Ok(packages::tree::collect(out)?
        .into_iter()
        .filter(|entry| entry.kind == packages::tree::EntryKind::File)
        .map(|entry| entry.relative.to_string_lossy().replace('\\', "/"))
        .filter(|relative| relative != MANIFEST_FILE)
        .collect())
}

fn write_manifest(out: &Path, manifest: &Manifest) -> Result<()> {
    std::fs::write(
        out.join(MANIFEST_FILE),
        serde_json::to_string_pretty(manifest)?,
    )?;
    Ok(())
}

fn report(out: &Path, manifest: &Manifest, zip_path: Option<&Path>) {
    let mpq_present = out.join("Data").join("patch-3.MPQ").is_file();
    let addon_count = std::fs::read_dir(out.join("Interface").join("AddOns"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    println!();
    println!("client pack complete: {}", out.display());
    println!(
        "  Data/patch-3.MPQ  {}",
        if mpq_present {
            "present"
        } else {
            "absent (no package-authored MPQ files)"
        }
    );
    println!("  addons            {addon_count}");
    println!("  packages          {}", manifest.packages.len());
    if let Some(zip_path) = zip_path {
        println!("  zip               {}", zip_path.display());
    }
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

    // ---- client pack ----

    fn out_dir(tmp: &TempDir) -> PathBuf {
        tmp.path().join("out")
    }

    /// An enabled Package with a local-install stamp, one addon file, and a content identity a
    /// test can check the manifest against.
    fn enabled_package(project: &ProjectLayout, name: &str) -> PathBuf {
        let dir = project.packages_dir().join(name);
        let addon = dir.join("client/addons").join(name);
        std::fs::create_dir_all(&addon).unwrap();
        std::fs::write(addon.join(format!("{name}.lua")), "-- addon\n").unwrap();
        let identity = stamp::content_identity(&dir).unwrap();
        stamp::ProvenanceStamp::local(Path::new("/src/example"), identity, 1_756_000_000)
            .write(&dir)
            .unwrap();
        dir
    }

    #[test]
    fn out_inside_the_configured_client_data_path_is_refused_and_nothing_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let data = client_data(&tmp);
        configured(&project, &data);

        let stack = FakeStack::new();
        let out = data.join("client-pack");
        let error = pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(
            error
                .to_string()
                .contains(&data.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn out_inside_the_packages_directory_is_refused_and_nothing_runs() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let stack = FakeStack::new();
        let out = project.packages_dir().join("client-pack");
        let error = pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn a_non_empty_directory_without_a_manifest_is_refused_and_no_runner_call_happens() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("stray.txt"), "not ours").unwrap();

        let stack = FakeStack::new();
        let error = pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&out.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
        assert!(out.join("stray.txt").exists());
    }

    #[test]
    fn a_directory_with_a_prior_manifest_is_emptied_then_packed() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(MANIFEST_FILE), "{}").unwrap();
        std::fs::write(out.join("stale-addon.lua"), "-- old\n").unwrap();

        let stack = FakeStack::new();
        pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap();

        assert!(!out.join("stale-addon.lua").exists());
        assert!(out.join(MANIFEST_FILE).is_file());
    }

    #[test]
    fn the_importer_is_built_before_it_packs_with_pack_out_never_apply_or_pack_client() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);

        let stack = FakeStack::new();
        pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap();

        let calls = stack.rendered();
        let build = calls
            .iter()
            .position(|c| c.contains("cargo build") && c.contains("lyracore-importer"))
            .expect("the importer was never built");
        let out_call = calls
            .iter()
            .position(|c| c.contains("--pack-out"))
            .expect("the packer never ran");
        assert!(
            build < out_call,
            "the importer must be built before it packs: {calls:?}"
        );
        assert!(
            calls[out_call].contains(&out.to_string_lossy().to_string()),
            "{}",
            calls[out_call]
        );
        assert!(!calls[out_call].contains("--apply"), "{}", calls[out_call]);
        assert!(
            !calls[out_call].contains("--pack-client"),
            "{}",
            calls[out_call]
        );
    }

    #[test]
    fn zip_renders_exactly_one_call_after_the_manifest_exists() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);

        let stack = FakeStack::new();
        pack(&project, &stack.runner(), &out.to_string_lossy(), true).unwrap();

        let zip_calls: Vec<_> = stack
            .rendered()
            .into_iter()
            .filter(|c| c.starts_with("zip "))
            .collect();
        assert_eq!(zip_calls.len(), 1, "{zip_calls:?}");
        assert!(out.join(MANIFEST_FILE).is_file());
    }

    #[test]
    fn without_zip_there_is_no_zip_call() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);

        let stack = FakeStack::new();
        pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap();

        assert!(
            stack.rendered().iter().all(|c| !c.starts_with("zip ")),
            "{:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_pre_existing_zip_is_refused_when_out_was_not_previously_ours() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::write(zip_path_for(&out), "not ours").unwrap();

        let stack = FakeStack::new();
        let error = pack(&project, &stack.runner(), &out.to_string_lossy(), true).unwrap_err();

        assert!(error.to_string().contains("zip"), "{error}");
        assert!(stack.rendered().is_empty(), "{:?}", stack.rendered());
    }

    #[test]
    fn a_pre_existing_zip_is_overwritten_when_out_was_previously_ours() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(MANIFEST_FILE), "{}").unwrap();
        std::fs::write(zip_path_for(&out), "old zip").unwrap();

        let stack = FakeStack::new();
        pack(&project, &stack.runner(), &out.to_string_lossy(), true).unwrap();

        let zip_calls = stack
            .rendered()
            .into_iter()
            .filter(|c| c.starts_with("zip "))
            .count();
        assert_eq!(zip_calls, 1);
    }

    #[test]
    fn a_failed_pack_is_reported_and_leaves_no_manifest() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);

        let stack = FakeStack::new().fail_on("--pack-out", "addon name collision: AutoLoot");

        let error = pack(&project, &stack.runner(), &out.to_string_lossy(), false).unwrap_err();

        assert!(error.to_string().contains("AutoLoot"), "{error}");
        assert!(!out.join(MANIFEST_FILE).exists());
    }

    #[test]
    fn the_manifest_lists_every_enabled_package_with_a_fresh_identity_and_the_files_in_out() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        enabled_package(&project, "greeter");
        let out = out_dir(&tmp);
        std::fs::create_dir_all(out.join("Interface/AddOns/Greeter")).unwrap();
        std::fs::write(out.join("Interface/AddOns/Greeter/Greeter.lua"), "-- hi\n").unwrap();
        std::fs::create_dir_all(out.join("Data")).unwrap();
        std::fs::write(out.join("Data/patch-3.MPQ"), "mpq bytes").unwrap();

        let stack = FakeStack::new();
        let manifest = build_manifest(&project, &stack.runner(), &out).unwrap();

        assert_eq!(manifest.format, 1);
        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].name, "greeter");
        assert_eq!(manifest.packages[0].source_kind, "local");
        assert_eq!(manifest.packages[0].source, "/src/example");
        let identity = stamp::content_identity(&project.packages_dir().join("greeter")).unwrap();
        assert_eq!(manifest.packages[0].content_identity, identity);
        assert_eq!(
            manifest.contents,
            vec![
                "Data/patch-3.MPQ".to_string(),
                "Interface/AddOns/Greeter/Greeter.lua".to_string(),
            ]
        );
    }

    #[test]
    fn a_package_without_a_stamp_records_empty_provenance_fields() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("handmade/src")).unwrap();
        std::fs::write(
            project.packages_dir().join("handmade/src/mod.rs"),
            "pub fn a() {}\n",
        )
        .unwrap();
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();

        let stack = FakeStack::new();
        let manifest = build_manifest(&project, &stack.runner(), &out).unwrap();

        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].source_kind, "");
        assert_eq!(manifest.packages[0].source, "");
        assert_eq!(manifest.packages[0].revision, "");
        assert!(!manifest.packages[0].content_identity.is_empty());
    }

    #[test]
    fn core_revision_falls_back_to_unknown_when_git_fails() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();

        let stack = FakeStack::new().fail_on("rev-parse HEAD", "not a git repository");
        let manifest = build_manifest(&project, &stack.runner(), &out).unwrap();

        assert_eq!(manifest.core_revision, "unknown");
    }

    #[test]
    fn core_revision_is_read_through_the_runner() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();

        let stack = FakeStack::new().with_stdout("rev-parse HEAD", "abc123\n");
        let manifest = build_manifest(&project, &stack.runner(), &out).unwrap();

        assert_eq!(manifest.core_revision, "abc123");
    }

    #[test]
    fn contents_excludes_the_manifest_itself() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let out = out_dir(&tmp);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join(MANIFEST_FILE), "{}").unwrap();
        std::fs::write(out.join("placeholder.txt"), "x").unwrap();

        let stack = FakeStack::new();
        let manifest = build_manifest(&project, &stack.runner(), &out).unwrap();

        assert_eq!(manifest.contents, vec!["placeholder.txt".to_string()]);
    }
}
