//! The enabled Packages' Script Artifacts: the Runtime Scripts they ship, the digest a Shard
//! records for each one, and the payload `apply_package_deltas` reads for the script Import Family.
//! The same module builds them: `packages build` compiles a Package's `scripts/` sources into the
//! artifact this parser then reads back.
//!
//! A Runtime Script has no base import and no other owner, so two Packages meeting on one
//! `script_id` or name is always a collision, never the row merge [`super::artifact`] does.
//!
//! The canonical form mirrors [`super::artifact`]'s, for the same reason: the module hashes it into
//! `artifact_hash`, so this module's fixtures are copied verbatim from the engine crate's own
//! canonical-form tests, and a drift shows up as a failing test here too.
//!
//! This parser refuses only what would corrupt the bytes or the plan — unknown version, an
//! undeclared member, an in-Package id/name clash — and leaves the event catalogue, identifier
//! bands, and name rules to the Module, which refuses an over-permissive plan on the first Shard.
//!
//! The build half is the Datascript step's sibling and deliberately shaped like it: one `bun run`
//! subprocess per Package, streamed so the toolchain's own diagnostics reach the author, fail-fast,
//! and the artifact validated afterwards by the same `lyracore-delta-check` that traces Package
//! Deltas. Two things differ, both because a Runtime Script is Package-authored rather than
//! schema-derived:
//!
//!  * The sources live INSIDE the Package (`packages/<name>/scripts/`), not under `datascripts/src/`.
//!    A Datascript sits outside a Package folder because only artifacts belong in one; a Runtime
//!    Script is a Package's own content, and the Official Package Collection ships both halves.
//!  * The builder is handed the hook Event Binding catalogue, read from `lyracore-delta-check
//!    --print-events`, so a `@event` typo fails while the author can still see which file it is in.
//!    The catalogue is never copied into the toolchain: the Module owns it, and a copy would drift.

use std::path::{Path, PathBuf};

use super::artifact::write_string;
use crate::cmd::packages::identity;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// The Import Family a Script Artifact belongs to — the name `apply_package_deltas` takes and
/// `game_package_import.family` records.
///
/// Unlike every other family it has no base import: no DBC and no dump holds a Runtime Script, so
/// applying the enabled plan is the whole of this family's reload.
pub const SCRIPT_FAMILY: &str = "script";

/// The value a Script Artifact's `kind` member carries. A Package Delta carries no `kind` at all —
/// version 1 of it shipped before there was a second kind to tell it from.
pub const SCRIPT_ARTIFACT_KIND: &str = "script";

/// The only Script Artifact envelope version this CLI reads.
const SCRIPT_VERSION: u64 = 1;

/// One whole Runtime Script, as it will sit in `game_script`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub script_id: u32,
    pub name: String,
    pub event: String,
    priority: i32,
    enabled: bool,
    source: String,
}

/// One Package's Script Artifact, read and digested.
#[derive(Debug, Clone)]
pub struct ScriptArtifact {
    /// The Package identity the artifact carries — the `package` half of the Shard's provenance key.
    pub package: String,
    /// The file it came from, so a refusal can name it.
    pub path: PathBuf,
    /// BLAKE3 of the canonical bytes, in the spelling `game_package_import.artifact_hash` stores.
    pub artifact_hash: String,
    /// The canonical bytes themselves — one line of the payload the reducer reads.
    canonical: String,
    scripts: Vec<Script>,
}

impl ScriptArtifact {
    /// The scripts this Package ships, in canonical (identifier) order.
    #[must_use]
    pub fn scripts(&self) -> &[Script] {
        &self.scripts
    }
}

/// Whether these bytes are a Script Artifact, from the root `kind` member alone.
///
/// Read before either parse: each parser then reports what is wrong with its OWN kind, instead of
/// routing a file to the wrong parser and describing a symptom rather than the mistake.
pub fn is_script_artifact(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|root| {
            root.as_object()
                .and_then(|object| object.get("kind"))
                .and_then(serde_json::Value::as_str)
                .map(|kind| kind == SCRIPT_ARTIFACT_KIND)
        })
        .unwrap_or(false)
}

/// Read one Script Artifact and digest it. Every refusal names the file, because the operator's
/// next move is to open it.
pub fn parse(text: &str, path: &Path) -> Result<ScriptArtifact> {
    let refuse = |what: String| Error::Usage(format!("{}: {what}", path.display()));

    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| refuse(format!("not valid JSON ({e})")))?;
    let object = root
        .as_object()
        .ok_or_else(|| refuse("a Script Artifact is a JSON object".to_string()))?;
    expect_members(
        object,
        "",
        &["kind", "package", "scripts", "source_hash", "version"],
    )
    .map_err(&refuse)?;

    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| refuse("no `version`".to_string()))?;
    if version != SCRIPT_VERSION {
        return Err(refuse(format!(
            "Script Artifact version {version}; this CLI reads version {SCRIPT_VERSION}. Rebuild \
             the Package with `lyracore packages build`, or update this checkout."
        )));
    }

    let package = member_string(object, "package").ok_or_else(|| refuse("no `package`".into()))?;
    let source_hash =
        member_string(object, "source_hash").ok_or_else(|| refuse("no `source_hash`".into()))?;

    let raw_scripts = object
        .get("scripts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| refuse("no `scripts` array".to_string()))?;
    let mut scripts: Vec<Script> = Vec::with_capacity(raw_scripts.len());
    for (index, script) in raw_scripts.iter().enumerate() {
        scripts.push(parse_script(script, index).map_err(&refuse)?);
    }
    scripts.sort_by_key(|script| script.script_id);

    // A Package disagreeing with ITSELF is a Datascript defect rather than a decision between
    // Packages, so it is refused here and never reaches the collision report.
    for pair in scripts.windows(2) {
        if pair[0].script_id == pair[1].script_id {
            return Err(refuse(format!(
                "two scripts share identifier {}",
                pair[0].script_id
            )));
        }
    }
    let mut names: Vec<&str> = scripts.iter().map(|script| script.name.as_str()).collect();
    names.sort_unstable();
    for pair in names.windows(2) {
        if pair[0] == pair[1] {
            return Err(refuse(format!("two scripts share the name `{}`", pair[0])));
        }
    }

    let canonical = canonical(&package, &source_hash, &scripts);
    Ok(ScriptArtifact {
        artifact_hash: blake3::hash(canonical.as_bytes()).to_hex().to_string(),
        package,
        path: path.to_path_buf(),
        canonical,
        scripts,
    })
}

fn parse_script(value: &serde_json::Value, index: usize) -> std::result::Result<Script, String> {
    let where_ = format!("scripts[{index}]");
    let object = value
        .as_object()
        .ok_or_else(|| format!("{where_} is not a JSON object"))?;
    expect_members(
        object,
        &where_,
        &[
            "enabled",
            "event",
            "name",
            "priority",
            "script_id",
            "source",
        ],
    )?;

    let member = |name: &str| {
        object
            .get(name)
            .ok_or_else(|| format!("{where_} has no `{name}`"))
    };
    let script_id = member("script_id")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("{where_}.script_id is not a whole number in 0..=4294967295"))?;
    let priority = member("priority")?
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| {
            format!("{where_}.priority is not a whole number in -2147483648..=2147483647")
        })?;
    let text = |name: &str| {
        member(name)?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{where_}.{name} is not a string"))
    };

    Ok(Script {
        script_id,
        name: text("name")?,
        event: text("event")?,
        priority,
        enabled: member("enabled")?
            .as_bool()
            .ok_or_else(|| format!("{where_}.enabled is not true or false"))?,
        source: text("source")?,
    })
}

/// Refuse a member no artifact of this version declares.
///
/// The module refuses the same member, so accepting it here would build canonical bytes that
/// silently drop it and send a plan the first Shard rejects. Preflight is where that belongs.
fn expect_members(
    object: &serde_json::Map<String, serde_json::Value>,
    where_: &str,
    declared: &[&str],
) -> std::result::Result<(), String> {
    for name in object.keys() {
        if !declared.contains(&name.as_str()) {
            let at = if where_.is_empty() {
                "a Script Artifact".to_string()
            } else {
                where_.to_string()
            };
            return Err(format!(
                "{at} has no member `{name}`; it declares {}",
                declared.join(", ")
            ));
        }
    }
    Ok(())
}

fn member_string(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Option<String> {
    object
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The artifact's canonical bytes — the exact input the module hashes into `artifact_hash` and the
/// exact line it reads back out of the payload.
fn canonical(package: &str, source_hash: &str, scripts: &[Script]) -> String {
    let mut out = String::new();
    out.push_str("{\"kind\":");
    write_string(&mut out, SCRIPT_ARTIFACT_KIND);
    out.push_str(",\"version\":");
    out.push_str(&SCRIPT_VERSION.to_string());
    out.push_str(",\"package\":");
    write_string(&mut out, package);
    out.push_str(",\"source_hash\":");
    write_string(&mut out, source_hash);
    out.push_str(",\"scripts\":[");
    for (index, script) in scripts.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"script_id\":");
        out.push_str(&script.script_id.to_string());
        out.push_str(",\"name\":");
        write_string(&mut out, &script.name);
        out.push_str(",\"event\":");
        write_string(&mut out, &script.event);
        out.push_str(",\"priority\":");
        out.push_str(&script.priority.to_string());
        out.push_str(",\"enabled\":");
        out.push_str(if script.enabled { "true" } else { "false" });
        out.push_str(",\"source\":");
        write_string(&mut out, &script.source);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// Every collision between these Packages, worded the way the module words it.
///
/// A script is never merged with another: the whole row belongs to one Package. So the only thing
/// to trace is identity — an identifier or a name that two Packages both claim.
pub fn collisions(artifacts: &[ScriptArtifact]) -> Vec<String> {
    let mut by_id: Vec<(u32, &str)> = Vec::new();
    let mut by_name: Vec<(&str, &str)> = Vec::new();
    let mut found = Vec::new();

    for artifact in artifacts {
        for script in &artifact.scripts {
            if let Some((_, first)) = by_id.iter().find(|(id, _)| *id == script.script_id) {
                found.push(format!(
                    "script {} is shipped by both `{first}` and `{}`",
                    script.script_id, artifact.package
                ));
                continue;
            }
            if let Some((_, first)) = by_name.iter().find(|(name, _)| *name == script.name) {
                found.push(format!(
                    "Runtime Script name `{}` is shipped by both `{first}` and `{}`",
                    script.name, artifact.package
                ));
                continue;
            }
            by_id.push((script.script_id, &artifact.package));
            by_name.push((&script.name, &artifact.package));
        }
    }
    found
}

/// The artifacts' canonical bytes, one per line — the payload shape `apply_package_deltas` reads.
///
/// Packing nothing is an empty payload, which is a statement rather than an omission: "no enabled
/// Package ships a Runtime Script". The reducer reconciles the Shard to it and every Package script
/// leaves.
#[must_use]
pub fn pack(artifacts: &[ScriptArtifact]) -> String {
    artifacts
        .iter()
        .map(|artifact| artifact.canonical.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- building a Script Artifact ----

/// The environment variable the builder reads its Event Binding catalogue from, one event per line.
const HOOK_EVENTS_ENV: &str = "LYRACORE_HOOK_EVENTS";

/// The Runtime Script sources the builder reads, in file-name order.
///
/// The inventory is deliberately shallow and contains only regular `.ts` and `.lua` files. This
/// is the same definition as the Runtime Script Toolchain's `scriptSources`: a nested file, a
/// symlink, or a README cannot affect emitted Lua and therefore cannot affect its Build Identity.
pub fn source_files(project: &ProjectLayout, package: &str) -> Result<Vec<PathBuf>> {
    let scripts_dir = project.package_scripts_dir(package);
    if !scripts_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut sources = Vec::new();
    for entry in std::fs::read_dir(scripts_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "ts" || extension == "lua")
        {
            sources.push(path);
        }
    }
    sources.sort();
    Ok(sources)
}

/// Enabled Packages carrying at least one Runtime Script source, in folder-name order. The same
/// listing already means "enabled", so a disabled Package's scripts are not there to find.
pub fn packages_with_scripts(project: &ProjectLayout) -> Result<Vec<String>> {
    let packages_dir = project.packages_dir();
    if !packages_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&packages_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !source_files(project, &name)?.is_empty() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Remove Script Artifacts previously emitted by this build after their last source disappears.
///
/// A `script.identity` sidecar marks the source-built mode. A source-free Script Artifact without
/// that sidecar is prebuilt Lua and remains untouched, so an Operator can install and replay it
/// without Bun. Artifact files are removed before the sidecar. If removal fails, the remaining
/// sidecar keeps any surviving artifact from being mistaken for prebuilt Lua by `packages check`.
pub fn remove_artifacts_without_sources(project: &ProjectLayout) -> Result<Vec<PathBuf>> {
    let packages_dir = project.packages_dir();
    if !packages_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut removed = Vec::new();
    for entry in std::fs::read_dir(packages_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(package) = entry.file_name().into_string() else {
            continue;
        };
        if !source_files(project, &package)?.is_empty() {
            continue;
        }

        let generated = entry.path().join("data/.generated");
        let sidecar = generated.join(identity::SCRIPT_IDENTITY_FILE);
        if !sidecar.is_file() {
            continue;
        }

        if generated.is_dir() {
            let mut files = Vec::new();
            for candidate in std::fs::read_dir(&generated)? {
                let candidate = candidate?;
                if candidate.file_type()?.is_file()
                    && candidate
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                {
                    files.push(candidate.path());
                }
            }
            files.sort();
            for path in files {
                let text = std::fs::read_to_string(&path)?;
                if is_script_artifact(&text) {
                    std::fs::remove_file(&path)?;
                    removed.push(path);
                }
            }
        }
        std::fs::remove_file(sidecar)?;
    }
    Ok(removed)
}

/// Print the Module's Event Binding catalogue. Captured, not streamed: the answer is the output.
pub fn hook_events_command(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("cargo")
        .arg("run")
        .arg("-p")
        .arg("lyracore-package-delta")
        .arg("--bin")
        .arg("lyracore-delta-check")
        .arg("--")
        .arg("--print-events")
        .cwd(project.root.clone())
}

/// One Package's Runtime Script build, as a subprocess.
pub fn build_command(project: &ProjectLayout, package: &str, events: &str) -> CommandSpec {
    CommandSpec::new("bun")
        .arg("run")
        .arg(project.script_builder_file().to_string_lossy().to_string())
        .arg(package.to_string())
        .env(
            "LYRACORE_PACKAGES_ROOT",
            project.packages_dir().to_string_lossy().to_string(),
        )
        .env(HOOK_EVENTS_ENV, events.to_string())
        .cwd(project.root.clone())
}

/// Read the Event Binding catalogue once, then compile each Package's scripts in folder-name order.
/// The first Package to fail stops the build; later Packages never run.
pub fn run_builds(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    packages: &[String],
) -> Result<()> {
    let events = runner
        .run_and_wait(&hook_events_command(project))
        .map_err(|e| {
            Error::Process(format!(
                "could not read the Event Binding catalogue from `lyracore-delta-check \
                 --print-events`, so a script's `@event` could not be checked against the Module. \
                 Nothing was compiled.\n  ({e})"
            ))
        })?;

    for package in packages {
        println!(
            "compiling Runtime Scripts in {} ({package})",
            project.package_scripts_dir(package).display()
        );
        runner
            .run_streaming(&build_command(project, package, &events))
            .map_err(|e| {
                Error::Process(format!(
                    "the Runtime Scripts of `{package}` did not compile into a Script Artifact. \
                     The toolchain's own diagnostic is above, naming the file and the directive or \
                     line to fix. A refused build writes nothing, so the Package's artifact is \
                     exactly what it was before this run.\n  ({e})"
                ))
            })?;
    }
    Ok(())
}

/// Verify every Script Artifact against its recorded Build Identity, the way `packages check` does
/// for Package Deltas. Returns one problem line per stale artifact.
pub fn stale(project: &ProjectLayout, artifacts: &[PathBuf]) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for path in artifacts {
        let package_dir = identity::package_dir(project, path)?;
        let package = package_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                Error::State(format!(
                    "Package directory {} has no UTF-8 folder name",
                    package_dir.display()
                ))
            })?;
        let has_sources = !source_files(project, package)?.is_empty();
        let sidecar = path.with_file_name(identity::SCRIPT_IDENTITY_FILE);
        let Ok(text) = std::fs::read_to_string(&sidecar) else {
            if !has_sources {
                // Source-free Script Artifacts are published prebuilt. They carry no Build
                // Identity because this checkout has no source tree to compare against.
                continue;
            }
            problems.push(format!(
                "{}: no Build Identity sidecar (predates identity tracking, or was removed). \
                 Rebuild with `lyracore packages build`.",
                path.display()
            ));
            continue;
        };
        if !has_sources {
            problems.push(format!(
                "{}: its Runtime Script sources were removed, but a source-built Script Artifact \
                 remains. Run `lyracore packages build` to remove it before replay.",
                path.display()
            ));
            continue;
        }
        let recorded = identity::ScriptIdentity::parse(&text, &sidecar)?;
        let current = identity::compute_script(project, path)?;
        for input in recorded.changed_against(&current) {
            problems.push(format!(
                "{}: {} changed. Rebuild with `lyracore packages build`.",
                path.display(),
                input.description()
            ));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn script_json(
        script_id: u32,
        name: &str,
        event: &str,
        priority: i32,
        enabled: bool,
    ) -> String {
        format!(
            r#"{{"script_id":{script_id},"name":"{name}","event":"{event}","priority":{priority},"enabled":{enabled},"source":"grant_xp(event.actor, 10)"}}"#
        )
    }

    fn artifact_json(package: &str, scripts: &[String]) -> String {
        format!(
            r#"{{"kind":"script","version":1,"package":"{package}","source_hash":"{HASH_A}","scripts":[{}]}}"#,
            scripts.join(",")
        )
    }

    fn read(text: &str) -> ScriptArtifact {
        parse(text, Path::new("script.json")).expect("artifact parses")
    }

    // ---- canonical bytes: fixtures taken verbatim from the engine crate ----

    /// From the engine crate's `the_canonical_form_has_a_fixed_member_order_and_no_whitespace`.
    /// This is the anchor: the expected bytes come from the crate that owns the format.
    #[test]
    fn the_canonical_form_has_a_fixed_member_order_and_no_whitespace() {
        let artifact = read(&artifact_json(
            "example.bolt",
            &[script_json(100_001, "bolt.greet", "on_login", 3, true)],
        ));

        assert_eq!(
            artifact.canonical,
            concat!(
                r#"{"kind":"script","version":1,"package":"example.bolt","#,
                r#""source_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
                r#""scripts":[{"script_id":100001,"name":"bolt.greet","event":"on_login","#,
                r#""priority":3,"enabled":true,"source":"grant_xp(event.actor, 10)"}]}"#,
            )
        );
    }

    /// Also from the engine crate: member order and whitespace are spellings, not content.
    #[test]
    fn how_the_artifact_was_written_cannot_change_its_canonical_bytes() {
        let compact = artifact_json(
            "example.bolt",
            &[script_json(100_001, "bolt.greet", "on_login", 0, true)],
        );
        let spelled_differently = format!(
            r#"{{
                "version" : 1,
                "scripts" : [ {{
                    "source"    : "grant_xp(event.actor, 10)",
                    "enabled"   : true,
                    "priority"  : 0,
                    "event"     : "on_login",
                    "name"      : "bolt.greet",
                    "script_id" : 100001
                }} ],
                "source_hash" : "{HASH_A}",
                "package" : "example.bolt",
                "kind" : "script"
            }}"#
        );

        assert_eq!(
            read(&compact).canonical,
            read(&spelled_differently).canonical
        );
        assert_eq!(
            read(&compact).artifact_hash,
            read(&spelled_differently).artifact_hash
        );
    }

    /// The payload packs one artifact per LINE, which only works because a canonical artifact
    /// escapes every control character. Lua source is the member most likely to hold a newline.
    #[test]
    fn a_canonical_artifact_never_contains_a_raw_newline_however_the_lua_is_written() {
        let json = format!(
            r#"{{"kind":"script","version":1,"package":"example.bolt","source_hash":"{HASH_A}","scripts":[{{"script_id":100001,"name":"bolt.multi","event":"on_login","priority":0,"enabled":true,"source":"local n = 1\nif n > 0 then\n\tgrant_xp(event.actor, n)\nend"}}]}}"#
        );

        let canonical = read(&json).canonical;

        assert!(!canonical.contains('\n'), "{canonical}");
        assert!(!canonical.contains('\t'), "{canonical}");
        assert!(
            canonical.contains(r"\n"),
            "the newline is escaped: {canonical}"
        );
    }

    #[test]
    fn scripts_are_written_in_identifier_order_however_they_were_listed() {
        let artifact = read(&artifact_json(
            "example.bolt",
            &[
                script_json(100_002, "bolt.b", "on_kill", -1, false),
                script_json(100_001, "bolt.a", "on_login", 7, true),
            ],
        ));

        let ids: Vec<u32> = artifact.scripts().iter().map(|s| s.script_id).collect();
        assert_eq!(ids, [100_001, 100_002]);
        assert!(
            artifact.canonical.find("bolt.a").unwrap() < artifact.canonical.find("bolt.b").unwrap(),
            "{}",
            artifact.canonical
        );
    }

    /// A negative priority is a spelling the canonical form has to keep, not normalize away.
    #[test]
    fn a_negative_priority_survives_the_canonical_form() {
        let artifact = read(&artifact_json(
            "example.bolt",
            &[script_json(100_001, "bolt.late", "on_login", -5, true)],
        ));

        assert!(
            artifact.canonical.contains(r#""priority":-5"#),
            "{}",
            artifact.canonical
        );
    }

    // ---- refusals ----

    #[test]
    fn an_unknown_version_is_refused_and_the_refusal_names_the_file() {
        let refusal = parse(
            &format!(
                r#"{{"kind":"script","version":9,"package":"p","source_hash":"{HASH_A}","scripts":[]}}"#
            ),
            Path::new("script.json"),
        )
        .expect_err("refused");

        assert!(refusal.to_string().contains("script.json"), "{refusal}");
        assert!(refusal.to_string().contains("version 9"), "{refusal}");
    }

    /// The module refuses an undeclared member, so accepting it here would build canonical bytes
    /// that silently drop it and send a plan the first Shard rejects.
    #[test]
    fn a_member_no_artifact_declares_is_refused_before_any_shard_is_read() {
        let refusal = parse(
            &format!(
                r#"{{"kind":"script","version":1,"package":"p","source_hash":"{HASH_A}","scripts":[],"notes":"hi"}}"#
            ),
            Path::new("script.json"),
        )
        .expect_err("refused");

        assert!(refusal.to_string().contains("`notes`"), "{refusal}");
    }

    #[test]
    fn one_package_shipping_two_scripts_at_one_identifier_is_refused() {
        let refusal = parse(
            &artifact_json(
                "example.bolt",
                &[
                    script_json(100_001, "bolt.a", "on_login", 0, true),
                    script_json(100_001, "bolt.b", "on_login", 0, true),
                ],
            ),
            Path::new("script.json"),
        )
        .expect_err("refused");

        assert!(refusal.to_string().contains("100001"), "{refusal}");
    }

    #[test]
    fn one_package_shipping_two_scripts_under_one_name_is_refused() {
        let refusal = parse(
            &artifact_json(
                "example.bolt",
                &[
                    script_json(100_001, "bolt.greet", "on_login", 0, true),
                    script_json(100_002, "bolt.greet", "on_kill", 0, true),
                ],
            ),
            Path::new("script.json"),
        )
        .expect_err("refused");

        assert!(refusal.to_string().contains("bolt.greet"), "{refusal}");
    }

    // ---- collisions between Packages ----

    #[test]
    fn two_packages_shipping_one_identifier_collide_and_the_report_names_both() {
        let artifacts = vec![
            read(&artifact_json(
                "example.first",
                &[script_json(100_001, "first.greet", "on_login", 0, true)],
            )),
            read(&artifact_json(
                "example.second",
                &[script_json(100_001, "second.greet", "on_login", 0, true)],
            )),
        ];

        let found = collisions(&artifacts);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("example.first"), "{found:?}");
        assert!(found[0].contains("example.second"), "{found:?}");
        assert!(found[0].contains("script 100001"), "{found:?}");
    }

    #[test]
    fn two_packages_shipping_one_name_collide() {
        let artifacts = vec![
            read(&artifact_json(
                "example.first",
                &[script_json(100_001, "shared.greet", "on_login", 0, true)],
            )),
            read(&artifact_json(
                "example.second",
                &[script_json(100_002, "shared.greet", "on_login", 0, true)],
            )),
        ];

        let found = collisions(&artifacts);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("shared.greet"), "{found:?}");
    }

    #[test]
    fn two_packages_shipping_different_scripts_do_not_collide() {
        let artifacts = vec![
            read(&artifact_json(
                "example.first",
                &[script_json(100_001, "first.greet", "on_login", 0, true)],
            )),
            read(&artifact_json(
                "example.second",
                &[script_json(100_002, "second.greet", "on_login", 0, true)],
            )),
        ];

        assert!(collisions(&artifacts).is_empty());
    }

    // ---- the payload ----

    #[test]
    fn the_payload_carries_one_canonical_artifact_per_line() {
        let artifacts = vec![
            read(&artifact_json(
                "example.alpha",
                &[script_json(100_001, "alpha.greet", "on_login", 0, true)],
            )),
            read(&artifact_json(
                "example.zeta",
                &[script_json(100_002, "zeta.greet", "on_login", 0, true)],
            )),
        ];

        let packed = pack(&artifacts);

        let lines: Vec<&str> = packed.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], artifacts[0].canonical);
        assert_eq!(lines[1], artifacts[1].canonical);
    }

    /// The empty plan is the statement that removes a disabled Package's scripts from every Shard,
    /// so it must be spellable.
    #[test]
    fn packing_nothing_is_an_empty_payload() {
        assert_eq!(pack(&[]), "");
    }

    // ---- kind routing ----

    #[test]
    fn only_a_script_kind_is_recognized_as_a_script_artifact() {
        assert!(is_script_artifact(&artifact_json("p", &[])));
        assert!(!is_script_artifact(r#"{"version":1,"claims":[]}"#));
        assert!(!is_script_artifact(r#"{"kind":"weather","version":1}"#));
        assert!(!is_script_artifact("{ not even valid }"));
    }

    // ---- the build half ----

    fn checkout(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        std::fs::create_dir_all(project.runtime_scripts_dir()).unwrap();
        std::fs::write(project.script_builder_file(), "// the builder\n").unwrap();
        std::fs::write(project.datascripts_dir().join("bun.lock"), "{}\n").unwrap();
        project
    }

    fn with_scripts(project: &ProjectLayout, package: &str, file: &str) {
        let dir = project.package_scripts_dir(package);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), "// @event on_login\n// @id 100200\n").unwrap();
    }

    /// The Script Artifact a build would have written, plus its sidecar.
    fn with_built_artifact(project: &ProjectLayout, package: &str) -> PathBuf {
        let dir = project.packages_dir().join(package).join("data/.generated");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{package}.script.json"));
        std::fs::write(&path, "{\"kind\":\"script\"}\n").unwrap();
        identity::write_script_identities(project, std::slice::from_ref(&path)).unwrap();
        path
    }

    #[test]
    fn only_enabled_packages_carrying_a_scripts_folder_are_built() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "zeta", "a.ts");
        with_scripts(&project, "alpha", "a.ts");
        std::fs::create_dir_all(project.packages_dir().join("rust_only")).unwrap();
        let notes = project.package_scripts_dir("notes_only");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("README.md"), "not executable\n").unwrap();
        std::fs::create_dir_all(notes.join("nested.ts")).unwrap();

        assert_eq!(packages_with_scripts(&project).unwrap(), ["alpha", "zeta"]);
    }

    #[test]
    fn the_source_inventory_is_shallow_and_contains_only_regular_typescript_and_lua_files() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let scripts = project.package_scripts_dir("fire_nova");
        std::fs::create_dir_all(scripts.join("nested")).unwrap();
        std::fs::write(scripts.join("alpha.ts"), "one\n").unwrap();
        std::fs::write(scripts.join("zeta.lua"), "two\n").unwrap();
        std::fs::write(scripts.join("README.md"), "notes\n").unwrap();
        std::fs::write(scripts.join("nested/hidden.ts"), "ignored\n").unwrap();

        let relative = source_files(&project, "fire_nova")
            .unwrap()
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(relative, ["alpha.ts", "zeta.lua"]);
    }

    #[test]
    fn the_builder_subprocess_carries_the_packages_root_and_the_event_catalogue() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let stack = FakeStack::new().with_stdout("--print-events", "on_login\non_death\n");

        run_builds(&project, &stack.runner(), &["fire_nova".to_string()]).unwrap();

        let spec = stack
            .calls()
            .into_iter()
            .find_map(|call| match call {
                crate::proc::fake::Call::Stream(spec) if spec.render().starts_with("bun run") => {
                    Some(spec)
                }
                _ => None,
            })
            .expect("the builder ran");
        assert!(spec.render().ends_with("fire_nova"), "{}", spec.render());
        assert_eq!(
            spec.env_value("LYRACORE_PACKAGES_ROOT"),
            Some(project.packages_dir().to_string_lossy().as_ref())
        );
        assert_eq!(
            spec.env_value(HOOK_EVENTS_ENV),
            Some("on_login\non_death\n")
        );
    }

    #[test]
    fn a_catalogue_that_cannot_be_read_stops_the_build_before_any_package_compiles() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new().fail_on("--print-events", "the delta crate does not compile");

        let error = run_builds(&project, &stack.runner(), &["fire_nova".to_string()]).unwrap_err();

        assert!(error.to_string().contains("Event Binding"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("bun run"), "{call}");
        }
    }

    #[test]
    fn a_mid_loop_failure_stops_the_build_and_a_later_package_never_compiles() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let stack = FakeStack::new()
            .with_stdout("--print-events", "on_login\n")
            .fail_on("build-scripts.ts alpha", "the script does not compile");

        let error = run_builds(
            &project,
            &stack.runner(),
            &["alpha".to_string(), "zeta".to_string()],
        )
        .unwrap_err();

        assert!(error.to_string().contains("alpha"), "{error}");
        for call in stack.rendered() {
            assert!(!call.contains("zeta"), "{call}");
        }
    }

    // ---- staleness ----

    #[test]
    fn a_freshly_built_script_artifact_is_current() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");

        assert!(stale(&project, &[path]).unwrap().is_empty());
    }

    #[test]
    fn an_edited_script_source_is_stale_and_names_that_input() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");

        std::fs::write(
            project
                .package_scripts_dir("fire_nova")
                .join("ember_echo.ts"),
            "// @event on_death\n// @id 100200\n",
        )
        .unwrap();

        let problems = stale(&project, &[path]).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("Runtime Script sources"),
            "{problems:?}"
        );
    }

    #[test]
    fn an_edited_toolchain_is_stale_and_an_installed_node_modules_is_not() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");

        // An install under the toolchain is not an input: `bun.lock` is the pin for what it holds.
        let modules = project.runtime_scripts_dir().join("node_modules/whatever");
        std::fs::create_dir_all(&modules).unwrap();
        std::fs::write(modules.join("index.js"), "module.exports = {};\n").unwrap();
        assert!(stale(&project, std::slice::from_ref(&path))
            .unwrap()
            .is_empty());

        std::fs::write(project.script_builder_file(), "// a different builder\n").unwrap();

        let problems = stale(&project, &[path]).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("toolchain"), "{problems:?}");
    }

    #[test]
    fn a_hand_edited_artifact_is_stale() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");
        std::fs::write(&path, "{\"kind\":\"script\",\"edited\":true}\n").unwrap();

        let problems = stale(&project, &[path]).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("hand-edited"), "{problems:?}");
    }

    #[test]
    fn a_missing_sidecar_is_stale_and_names_the_rebuild_command() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");
        std::fs::remove_file(path.with_file_name(identity::SCRIPT_IDENTITY_FILE)).unwrap();

        let problems = stale(&project, &[path]).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("lyracore packages build"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_source_free_prebuilt_artifact_needs_no_build_identity() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "playerbots", "personality.lua");
        let path = with_built_artifact(&project, "playerbots");
        std::fs::remove_dir_all(project.package_scripts_dir("playerbots")).unwrap();
        std::fs::remove_file(path.with_file_name(identity::SCRIPT_IDENTITY_FILE)).unwrap();

        assert!(stale(&project, &[path]).unwrap().is_empty());
    }

    #[test]
    fn a_source_built_artifact_without_its_sources_is_stale_even_if_its_identity_matches() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        with_scripts(&project, "fire_nova", "ember_echo.ts");
        let path = with_built_artifact(&project, "fire_nova");
        std::fs::remove_dir_all(project.package_scripts_dir("fire_nova")).unwrap();
        let incorrectly_recertified = identity::compute_script(&project, &path).unwrap();
        std::fs::write(
            path.with_file_name(identity::SCRIPT_IDENTITY_FILE),
            incorrectly_recertified.render(),
        )
        .unwrap();

        let problems = stale(&project, &[path]).unwrap();

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("sources were removed"), "{problems:?}");
    }
}
