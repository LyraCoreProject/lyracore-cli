//! The Build Identity: what a generated Package Delta artifact was built FROM, recorded next to it
//! so a later run can tell whether it is still current.
//!
//! `packages build` writes one sidecar per artifact, at `<package>/data/.generated/spell.identity.
//! json`, right after that artifact has emitted and validated. `packages check` (and the identity
//! gate `preflight` runs on its behalf) recomputes every recorded input from the checkout on disk
//! and refuses, naming the specific input, the moment one no longer matches.
//!
//! # A deliberate deviation: the sidecar name does not end in `.json`
//!
//! Issue #316's triage named the sidecar `spell.identity.json`, next to `spell.json`. It cannot be
//! called that: `artifact::read_enabled` — reused here unmodified, per this change's file ownership
//! — globs EVERY `*.json` file in `data/.generated/` and hard-parses each one as a Package Delta
//! artifact. A sidecar ending in `.json` would be swept into that scan and fail to parse as one,
//! breaking discovery for the very Package it describes. [`IDENTITY_FILE`] therefore has no `.json`
//! suffix; its content is still canonical JSON.
//!
//! # Why a sibling file, not a member of the artifact
//!
//! `PackageDelta::parse` (the engine crate `artifact.rs` mirrors) refuses an unknown member, and
//! `game_package_import.artifact_hash` is BLAKE3 over exactly the artifact's four canonical fields.
//! Folding the Build Identity into the artifact would move that hash on every Bun or tsconfig bump
//! that changes nothing about what the artifact CLAIMS — and `packages replay` treats a moved hash
//! as a Shard needing every claim reapplied. The two questions ("what does this artifact say" and
//! "is this artifact still current") stay two files for exactly that reason.
//!
//! # What is recorded, and how
//!
//! Every recorded hash is SHA-256, over UTF-8 relative paths and file bytes — deliberately a
//! different algorithm from the artifact's own BLAKE3 canonical-form digest, so the two can never be
//! confused for one another by a shared prefix. A tree hash walks [`super::tree::collect`]'s sorted
//! entries (the same traversal `packages add` reviews a Package tree with) so a moved file, an added
//! empty directory, and an edited one are all drift; a single-file hash is the file's raw bytes.
//! Nothing here reads a timestamp or an absolute path, so the sidecar for one input tree is
//! byte-identical wherever it is built.
//!
//! The recorded Bun version is the checkout's PIN (`doctor::REQUIRED_BUN`), not a shelled `bun
//! --version` — `packages build` already hard-gates on that exact pin before it runs anything, so
//! asking Bun to confirm its own version a second time would only be a slower way to read the same
//! constant.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::tree::{self, EntryKind};
use crate::cmd::doctor;
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// Where `packages build` writes one Build Identity, next to the artifact it describes.
///
/// No `.json` suffix, despite JSON content — see the module doc comment for why: that suffix would
/// put this file inside `artifact::read_enabled`'s own artifact glob.
pub const IDENTITY_FILE: &str = "spell.identity";

/// The same sidecar, for the Script Artifact next to it. A Package may ship both kinds, and they
/// are built from different inputs, so each records its own.
pub const SCRIPT_IDENTITY_FILE: &str = "script.identity";

/// The only sidecar envelope version this CLI reads or writes.
const IDENTITY_VERSION: u64 = 1;

/// One recorded input, named so a staleness refusal can point at exactly what changed rather than
/// making the operator diff two JSON files by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Source,
    Generated,
    Snapshot,
    Lib,
    BunVersion,
    Tsconfig,
    PackageJson,
    BunLock,
    Artifact,
    ScriptSource,
    Toolchain,
}

impl Input {
    /// What changed, and where to look — the sentence a staleness refusal names this input with.
    pub fn description(self) -> &'static str {
        match self {
            Input::Source => "its Datascript source under datascripts/src/<package>/",
            Input::Generated => {
                "the generated Module schema/typings in datascripts/generated/ (a schema change)"
            }
            Input::Snapshot => "the Base Snapshot at datascripts/generated/base-snapshot.json",
            Input::Lib => "the authoring library at datascripts/lib/",
            Input::BunVersion => "the pinned Bun version this checkout builds Datascripts with",
            Input::Tsconfig => "datascripts/tsconfig.json",
            Input::PackageJson => "datascripts/package.json",
            Input::BunLock => "datascripts/bun.lock",
            Input::Artifact => {
                "the artifact file itself (its bytes no longer match what was last built — hand-edited?)"
            }
            Input::ScriptSource => "its Runtime Script sources under packages/<package>/scripts/",
            Input::Toolchain => {
                "the pinned Runtime Script toolchain in datascripts/runtime-scripts/"
            }
        }
    }
}

/// One Package's Build Identity: every input `packages build` read before it wrote the artifact
/// next to this sidecar.
///
/// `snapshot_hash` is empty exactly when the Base Snapshot was absent when this value was computed
/// — see [`compute`]. A value written by `packages build` never has an empty one: the build already
/// refuses to run any Datascript before a Base Snapshot exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub source_hash: String,
    pub generated_hash: String,
    pub snapshot_hash: String,
    pub lib_hash: String,
    pub bun_version: String,
    pub tsconfig_hash: String,
    pub package_json_hash: String,
    pub bun_lock_hash: String,
    pub artifact_hash: String,
}

impl Identity {
    /// Every recorded field that differs between `self` (typically the sidecar as recorded) and
    /// `current` (typically freshly recomputed), in the sidecar's own field order.
    ///
    /// `verify_snapshot` skips the snapshot comparison entirely rather than reporting a mismatch: a
    /// checkout with no local Base Snapshot cannot recompute it at all, so silence here (as opposed
    /// to a false match OR a false mismatch) is the only honest answer. The caller reports that
    /// omission separately.
    pub fn changed_against(&self, current: &Identity, verify_snapshot: bool) -> Vec<Input> {
        let mut changed = Vec::with_capacity(9);
        let mut check = |same: bool, input: Input| {
            if !same {
                changed.push(input);
            }
        };
        check(self.source_hash == current.source_hash, Input::Source);
        check(
            self.generated_hash == current.generated_hash,
            Input::Generated,
        );
        if verify_snapshot {
            check(self.snapshot_hash == current.snapshot_hash, Input::Snapshot);
        }
        check(self.lib_hash == current.lib_hash, Input::Lib);
        check(self.bun_version == current.bun_version, Input::BunVersion);
        check(self.tsconfig_hash == current.tsconfig_hash, Input::Tsconfig);
        check(
            self.package_json_hash == current.package_json_hash,
            Input::PackageJson,
        );
        check(self.bun_lock_hash == current.bun_lock_hash, Input::BunLock);
        check(self.artifact_hash == current.artifact_hash, Input::Artifact);
        changed
    }

    /// The sidecar's canonical bytes: no whitespace, members in a fixed alphabetical order, so two
    /// computations of the same inputs are byte-identical wherever they run.
    pub fn render(&self) -> String {
        format!(
            "{{\"artifact_hash\":{a},\"bun_lock_hash\":{bl},\"bun_version\":{bv},\
             \"generated_hash\":{g},\"lib_hash\":{l},\"package_json_hash\":{pj},\
             \"snapshot_hash\":{s},\"source_hash\":{sh},\"tsconfig_hash\":{t},\"version\":{v}}}",
            a = json_string(&self.artifact_hash),
            bl = json_string(&self.bun_lock_hash),
            bv = json_string(&self.bun_version),
            g = json_string(&self.generated_hash),
            l = json_string(&self.lib_hash),
            pj = json_string(&self.package_json_hash),
            s = json_string(&self.snapshot_hash),
            sh = json_string(&self.source_hash),
            t = json_string(&self.tsconfig_hash),
            v = IDENTITY_VERSION,
        )
    }

    /// Read a sidecar back. `path` is used only to name the file in a refusal.
    pub fn parse(text: &str, path: &Path) -> Result<Identity> {
        let refuse = |what: String| Error::Usage(format!("{}: {what}", path.display()));
        let object = parse_sidecar(text, path)?;

        let field = |name: &str| -> Result<String> {
            object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| refuse(format!("no `{name}`")))
        };

        Ok(Identity {
            source_hash: field("source_hash")?,
            generated_hash: field("generated_hash")?,
            snapshot_hash: field("snapshot_hash")?,
            lib_hash: field("lib_hash")?,
            bun_version: field("bun_version")?,
            tsconfig_hash: field("tsconfig_hash")?,
            package_json_hash: field("package_json_hash")?,
            bun_lock_hash: field("bun_lock_hash")?,
            artifact_hash: field("artifact_hash")?,
        })
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// SHA-256 over an existing directory's [`tree::collect`] entries: kind, relative path, and (for a
/// file) its length and bytes, each length-prefixed with a NUL so no concatenation of two trees can
/// collide. A directory that does not exist hashes as the empty tree — a checkout that predates
/// `datascripts/lib/`, or one whose typings have not been generated yet, is a real (and different)
/// state from one where the directory exists and is empty.
///
/// `exclude` names one top-level relative file to skip, so a tree that also holds a separately
/// hashed file (the Base Snapshot inside `datascripts/generated/`) does not double-count it under
/// two different [`Input`]s.
fn hash_tree(root: &Path, exclude: Option<&str>) -> Result<String> {
    let mut hasher = Sha256::new();
    if root.is_dir() {
        for entry in tree::collect(root)? {
            let relative = entry.relative.to_string_lossy().replace('\\', "/");
            if entry.kind == EntryKind::File && Some(relative.as_str()) == exclude {
                continue;
            }
            hasher.update(match entry.kind {
                EntryKind::Directory => b"directory".as_slice(),
                EntryKind::File => b"file".as_slice(),
            });
            hasher.update([0u8]);
            hasher.update(relative.as_bytes());
            hasher.update([0u8]);
            if entry.kind == EntryKind::File {
                let bytes = std::fs::read(&entry.path)?;
                hasher.update(bytes.len().to_string().as_bytes());
                hasher.update([0u8]);
                hasher.update(&bytes);
            }
        }
    }
    Ok(format!("sha256-tree-v1:{:x}", hasher.finalize()))
}

/// SHA-256 of one file's raw bytes.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| {
        Error::Process(format!(
            "cannot read {} for its Build Identity: {e}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("sha256-v1:{:x}", hasher.finalize()))
}

/// The enabled Package folder an artifact was discovered inside — the top component of its path
/// relative to `packages/`. This is a Package's FOLDER name, which is what selects its Datascript
/// source under `datascripts/src/<folder>/`; it need not equal the artifact's own declared
/// `package` identity.
pub fn package_dir(project: &ProjectLayout, artifact_path: &Path) -> Result<PathBuf> {
    let root = project.packages_dir();
    let relative = artifact_path.strip_prefix(&root).map_err(|_| {
        Error::State(format!(
            "artifact {} is not inside the enabled Package Inventory {}",
            artifact_path.display(),
            root.display()
        ))
    })?;
    let folder = relative.components().next().ok_or_else(|| {
        Error::State(format!(
            "artifact {} names no enclosing Package folder",
            artifact_path.display()
        ))
    })?;
    Ok(root.join(folder.as_os_str()))
}

/// Compute the Build Identity for one Package from the checkout on disk right now.
///
/// Returns whether the Base Snapshot was present: `false` means `snapshot_hash` is empty and the
/// caller must not compare it — see [`Identity::changed_against`]. Every other field is always
/// computed; a missing `datascripts/lib/` or `datascripts/generated/` hashes as an empty tree rather
/// than failing, so a checkout that predates one of them still gets an Identity to compare against.
pub fn compute(
    project: &ProjectLayout,
    package_dir: &Path,
    artifact_hash: &str,
) -> Result<(Identity, bool)> {
    let name = package_dir.file_name().ok_or_else(|| {
        Error::State(format!(
            "Package directory {} has no folder name",
            package_dir.display()
        ))
    })?;
    let datascripts = project.datascripts_dir();

    let source_hash = hash_tree(&project.datascripts_src_dir().join(name), None)?;
    let generated_hash = hash_tree(
        &project.datascript_types_dir(),
        Some(ProjectLayout::BASE_SNAPSHOT_FILE),
    )?;
    let snapshot_file = project.base_snapshot_file();
    let (snapshot_hash, snapshot_available) = if snapshot_file.is_file() {
        (hash_file(&snapshot_file)?, true)
    } else {
        (String::new(), false)
    };
    let lib_hash = hash_tree(&project.datascripts_lib_dir(), None)?;
    let tsconfig_hash = hash_file(&datascripts.join("tsconfig.json"))?;
    let package_json_hash = hash_file(&datascripts.join("package.json"))?;
    let bun_lock_hash = hash_file(&datascripts.join("bun.lock"))?;

    Ok((
        Identity {
            source_hash,
            generated_hash,
            snapshot_hash,
            lib_hash,
            bun_version: doctor::REQUIRED_BUN.to_string(),
            tsconfig_hash,
            package_json_hash,
            bun_lock_hash,
            artifact_hash: artifact_hash.to_string(),
        },
        snapshot_available,
    ))
}

/// Write one Build Identity sidecar next to `artifact.path`. Called only after that artifact has
/// emitted and validated — see the module doc comment for why a stale identity next to a
/// half-trusted artifact would be worse than no identity at all.
///
/// The Base Snapshot must be present: `packages build` already refuses to run any Datascript
/// without one, so a missing one here means that invariant broke rather than describing a real
/// checkout state.
pub fn write(project: &ProjectLayout, artifact: &super::artifact::Artifact) -> Result<()> {
    let dir = package_dir(project, &artifact.path)?;
    let (identity, snapshot_available) = compute(project, &dir, &artifact.artifact_hash)?;
    if !snapshot_available {
        return Err(Error::State(format!(
            "writing the Build Identity for `{}` found no Base Snapshot at {}, but `packages \
             build` already requires one before any Datascript runs — this should be unreachable",
            artifact.package,
            project.base_snapshot_file().display()
        )));
    }
    let sidecar = artifact
        .path
        .parent()
        .ok_or_else(|| {
            Error::State(format!(
                "artifact path {} has no parent directory",
                artifact.path.display()
            ))
        })?
        .join(IDENTITY_FILE);
    std::fs::write(sidecar, identity.render())?;
    Ok(())
}

/// Write a Build Identity sidecar next to every one of `artifacts`.
pub fn write_all(project: &ProjectLayout, artifacts: &[super::artifact::Artifact]) -> Result<()> {
    for artifact in artifacts {
        write(project, artifact)?;
    }
    Ok(())
}

// ---- the Script Artifact's own Build Identity ----

/// What a Script Artifact was built from. Four inputs, not the Delta's nine: a Runtime Script reads
/// no Base Snapshot, imports no authoring library, and never sees the Module schema typings.
///
/// `artifact_hash` here is SHA-256 of the artifact file's raw bytes, not the Delta's BLAKE3
/// canonical-form digest: this CLI reproduces the canonical form for Package Deltas only, and the
/// question a sidecar answers is "were these bytes hand-edited", which raw bytes answer exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptIdentity {
    pub source_hash: String,
    pub toolchain_hash: String,
    pub bun_version: String,
    pub bun_lock_hash: String,
    pub artifact_hash: String,
}

impl ScriptIdentity {
    /// Every recorded input that differs between `self` and `current`, in the sidecar's field order.
    pub fn changed_against(&self, current: &ScriptIdentity) -> Vec<Input> {
        let mut changed = Vec::with_capacity(5);
        let mut check = |same: bool, input: Input| {
            if !same {
                changed.push(input);
            }
        };
        check(self.source_hash == current.source_hash, Input::ScriptSource);
        check(
            self.toolchain_hash == current.toolchain_hash,
            Input::Toolchain,
        );
        check(self.bun_version == current.bun_version, Input::BunVersion);
        check(self.bun_lock_hash == current.bun_lock_hash, Input::BunLock);
        check(self.artifact_hash == current.artifact_hash, Input::Artifact);
        changed
    }

    /// Canonical bytes, in the same spelling [`Identity::render`] uses.
    pub fn render(&self) -> String {
        format!(
            "{{\"artifact_hash\":{a},\"bun_lock_hash\":{bl},\"bun_version\":{bv},\
             \"source_hash\":{s},\"toolchain_hash\":{t},\"version\":{v}}}",
            a = json_string(&self.artifact_hash),
            bl = json_string(&self.bun_lock_hash),
            bv = json_string(&self.bun_version),
            s = json_string(&self.source_hash),
            t = json_string(&self.toolchain_hash),
            v = IDENTITY_VERSION,
        )
    }

    /// Read a sidecar back. `path` is used only to name the file in a refusal.
    pub fn parse(text: &str, path: &Path) -> Result<ScriptIdentity> {
        let object = parse_sidecar(text, path)?;
        let refuse = |what: String| Error::Usage(format!("{}: {what}", path.display()));
        let field = |name: &str| -> Result<String> {
            object
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| refuse(format!("no `{name}`")))
        };
        Ok(ScriptIdentity {
            source_hash: field("source_hash")?,
            toolchain_hash: field("toolchain_hash")?,
            bun_version: field("bun_version")?,
            bun_lock_hash: field("bun_lock_hash")?,
            artifact_hash: field("artifact_hash")?,
        })
    }
}

/// SHA-256 over the FILES directly inside `root`, sorted by name, each length-prefixed.
///
/// Shallow on purpose: `datascripts/runtime-scripts/` holds an installed `node_modules/` beside its
/// checked-in files, and the pin for what is in there is `bun.lock`, which is a recorded input of
/// its own. A new toolchain file dropped in that directory is picked up without editing this.
fn hash_dir_files(root: &Path) -> Result<String> {
    let mut names: Vec<PathBuf> = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.is_file() {
                names.push(path);
            }
        }
    }
    names.sort();

    let mut hasher = Sha256::new();
    for path in names {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = std::fs::read(&path)?;
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
    }
    Ok(format!("sha256-dir-v1:{:x}", hasher.finalize()))
}

/// Compute the Build Identity of one Package's Script Artifact from the checkout on disk right now.
///
/// `artifact_path` is the Script Artifact itself; the Package folder it sits in selects the
/// `scripts/` sources, exactly as the Delta side derives its Datascript folder.
pub fn compute_script(project: &ProjectLayout, artifact_path: &Path) -> Result<ScriptIdentity> {
    let dir = package_dir(project, artifact_path)?;
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    Ok(ScriptIdentity {
        source_hash: hash_script_sources(&super::script::source_files(project, &name)?)?,
        toolchain_hash: hash_dir_files(&project.runtime_scripts_dir())?,
        bun_version: doctor::REQUIRED_BUN.to_string(),
        bun_lock_hash: hash_file(&project.datascripts_dir().join("bun.lock"))?,
        artifact_hash: hash_file(artifact_path)?,
    })
}

/// SHA-256 over the exact Runtime Script source inventory, mirrored in `build-scripts.ts`.
fn hash_script_sources(files: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = std::fs::read(path)?;
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
    }
    Ok(format!("sha256-script-sources-v1:{:x}", hasher.finalize()))
}

/// Write one Script Artifact Build Identity sidecar next to each of `artifacts`. Called only after
/// the artifacts have validated, for the same reason the Delta side is.
pub fn write_script_identities(project: &ProjectLayout, artifacts: &[PathBuf]) -> Result<()> {
    for path in artifacts {
        let dir = package_dir(project, path)?;
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if super::script::source_files(project, &name)?.is_empty() {
            continue;
        }
        let sidecar = path
            .parent()
            .ok_or_else(|| {
                Error::State(format!(
                    "artifact path {} has no parent directory",
                    path.display()
                ))
            })?
            .join(SCRIPT_IDENTITY_FILE);
        std::fs::write(sidecar, compute_script(project, path)?.render())?;
    }
    Ok(())
}

/// The JSON object of a sidecar of either kind, with its envelope version already checked.
fn parse_sidecar(text: &str, path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    let refuse = |what: String| Error::Usage(format!("{}: {what}", path.display()));
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| refuse(format!("not valid JSON ({e})")))?;
    let object = root
        .as_object()
        .ok_or_else(|| refuse("a Build Identity sidecar is a JSON object".to_string()))?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| refuse("no `version`".to_string()))?;
    if version != IDENTITY_VERSION {
        return Err(refuse(format!(
            "sidecar version {version}; this CLI reads version {IDENTITY_VERSION}. Rebuild \
             the Package with `lyracore packages build`, or update this checkout."
        )));
    }
    Ok(object.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    /// A checkout shaped so `compute` can read every input: a Package's Datascript source, the
    /// generated typings (plus a Base Snapshot inside them), the authoring library, and the three
    /// pinned toolchain files.
    fn checkout(tmp: &TempDir) -> ProjectLayout {
        let project = project(tmp);
        std::fs::create_dir_all(project.datascripts_src_dir().join("fire_nova")).unwrap();
        std::fs::write(
            project
                .datascripts_src_dir()
                .join("fire_nova")
                .join("spells.ts"),
            "// a Datascript\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.datascript_types_dir().join("types")).unwrap();
        std::fs::write(
            project
                .datascript_types_dir()
                .join("types")
                .join("spell.ts"),
            "export type Spell = { spellId: number };\n",
        )
        .unwrap();
        std::fs::write(project.base_snapshot_file(), "{\"spells\":[]}\n").unwrap();
        std::fs::create_dir_all(project.datascripts_lib_dir()).unwrap();
        std::fs::write(
            project.datascripts_lib_dir().join("index.ts"),
            "// the authoring library\n",
        )
        .unwrap();
        std::fs::write(project.datascripts_dir().join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(project.datascripts_dir().join("package.json"), "{}\n").unwrap();
        std::fs::write(project.datascripts_dir().join("bun.lock"), "{}\n").unwrap();
        project
    }

    fn package_dir_of(project: &ProjectLayout) -> PathBuf {
        project.packages_dir().join("fire_nova")
    }

    // ---- determinism ----

    #[test]
    fn the_same_fixture_tree_computes_byte_identical_sidecars_twice() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let (first, available_first) =
            compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        let (second, available_second) =
            compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert!(available_first && available_second);
        assert_eq!(first.render(), second.render());
    }

    #[test]
    fn the_rendered_sidecar_reads_back_as_the_same_identity() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (identity, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        assert!(available);

        let read_back = Identity::parse(&identity.render(), Path::new(IDENTITY_FILE))
            .expect("the rendered sidecar parses");

        assert_eq!(read_back, identity);
    }

    #[test]
    fn the_rendered_sidecar_has_no_whitespace_and_sorted_members() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (identity, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        let rendered = identity.render();
        assert!(!rendered.contains(' '), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
        let artifact_at = rendered.find("\"artifact_hash\"").unwrap();
        let version_at = rendered.find("\"version\"").unwrap();
        assert!(artifact_at < version_at, "{rendered}");
    }

    // ---- no absolute paths, no timestamps ----

    #[test]
    fn moving_the_checkout_produces_the_same_sidecar() {
        let first_tmp = TempDir::new().unwrap();
        let first = checkout(&first_tmp);
        let second_tmp = TempDir::new().unwrap();
        let second = checkout(&second_tmp);

        let (identity_one, _) = compute(&first, &package_dir_of(&first), HASH_A).unwrap();
        let (identity_two, _) = compute(&second, &package_dir_of(&second), HASH_A).unwrap();

        assert_eq!(identity_one.render(), identity_two.render());
    }

    // ---- each recorded input flips staleness independently ----

    #[test]
    fn a_changed_datascript_source_is_named_and_nothing_else_is() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        std::fs::write(
            project
                .datascripts_src_dir()
                .join("fire_nova")
                .join("spells.ts"),
            "// edited\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        let changed = recorded.changed_against(&current, available);
        assert_eq!(changed, vec![Input::Source], "{changed:?}");
    }

    #[test]
    fn a_changed_generated_typing_is_named() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        std::fs::write(
            project
                .datascript_types_dir()
                .join("types")
                .join("spell.ts"),
            "export type Spell = { spellId: number; cooldownMs: number };\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::Generated]
        );
    }

    #[test]
    fn a_changed_base_snapshot_is_named_and_does_not_also_flag_generated() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        std::fs::write(project.base_snapshot_file(), "{\"spells\":[{}]}\n").unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        // The snapshot lives inside `generated/`, but it is a separate recorded input — changing
        // it alone must not also report the typings as stale.
        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::Snapshot]
        );
    }

    #[test]
    fn a_changed_authoring_library_is_named() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        std::fs::write(
            project.datascripts_lib_dir().join("index.ts"),
            "// a different library\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::Lib]
        );
    }

    #[test]
    fn a_changed_tsconfig_package_json_or_lockfile_is_named() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        std::fs::write(
            project.datascripts_dir().join("tsconfig.json"),
            "{\"strict\":true}\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::Tsconfig]
        );

        std::fs::write(project.datascripts_dir().join("tsconfig.json"), "{}\n").unwrap();
        std::fs::write(
            project.datascripts_dir().join("package.json"),
            "{\"name\":\"datascripts\"}\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::PackageJson]
        );

        std::fs::write(project.datascripts_dir().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            project.datascripts_dir().join("bun.lock"),
            "{\"lockfileVersion\":1}\n",
        )
        .unwrap();
        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::BunLock]
        );
    }

    #[test]
    fn a_changed_bun_pin_is_named() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (mut recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();
        recorded.bun_version = "1.2.9".to_string();

        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::BunVersion]
        );
    }

    #[test]
    fn a_hand_edited_artifact_is_named() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        let (current, available) =
            compute(&project, &package_dir_of(&project), "a-different-hash").unwrap();

        assert_eq!(
            recorded.changed_against(&current, available),
            vec![Input::Artifact]
        );
    }

    // ---- the missing-snapshot contract ----

    #[test]
    fn a_missing_base_snapshot_reports_unavailable_rather_than_an_empty_hash_match() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::remove_file(project.base_snapshot_file()).unwrap();

        let (current, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert!(!available);
        assert_eq!(current.snapshot_hash, "");
    }

    #[test]
    fn changed_against_skips_the_snapshot_field_when_told_it_is_unverifiable() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let (recorded, _) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        // The recorded sidecar has a real snapshot hash; "current" has none because the local
        // snapshot is missing. Told the snapshot is unverifiable, that must not be reported.
        let mut current = recorded.clone();
        current.snapshot_hash = String::new();

        assert!(recorded.changed_against(&current, false).is_empty());
        assert_eq!(
            recorded.changed_against(&current, true),
            vec![Input::Snapshot],
            "told to verify, the same difference IS reported"
        );
    }

    // ---- missing directories hash as the empty tree rather than failing ----

    #[test]
    fn a_missing_authoring_library_computes_rather_than_failing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::remove_dir_all(project.datascripts_lib_dir()).unwrap();

        let (identity, available) = compute(&project, &package_dir_of(&project), HASH_A).unwrap();

        assert!(available);
        assert!(identity.lib_hash.starts_with("sha256-tree-v1:"));
    }

    // ---- package_dir ----

    #[test]
    fn package_dir_is_the_top_level_folder_under_the_enabled_inventory() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let artifact_path = project
            .packages_dir()
            .join("fire_nova")
            .join("data/.generated/spell.json");

        let dir = package_dir(&project, &artifact_path).unwrap();

        assert_eq!(dir, project.packages_dir().join("fire_nova"));
    }

    // ---- write / write_all ----

    fn artifact_json(package: &str) -> String {
        format!(
            r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#
        )
    }

    #[test]
    fn write_places_the_sidecar_next_to_the_artifact_after_it_validates() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let generated_dir = project
            .packages_dir()
            .join("fire_nova")
            .join("data/.generated");
        std::fs::create_dir_all(&generated_dir).unwrap();
        std::fs::write(generated_dir.join("spell.json"), artifact_json("fire_nova")).unwrap();
        let artifacts = super::super::artifact::read_enabled(&project.packages_dir())
            .unwrap()
            .deltas;

        write_all(&project, &artifacts).unwrap();

        let sidecar = generated_dir.join(IDENTITY_FILE);
        assert!(sidecar.is_file());
        let identity =
            Identity::parse(&std::fs::read_to_string(&sidecar).unwrap(), &sidecar).unwrap();
        assert_eq!(identity.artifact_hash, artifacts[0].artifact_hash);
    }

    #[test]
    fn writing_without_a_base_snapshot_is_refused_as_an_internal_invariant() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::remove_file(project.base_snapshot_file()).unwrap();
        let generated_dir = project
            .packages_dir()
            .join("fire_nova")
            .join("data/.generated");
        std::fs::create_dir_all(&generated_dir).unwrap();
        std::fs::write(generated_dir.join("spell.json"), artifact_json("fire_nova")).unwrap();
        let artifacts = super::super::artifact::read_enabled(&project.packages_dir())
            .unwrap()
            .deltas;

        let error = write_all(&project, &artifacts).unwrap_err();

        assert!(error.to_string().contains("unreachable"), "{error}");
        assert!(!generated_dir.join(IDENTITY_FILE).exists());
    }

    #[test]
    fn script_source_hash_matches_the_toolchains_shallow_inventory() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let scripts = project.package_scripts_dir("fire_nova");
        std::fs::create_dir_all(scripts.join("nested")).unwrap();
        std::fs::write(
            scripts.join("alpha.ts"),
            "// @event on_login\n// @id 100201\nfunction script(): void {}\n",
        )
        .unwrap();
        std::fs::write(
            scripts.join("zeta.lua"),
            "-- @event on_login\n-- @id 100202\nreturn 2\n",
        )
        .unwrap();
        std::fs::write(scripts.join("README.md"), "ignored\n").unwrap();
        std::fs::write(scripts.join("nested/hidden.ts"), "ignored\n").unwrap();
        std::fs::create_dir_all(project.runtime_scripts_dir()).unwrap();
        std::fs::write(project.script_builder_file(), "// builder\n").unwrap();
        let artifact = project
            .packages_dir()
            .join("fire_nova/data/.generated/fire_nova.script.json");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, "{}\n").unwrap();

        let identity = compute_script(&project, &artifact).unwrap();

        assert_eq!(
            identity.source_hash,
            "sha256-script-sources-v1:8395ead00aad341a7daa23658447385da94dabf6932b406cbfbdb5e2fd664002"
        );
    }
}
