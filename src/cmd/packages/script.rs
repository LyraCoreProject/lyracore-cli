//! The enabled Packages' Script Artifacts: the Runtime Scripts they ship, the digest a Shard
//! records for each one, and the payload `apply_package_deltas` reads for the script Import Family.
//!
//! A Package Delta states COLUMNS of rows a base import owns, so [`super::artifact`] has to merge
//! two Packages that meet on one row. A Runtime Script has no base import behind it and no other
//! owner: the Package that ships a script ships all of it. Two Packages meeting on one `script_id`
//! or one name is therefore never a merge, only a collision a human has to settle.
//!
//! # Why the canonical form is rebuilt here
//!
//! Same reason [`super::artifact`] rebuilds the Package Delta's, and on the same terms: the module
//! records `game_package_import.artifact_hash` as BLAKE3 over an artifact's canonical bytes, so
//! comparing a Shard's recorded digest against this checkout means reproducing those bytes exactly.
//! The rules are fixed and small — no whitespace, a declared member order, scripts sorted by
//! identifier, and JSON's own string escapes — and the fixtures in this module's tests are taken
//! verbatim from the engine crate's own canonical-form tests, so a drift shows up as a failing test.
//!
//! # What this parser refuses, and what it leaves to the module
//!
//! It refuses what would make the canonical bytes wrong or the plan unapplyable: an unknown version,
//! a member no artifact declares, and one Package shipping two scripts at one identifier or under
//! one name.
//!
//! It does NOT re-check the event catalogue, the Package script identifier band, the name character
//! rules or the empty-source rule. Those are the Module's, held once in the engine's artifact crate
//! and asserted against the Module's own dispatch. A copy here could only drift into refusing a
//! Package the Module accepts, and the Module refuses such a plan on the first Shard before it
//! writes anything.

use std::path::{Path, PathBuf};

use super::artifact::write_string;
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
/// Read before either parse, not after: each parser reports what is wrong with its OWN kind, and a
/// file read by the wrong one describes a symptom rather than the mistake. Anything that is not a
/// JSON object with `"kind": "script"` goes to the Package Delta parser, which reports what is
/// wrong with it far better than this can.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
