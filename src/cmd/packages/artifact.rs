//! The enabled Packages' generated Delta artifacts: what they claim, what they disagree about, and
//! the digest the Shard records for each one.
//!
//! `packages replay` has to answer two questions before it writes to the first Shard: is this set of
//! artifacts applicable at all, and does a Shard already hold exactly this set. Both are answered
//! here, from the working tree, once per run.
//!
//! # Why the canonical form is rebuilt here
//!
//! The module records `game_package_import.artifact_hash` as BLAKE3 over an artifact's CANONICAL
//! bytes, not over the file as written — so two artifacts that say the same thing hash the same
//! however they were spelled. Comparing a Shard's recorded digest against this checkout therefore
//! means reproducing that canonical form exactly. The rules are fixed and small, and they are
//! restated here rather than imported because the crate that owns them lives in the engine
//! repository, which this CLI does not build against:
//!
//!  * No whitespace anywhere, and no trailing newline.
//!  * Members appear in a fixed declared order; `fields` members appear sorted by name.
//!  * Claims appear sorted by table, then spell, then effect index.
//!  * Integers are plain decimal, so `1e2`, `100.0` and `100` all become `100`.
//!  * An unsigned 64-bit value is a decimal string with no sign, no padding and no separators.
//!  * A float is the shortest decimal that reads back as the same `f32`, always with a decimal
//!    point.
//!  * A string escapes only what JSON requires, using the short escape where one exists.
//!
//! A drift between the two spellings can only make a digest MISMATCH, never falsely match, so its
//! worst outcome is a Shard that reapplies work it already holds. The fixtures in this module's
//! tests are taken verbatim from the engine crate's own canonical-form tests, so a drift shows up
//! as a failing test rather than as a resume that silently never skips.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Where a Package's generated Delta artifacts live, relative to the Package folder.
const GENERATED_DIR: &str = "data/.generated";

/// The only artifact envelope version this CLI reads.
const DELTA_VERSION: u64 = 1;

/// The `kind` a Script Artifact carries. A Package Delta carries no `kind` at all — version 1 of it
/// shipped before there was a second kind to tell it from — so an absent member means a Delta.
const SCRIPT_ARTIFACT_KIND: &str = "script";

/// The Import Family the Package Delta schema covers.
pub const SPELL_FAMILY: &str = "spell";

/// The claimable tables, in the order the canonical form sorts them.
const TABLE_SPELL: &str = "game_spell";
const TABLE_SPELL_EFFECT: &str = "game_spell_effect";

/// Which row a claim addresses. Ordered the way the canonical form sorts claims: `game_spell`
/// before `game_spell_effect`, then by spell, then by effect index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Spell { spell_id: u64 },
    SpellEffect { spell_id: u64, effect_index: u64 },
}

impl Key {
    const fn table(self) -> &'static str {
        match self {
            Self::Spell { .. } => TABLE_SPELL,
            Self::SpellEffect { .. } => TABLE_SPELL_EFFECT,
        }
    }

    fn render(self) -> String {
        match self {
            Self::Spell { spell_id } => format!("{{spell_id={spell_id}}}"),
            Self::SpellEffect {
                spell_id,
                effect_index,
            } => format!("{{spell_id={spell_id},effect_index={effect_index}}}"),
        }
    }

    fn write_canonical(self, out: &mut String) {
        match self {
            Self::Spell { spell_id } => {
                let _ = write!(out, "{{\"spell_id\":{spell_id}}}");
            }
            Self::SpellEffect {
                spell_id,
                effect_index,
            } => {
                let _ = write!(
                    out,
                    "{{\"spell_id\":{spell_id},\"effect_index\":{effect_index}}}"
                );
            }
        }
    }
}

/// One claimed column: its declared type and its value in canonical spelling.
///
/// The literal is normalized at parse time rather than at write time, so a padded `"0042"` and a
/// `1.50` are already the bytes the digest covers by the time anything compares them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claimed {
    tag: &'static str,
    literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    key: Key,
    inserts: bool,
    fields: BTreeMap<String, Claimed>,
}

/// One Package's generated Delta artifact, read and digested.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// The Package identity the artifact carries — the `package` half of the Shard's provenance key.
    pub package: String,
    /// The file it came from, so a refusal can name it.
    pub path: PathBuf,
    /// The digest of the Datascript source, carried verbatim from the artifact.
    pub source_hash: String,
    /// BLAKE3 of the canonical bytes, in the spelling `game_package_import.artifact_hash` stores.
    pub artifact_hash: String,
    /// Rows this Package changes but does not own.
    pub updated_rows: u64,
    /// Rows this Package invents.
    pub inserted_rows: u64,
    claims: Vec<Claim>,
}

/// What one walk of the enabled Package Inventory found.
///
/// A Package ships every artifact kind it has into one `data/.generated/` directory, so the walk
/// meets Script Artifacts as well as Package Deltas. They are a different artifact with a different
/// applier, and this reader has nothing to say about them beyond how many it passed over.
#[derive(Debug, Clone, Default)]
pub struct Enabled {
    /// Every Package Delta, ordered by Package folder then file name.
    pub deltas: Vec<Artifact>,
    /// How many Script Artifacts were passed over, for a report to name once.
    pub scripts_skipped: usize,
}

impl Enabled {
    /// The one line a report prints about the Script Artifacts it passed over, or nothing when
    /// there were none.
    #[must_use]
    pub fn skipped_note(&self) -> Option<String> {
        match self.scripts_skipped {
            0 => None,
            1 => Some("1 Script Artifact skipped: not a Package Delta".to_string()),
            n => Some(format!("{n} Script Artifacts skipped: not Package Deltas")),
        }
    }
}

/// Every enabled Package's artifacts, ordered by Package folder then file name so the same tree
/// always produces the same plan.
///
/// A missing root is an error, never an empty set: "no Package claims this family" is a statement
/// the operator makes by pointing at a real Package Inventory that happens to hold no artifacts.
pub fn read_enabled(root: &Path) -> Result<Enabled> {
    if !root.is_dir() {
        return Err(Error::Usage(format!(
            "enabled packages root `{}` is not a directory — name the directory holding the \
             enabled Packages (normally `packages/`).",
            root.display()
        )));
    }

    let mut packages: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    packages.sort();

    let mut enabled = Enabled::default();
    for package in packages {
        let generated = package.join(GENERATED_DIR);
        if !generated.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&generated)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();

        for path in files {
            let text = std::fs::read_to_string(&path)?;
            if is_script_artifact(&text) {
                enabled.scripts_skipped += 1;
                continue;
            }
            let artifact = parse(&text, &path)?;
            if let Some(seen) = enabled
                .deltas
                .iter()
                .find(|a| a.package == artifact.package)
            {
                return Err(Error::Usage(format!(
                    "package `{}` appears twice in the enabled Package Inventory:\n  {}\n  {}\nThe \
                     module refuses a plan that names one Package twice, so nothing was applied.",
                    artifact.package,
                    seen.path.display(),
                    path.display()
                )));
            }
            enabled.deltas.push(artifact);
        }
    }
    Ok(enabled)
}

/// Whether these bytes are a Script Artifact, from the root `kind` member alone.
///
/// Read before the parse, not after: a Script Artifact hard-parsed as a Package Delta fails on a
/// missing `claims` member, which describes the symptom rather than the fact that this file was
/// never a Delta. Anything that is not a JSON object with `"kind": "script"` goes to the parser,
/// which reports what is wrong with it far better than this can.
fn is_script_artifact(text: &str) -> bool {
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

/// Every disagreement between these Packages, in canonical order.
///
/// Two Packages claiming different columns of one row merge. Claiming the same column, or inserting
/// the same row, is a conflict whether or not they agree on the value — there are no priority
/// numbers, and this reports what a human has to resolve rather than picking a winner.
pub fn conflicts(artifacts: &[Artifact]) -> Vec<String> {
    let mut inserted_by: BTreeMap<Key, &str> = BTreeMap::new();
    let mut held: BTreeMap<(Key, &str), (&str, &str)> = BTreeMap::new();
    let mut found = Vec::new();

    for artifact in artifacts {
        for claim in &artifact.claims {
            if claim.inserts {
                match inserted_by.get(&claim.key) {
                    Some(holder) => found.push(format!(
                        "`{}` row {}: packages `{holder}` and `{}` both insert this row",
                        claim.key.table(),
                        claim.key.render(),
                        artifact.package
                    )),
                    None => {
                        inserted_by.insert(claim.key, &artifact.package);
                    }
                }
            }
            for (name, claimed) in &claim.fields {
                match held.get(&(claim.key, name.as_str())) {
                    Some((holder, holder_value)) => found.push(format!(
                        "`{}` row {}: packages `{holder}` and `{}` both claim column `{name}` \
                         (`{holder}` sets {holder_value}, `{}` sets {})",
                        claim.key.table(),
                        claim.key.render(),
                        artifact.package,
                        artifact.package,
                        claimed.literal
                    )),
                    None => {
                        held.insert(
                            (claim.key, name.as_str()),
                            (&artifact.package, &claimed.literal),
                        );
                    }
                }
            }
        }
    }
    found
}

/// Read one artifact and digest it. Every refusal names the file, because the operator's next move
/// is to open it.
fn parse(text: &str, path: &Path) -> Result<Artifact> {
    let refuse = |what: String| Error::Usage(format!("{}: {what}", path.display()));

    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|e| refuse(format!("not valid JSON ({e})")))?;
    let object = root
        .as_object()
        .ok_or_else(|| refuse("a Package Delta artifact is a JSON object".to_string()))?;

    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| refuse("no `version`".to_string()))?;
    if version != DELTA_VERSION {
        return Err(refuse(format!(
            "artifact version {version}; this CLI reads version {DELTA_VERSION}. Rebuild the \
             Package with `lyracore packages build`, or update this checkout."
        )));
    }

    let package = member_string(object, "package").ok_or_else(|| refuse("no `package`".into()))?;
    let source_hash =
        member_string(object, "source_hash").ok_or_else(|| refuse("no `source_hash`".into()))?;

    let raw_claims = object
        .get("claims")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| refuse("no `claims` array".to_string()))?;

    let mut claims: Vec<Claim> = Vec::with_capacity(raw_claims.len());
    for claim in raw_claims {
        claims.push(parse_claim(claim).map_err(refuse)?);
    }
    // The canonical form orders claims by key alone, and stably: two claims on one row keep the
    // order they were written in.
    claims.sort_by_key(|claim| claim.key);

    let inserted_rows = claims.iter().filter(|c| c.inserts).count() as u64;
    let artifact = Artifact {
        artifact_hash: blake3::hash(canonical(&package, &source_hash, &claims).as_bytes())
            .to_hex()
            .to_string(),
        package,
        path: path.to_path_buf(),
        source_hash,
        updated_rows: claims.len() as u64 - inserted_rows,
        inserted_rows,
        claims,
    };
    Ok(artifact)
}

fn parse_claim(value: &serde_json::Value) -> std::result::Result<Claim, String> {
    let object = value.as_object().ok_or("a claim is a JSON object")?;
    let table = object
        .get("table")
        .and_then(serde_json::Value::as_str)
        .ok_or("a claim needs a `table`")?;
    let operation = object
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .ok_or("a claim needs an `operation`")?;
    let inserts = match operation {
        "insert" => true,
        "update" => false,
        other => return Err(format!("unknown claim operation `{other}`")),
    };

    let key_object = object
        .get("key")
        .and_then(serde_json::Value::as_object)
        .ok_or("a claim needs a `key`")?;
    let spell_id = key_object
        .get("spell_id")
        .and_then(serde_json::Value::as_u64)
        .ok_or("a claim key needs a `spell_id`")?;
    let effect_index = key_object.get("effect_index").map(|index| {
        index
            .as_u64()
            .ok_or("`effect_index` is a whole number".to_string())
    });

    let key = match (table, effect_index) {
        (TABLE_SPELL, None) => Key::Spell { spell_id },
        (TABLE_SPELL_EFFECT, Some(index)) => Key::SpellEffect {
            spell_id,
            effect_index: index?,
        },
        (TABLE_SPELL, Some(_)) => {
            return Err(format!("a `{TABLE_SPELL}` key has no `effect_index`"))
        }
        (TABLE_SPELL_EFFECT, None) => {
            return Err(format!(
                "a `{TABLE_SPELL_EFFECT}` key needs an `effect_index`"
            ))
        }
        (other, _) => return Err(format!("unknown claim table `{other}`")),
    };

    let raw_fields = object
        .get("fields")
        .and_then(serde_json::Value::as_object)
        .ok_or("a claim needs a `fields` object")?;
    let mut fields = BTreeMap::new();
    for (name, raw) in raw_fields {
        fields.insert(
            name.clone(),
            parse_value(raw).map_err(|e| format!("column `{name}`: {e}"))?,
        );
    }

    Ok(Claim {
        key,
        inserts,
        fields,
    })
}

/// One claimed value, normalized to its canonical spelling.
fn parse_value(raw: &serde_json::Value) -> std::result::Result<Claimed, String> {
    let object = raw.as_object().ok_or("a claimed value is a JSON object")?;
    let tag = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or("a claimed value needs a `type`")?;
    let value = object
        .get("value")
        .ok_or("a claimed value needs a `value`")?;

    let (tag, literal) = match tag {
        "u8" | "u16" | "u32" => {
            let width: &'static str = match tag {
                "u8" => "u8",
                "u16" => "u16",
                _ => "u32",
            };
            (
                width,
                value
                    .as_u64()
                    .ok_or_else(|| format!("`{width}` takes a whole number"))?
                    .to_string(),
            )
        }
        "i32" => (
            "i32",
            value
                .as_i64()
                .ok_or("`i32` takes a whole number")?
                .to_string(),
        ),
        // A 64-bit value travels as a string, because JSON numbers cannot carry one intact.
        "u64" => {
            let text = value.as_str().ok_or("`u64` takes a decimal STRING")?;
            let parsed: u64 = text
                .parse()
                .map_err(|_| format!("`{text}` is not an unsigned 64-bit decimal"))?;
            ("u64", format!("\"{parsed}\""))
        }
        "f32" => {
            let number = value.as_f64().ok_or("`f32` takes a number")? as f32;
            if !number.is_finite() {
                return Err("`f32` takes a finite number".to_string());
            }
            let mut text = number.to_string();
            if !text.contains('.') {
                text.push_str(".0");
            }
            ("f32", text)
        }
        "bool" => (
            "bool",
            value
                .as_bool()
                .ok_or("`bool` takes true or false")?
                .to_string(),
        ),
        "string" => {
            let mut text = String::new();
            write_string(&mut text, value.as_str().ok_or("`string` takes a string")?);
            ("string", text)
        }
        other => return Err(format!("unknown column type `{other}`")),
    };

    Ok(Claimed { tag, literal })
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

/// The artifact's canonical bytes — the exact input the module hashes into `artifact_hash`.
fn canonical(package: &str, source_hash: &str, claims: &[Claim]) -> String {
    let mut out = String::new();
    let _ = write!(out, "{{\"version\":{DELTA_VERSION},\"package\":");
    write_string(&mut out, package);
    out.push_str(",\"source_hash\":");
    write_string(&mut out, source_hash);
    out.push_str(",\"claims\":[");
    for (index, claim) in claims.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"table\":");
        write_string(&mut out, claim.key.table());
        out.push_str(",\"key\":");
        claim.key.write_canonical(&mut out);
        out.push_str(",\"operation\":");
        write_string(&mut out, if claim.inserts { "insert" } else { "update" });
        out.push_str(",\"fields\":{");
        for (index, (name, claimed)) in claim.fields.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            write_string(&mut out, name);
            out.push_str(":{\"type\":");
            write_string(&mut out, claimed.tag);
            out.push_str(",\"value\":");
            out.push_str(&claimed.literal);
            out.push('}');
        }
        out.push_str("}}");
    }
    out.push_str("]}");
    out
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The source digests and spell ids the engine crate's own artifact fixtures use.
    const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REAL_SPELL: u32 = 133;
    const PACKAGE_SPELL: u32 = 6_000_001;

    /// One Package's Script Artifact, as `packages build` would emit it next to a Delta.
    const SCRIPT_ARTIFACT: &str = concat!(
        r#"{"kind":"script","version":1,"package":"example.bolt","#,
        r#""source_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
        r#""scripts":[{"script_id":100001,"name":"bolt.greet","event":"on_login","#,
        r#""priority":0,"enabled":true,"source":"grant_xp(event.actor, 10)"}]}"#,
    );

    fn one_spell_update(package: &str, spell_id: u32, fields: &str) -> String {
        format!(
            r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":{spell_id}}},"operation":"update","fields":{fields}}}]}}"#
        )
    }

    fn read(text: &str) -> Artifact {
        parse(text, Path::new("spell.json")).expect("artifact parses")
    }

    fn canonical_of(text: &str) -> String {
        let a = read(text);
        canonical(&a.package, &a.source_hash, &a.claims)
    }

    /// Taken verbatim from the engine crate's `the_canonical_form_has_no_whitespace_and_a_fixed_
    /// member_order`. This is the anchor: the expected bytes come from the crate that owns the
    /// format, not from re-running this module's own writer.
    #[test]
    fn the_canonical_form_has_no_whitespace_and_a_fixed_member_order() {
        let source = one_spell_update(
            "example.pkg",
            REAL_SPELL,
            r#"{ "gcd_ms": { "type": "u32", "value": 1500 } }"#,
        );

        assert_eq!(
            canonical_of(&source),
            format!(
                r#"{{"version":1,"package":"example.pkg","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#
            )
        );
    }

    /// Also verbatim from the engine crate: the two spellings differ in member order, indentation,
    /// a padded 64-bit string and a trailing-zero float, and must digest identically.
    #[test]
    fn member_order_whitespace_and_number_spelling_do_not_change_the_bytes() {
        let spelled_one = format!(
            r#"{{
                "version": 1,
                "package": "example.pkg",
                "source_hash": "{HASH_A}",
                "claims": [
                    {{
                        "table": "game_spell",
                        "key": {{ "spell_id": 133 }},
                        "operation": "update",
                        "fields": {{
                            "cooldown_ms": {{ "type": "u32", "value": 1500 }},
                            "family_flags": {{ "type": "u64", "value": "42" }}
                        }}
                    }},
                    {{
                        "table": "game_spell_effect",
                        "key": {{ "spell_id": 133, "effect_index": 0 }},
                        "operation": "update",
                        "fields": {{ "per_level": {{ "type": "f32", "value": 1.5 }} }}
                    }}
                ]
            }}"#
        );
        let spelled_two = format!(
            r#"{{"claims":[{{"fields":{{"per_level":{{"value":1.50,"type":"f32"}}}},"operation":"update","key":{{"effect_index":0,"spell_id":133}},"table":"game_spell_effect"}},{{"operation":"update","fields":{{"family_flags":{{"type":"u64","value":"0042"}},"cooldown_ms":{{"value":1500,"type":"u32"}}}},"key":{{"spell_id":133}},"table":"game_spell"}}],"source_hash":"{HASH_A}","package":"example.pkg","version":1}}"#
        );

        assert_eq!(canonical_of(&spelled_one), canonical_of(&spelled_two));
        assert_eq!(
            read(&spelled_one).artifact_hash,
            read(&spelled_two).artifact_hash
        );
    }

    /// The canonical form is a fixed point, which is what makes the digest stable across a rewrite.
    #[test]
    fn the_canonical_form_reads_back_as_itself() {
        let source = one_spell_update(
            "example.pkg",
            REAL_SPELL,
            r#"{"cooldown_ms":{"type":"u32","value":1500}}"#,
        );

        let once = canonical_of(&source);

        assert_eq!(canonical_of(&once), once);
    }

    #[test]
    fn a_float_always_carries_a_decimal_point() {
        let source = format!(
            r#"{{"version":1,"package":"p","source_hash":"{HASH_A}","claims":[{{"table":"game_spell_effect","key":{{"spell_id":133,"effect_index":0}},"operation":"update","fields":{{"radius_yd":{{"type":"f32","value":0}}}}}}]}}"#
        );

        assert!(
            canonical_of(&source).contains(r#""value":0.0"#),
            "{}",
            canonical_of(&source)
        );
    }

    #[test]
    fn claims_sort_by_table_then_spell_then_effect_index() {
        let source = format!(
            r#"{{"version":1,"package":"p","source_hash":"{HASH_A}","claims":[
                {{"table":"game_spell_effect","key":{{"spell_id":133,"effect_index":2}},"operation":"update","fields":{{"p0":{{"type":"i32","value":1}}}}}},
                {{"table":"game_spell_effect","key":{{"spell_id":133,"effect_index":0}},"operation":"update","fields":{{"p0":{{"type":"i32","value":1}}}}}},
                {{"table":"game_spell","key":{{"spell_id":999}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1}}}}}},
                {{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1}}}}}}
            ]}}"#
        );

        let canonical = canonical_of(&source);
        let order: Vec<&str> = [
            "spell_id\":133}",
            "spell_id\":999}",
            "effect_index\":0",
            "effect_index\":2",
        ]
        .iter()
        .map(|needle| {
            assert!(
                canonical.contains(needle),
                "{needle} missing from {canonical}"
            );
            *needle
        })
        .collect();
        let positions: Vec<usize> = order.iter().map(|n| canonical.find(n).unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{canonical}");
    }

    #[test]
    fn a_string_escapes_only_what_json_requires() {
        let mut out = String::new();
        write_string(&mut out, "a\"b\\c\nd\te\u{1}");
        assert_eq!(out, r#""a\"b\\c\nd\te\u0001""#);
    }

    // ---- discovery, mirroring the importer's own stage ----

    struct Tree(TempDir);
    impl Tree {
        fn new() -> Self {
            Self(TempDir::new().unwrap())
        }
        fn root(&self) -> &Path {
            self.0.path()
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.0.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[test]
    fn artifacts_are_discovered_inside_each_enabled_package_in_folder_order() {
        let tree = Tree::new();
        tree.write(
            "zeta/data/.generated/spell.json",
            &one_spell_update(
                "example.zeta",
                REAL_SPELL,
                r#"{"cooldown_ms":{"type":"u32","value":1500}}"#,
            ),
        );
        tree.write(
            "alpha/data/.generated/spell.json",
            &one_spell_update(
                "example.alpha",
                REAL_SPELL,
                r#"{"gcd_ms":{"type":"u32","value":1000}}"#,
            ),
        );

        let found = read_enabled(tree.root()).expect("discovery succeeds");

        let packages: Vec<&str> = found.deltas.iter().map(|a| a.package.as_str()).collect();
        assert_eq!(packages, ["example.alpha", "example.zeta"]);
    }

    #[test]
    fn a_package_with_no_generated_artifacts_contributes_nothing() {
        let tree = Tree::new();
        tree.write("rust-only/src/mod.rs", "");
        tree.write("rust-only/data/hand-written.json", "{ not even valid }");

        assert!(read_enabled(tree.root())
            .expect("discovery succeeds")
            .deltas
            .is_empty());
    }

    #[test]
    fn an_enabled_root_holding_no_packages_is_an_empty_plan_rather_than_a_refusal() {
        let tree = Tree::new();

        assert!(read_enabled(tree.root())
            .expect("discovery succeeds")
            .deltas
            .is_empty());
    }

    /// A Package ships every artifact kind it has into one directory. A Script Artifact is not a
    /// Package Delta and has a different applier, so reading one as a Delta would refuse a Package
    /// that did nothing wrong.
    #[test]
    fn a_script_artifact_is_skipped_rather_than_read_as_a_package_delta() {
        let tree = Tree::new();
        tree.write(
            "bolt/data/.generated/spell.json",
            &one_spell_update(
                "example.bolt",
                REAL_SPELL,
                r#"{"cooldown_ms":{"type":"u32","value":1500}}"#,
            ),
        );
        tree.write("bolt/data/.generated/script.json", SCRIPT_ARTIFACT);

        let found = read_enabled(tree.root()).expect("discovery succeeds");

        let packages: Vec<&str> = found.deltas.iter().map(|a| a.package.as_str()).collect();
        assert_eq!(packages, ["example.bolt"]);
        assert_eq!(found.scripts_skipped, 1);
        assert_eq!(
            found.skipped_note().as_deref(),
            Some("1 Script Artifact skipped: not a Package Delta")
        );
    }

    /// A Package that ships only Runtime Scripts claims no Delta at all. That is a clean empty
    /// plan, not a refusal, and the report still says what it passed over.
    #[test]
    fn a_package_shipping_only_a_script_artifact_contributes_no_delta() {
        let tree = Tree::new();
        tree.write("bolt/data/.generated/script.json", SCRIPT_ARTIFACT);

        let found = read_enabled(tree.root()).expect("discovery succeeds");

        assert!(found.deltas.is_empty());
        assert_eq!(found.scripts_skipped, 1);
    }

    /// The skip is a fact about the file, not about the Package it came from: the same Package may
    /// still ship a Delta, and a second Package's script is counted too.
    #[test]
    fn every_script_artifact_is_counted_and_the_note_reads_as_a_plural() {
        let tree = Tree::new();
        tree.write("alpha/data/.generated/script.json", SCRIPT_ARTIFACT);
        tree.write("zeta/data/.generated/script.json", SCRIPT_ARTIFACT);

        let found = read_enabled(tree.root()).expect("discovery succeeds");

        assert_eq!(found.scripts_skipped, 2);
        assert_eq!(
            found.skipped_note().as_deref(),
            Some("2 Script Artifacts skipped: not Package Deltas")
        );
    }

    /// Only the Script Artifact kind is passed over. Anything else still meets the parser, which
    /// reports what is wrong with it far better than a silent skip would.
    #[test]
    fn an_artifact_of_an_unknown_kind_is_still_refused_by_the_parser() {
        let tree = Tree::new();
        tree.write(
            "odd/data/.generated/thing.json",
            r#"{"kind":"weather","version":1}"#,
        );

        let refusal = read_enabled(tree.root()).expect_err("refused");

        assert!(refusal.to_string().contains("thing.json"), "{refusal}");
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_refused() {
        let tree = Tree::new();

        let refusal = read_enabled(&tree.root().join("nowhere")).expect_err("refused");

        assert!(refusal.to_string().contains("not a directory"), "{refusal}");
    }

    #[test]
    fn an_invalid_artifact_is_refused_and_the_refusal_names_its_file() {
        let tree = Tree::new();
        tree.write("broken/data/.generated/spell.json", r#"{"version":9}"#);

        let refusal = read_enabled(tree.root()).expect_err("refused");

        assert!(refusal.to_string().contains("spell.json"), "{refusal}");
    }

    #[test]
    fn one_package_named_by_two_artifacts_is_refused_before_anything_is_applied() {
        let tree = Tree::new();
        tree.write(
            "one/data/.generated/a.json",
            &one_spell_update(
                "example.twice",
                REAL_SPELL,
                r#"{"gcd_ms":{"type":"u32","value":1}}"#,
            ),
        );
        tree.write(
            "two/data/.generated/b.json",
            &one_spell_update(
                "example.twice",
                134,
                r#"{"gcd_ms":{"type":"u32","value":1}}"#,
            ),
        );

        let refusal = read_enabled(tree.root()).expect_err("refused");

        assert!(refusal.to_string().contains("appears twice"), "{refusal}");
    }

    // ---- conflicts ----

    #[test]
    fn two_packages_claiming_different_columns_of_one_row_do_not_conflict() {
        let artifacts = vec![
            read(&one_spell_update(
                "a",
                REAL_SPELL,
                r#"{"gcd_ms":{"type":"u32","value":1500}}"#,
            )),
            read(&one_spell_update(
                "b",
                REAL_SPELL,
                r#"{"cooldown_ms":{"type":"u32","value":3000}}"#,
            )),
        ];

        assert!(conflicts(&artifacts).is_empty());
    }

    #[test]
    fn two_packages_claiming_one_column_conflict_and_the_report_names_both() {
        let artifacts = vec![
            read(&one_spell_update(
                "example.first",
                REAL_SPELL,
                r#"{"cooldown_ms":{"type":"u32","value":1500}}"#,
            )),
            read(&one_spell_update(
                "example.second",
                REAL_SPELL,
                r#"{"cooldown_ms":{"type":"u32","value":3000}}"#,
            )),
        ];

        let found = conflicts(&artifacts);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("example.first"), "{found:?}");
        assert!(found[0].contains("example.second"), "{found:?}");
        assert!(found[0].contains("cooldown_ms"), "{found:?}");
    }

    /// Agreement is not a defence: the tracer refuses a shared column whatever the values, because
    /// nothing decides which Package owns it afterwards.
    #[test]
    fn two_packages_claiming_one_column_conflict_even_when_they_agree() {
        let artifacts = vec![
            read(&one_spell_update(
                "a",
                REAL_SPELL,
                r#"{"gcd_ms":{"type":"u32","value":1500}}"#,
            )),
            read(&one_spell_update(
                "b",
                REAL_SPELL,
                r#"{"gcd_ms":{"type":"u32","value":1500}}"#,
            )),
        ];

        assert_eq!(conflicts(&artifacts).len(), 1);
    }

    #[test]
    fn two_packages_inventing_one_spell_conflict() {
        let insert = |package: &str| {
            read(&format!(
                r#"{{"version":1,"package":"{package}","source_hash":"{HASH_A}","claims":[{{"table":"game_spell","key":{{"spell_id":{PACKAGE_SPELL}}},"operation":"insert","fields":{{"name":{{"type":"string","value":"Bolt"}}}}}}]}}"#
            ))
        };
        let artifacts = vec![insert("example.first"), insert("example.second")];

        let found = conflicts(&artifacts);

        assert!(
            found.iter().any(|c| c.contains("both insert this row")),
            "{found:?}"
        );
    }

    #[test]
    fn an_artifact_counts_the_rows_it_invents_apart_from_the_rows_it_tunes() {
        let artifact = read(&format!(
            r#"{{"version":1,"package":"p","source_hash":"{HASH_A}","claims":[
                {{"table":"game_spell","key":{{"spell_id":{PACKAGE_SPELL}}},"operation":"insert","fields":{{"name":{{"type":"string","value":"Bolt"}}}}}},
                {{"table":"game_spell","key":{{"spell_id":{REAL_SPELL}}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1}}}}}}
            ]}}"#
        ));

        assert_eq!(artifact.inserted_rows, 1);
        assert_eq!(artifact.updated_rows, 1);
    }
}
