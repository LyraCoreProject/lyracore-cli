//! The Package provenance stamp: where an installed Package came from, and what its content was
//! when it was installed.
//!
//! The stamp lives INSIDE the Package directory (`packages/<name>/.lyracore-package.toml`) rather
//! than in a side ledger, so the record travels with the folder. Disabling a Package is a move of
//! that one directory; a side ledger would need a second, separately-corruptible move to stay
//! paired with it.
//!
//! The stamp is deliberately excluded from the content identity it carries: a hash cannot cover
//! the file it is written into.
//!
//! TOML, hand-written and hand-read. Four flat string keys do not earn a parser dependency, and a
//! stamp is read on a path where being unable to read one must degrade to "unrecorded" rather than
//! to a failed command — `packages list` has to describe a hand-edited or pre-existing Package
//! sensibly.

use crate::Result;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The stamp's file name inside the Package directory.
pub const STAMP_FILE: &str = ".lyracore-package.toml";

/// The Package Source kind a local-folder install records. Git URLs and Official Package lookups
/// are separate issues; an unrecognised kind read back from disk is rendered verbatim rather than
/// refused, because this file is operator-editable input.
pub const SOURCE_LOCAL: &str = "local";

/// What `packages add` recorded about an install.
///
/// Every field is optional on READ (an empty string means the key was absent or unparseable) and
/// mandatory on WRITE. That asymmetry is the point: a hand-edited stamp still renders whatever it
/// still has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvenanceStamp {
    /// `local` today. The vocabulary this field may grow: `git`, `official`.
    pub source_kind: String,
    /// The absolute folder the Package was copied FROM.
    pub source: String,
    /// [`content_identity`] of the copied tree at install time.
    pub content_identity: String,
    /// UTC, RFC 3339, second resolution.
    pub installed_at: String,
}

impl ProvenanceStamp {
    /// The stamp a local-folder install writes. `now` is epoch seconds.
    pub fn local(source: &Path, content_identity: String, now: u64) -> Self {
        Self {
            source_kind: SOURCE_LOCAL.to_string(),
            source: source.to_string_lossy().to_string(),
            content_identity,
            installed_at: utc_rfc3339(now),
        }
    }

    /// Read the stamp out of a Package directory.
    ///
    /// `None` means this Package has no readable stamp — it was created by hand, predates
    /// `packages add`, or its stamp file is unreadable. All three are states `packages list`
    /// describes; none of them is an error.
    pub fn read(package_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(package_dir.join(STAMP_FILE)).ok()?;
        Some(Self::parse(&text))
    }

    pub fn write(&self, package_dir: &Path) -> Result<()> {
        std::fs::write(package_dir.join(STAMP_FILE), self.render())?;
        Ok(())
    }

    pub fn render(&self) -> String {
        format!(
            "# Written by `lyracore packages add`. It records where this Package came from and\n\
             # what its content was at install time; `lyracore packages list` compares the tree\n\
             # against `content_identity` to report local drift. This file is excluded from that\n\
             # hash. Editing it changes only the report, never the Package.\n\
             source_kind = {}\n\
             source = {}\n\
             content_identity = {}\n\
             installed_at = {}\n",
            quote(&self.source_kind),
            quote(&self.source),
            quote(&self.content_identity),
            quote(&self.installed_at),
        )
    }

    /// Flat `key = "value"` lines. Never fails: an unrecognised key is ignored and a missing one
    /// stays empty.
    fn parse(text: &str) -> Self {
        let mut stamp = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            match key.trim() {
                "source_kind" => stamp.source_kind = value,
                "source" => stamp.source = value,
                "content_identity" => stamp.content_identity = value,
                "installed_at" => stamp.installed_at = value,
                _ => {}
            }
        }
        stamp
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped) => out.push(escaped),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The content identity of a Package tree: FNV-1a/64 over every file in it, in sorted relative-path
/// order, as `<relative path>\0<byte length>\0<bytes>`.
///
/// FNV-1a rather than a digest crate, and hand-written rather than `DefaultHasher`: this value is
/// PERSISTED, so it needs an algorithm that is fixed forever, and `DefaultHasher`'s explicitly is
/// not stable across Rust releases. What it must catch is an operator editing an installed copy —
/// accidental drift, which any 64-bit hash catches. It is not a tamper seal and the trust review
/// already says the install is not one.
///
/// The algorithm name is part of the recorded value, so replacing it later is a readable migration
/// rather than a silent reinterpretation of old stamps.
pub fn content_identity(package_dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    collect_files(package_dir, package_dir, &mut files)?;
    files.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for relative in &files {
        let bytes = std::fs::read(package_dir.join(relative))?;
        fnv(&mut hash, relative.as_bytes());
        fnv(&mut hash, &[0]);
        fnv(&mut hash, bytes.len().to_string().as_bytes());
        fnv(&mut hash, &[0]);
        fnv(&mut hash, &bytes);
    }
    Ok(format!("fnv1a64:{hash:016x}"))
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Every file under `dir`, as `/`-separated paths relative to `root`, with the stamp itself left
/// out. Directories contribute only through the paths of the files inside them, so an empty
/// directory is not part of a Package's identity — nothing the build or the packer reads is.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if relative != STAMP_FILE {
            out.push(relative);
        }
    }
    Ok(())
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Epoch seconds as UTC RFC 3339, second resolution.
///
/// The civil-date conversion is the standard days-from-epoch algorithm with the year shifted to
/// start in March, so the leap day lands at the end of the shifted year and needs no special case.
fn utc_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn a_stamp_round_trips_through_its_file() {
        let tmp = TempDir::new().unwrap();
        let stamp = ProvenanceStamp::local(
            Path::new("/home/dev/my \"packages\"/greeter"),
            "fnv1a64:0123456789abcdef".to_string(),
            1_756_000_000,
        );

        stamp.write(tmp.path()).unwrap();

        assert_eq!(ProvenanceStamp::read(tmp.path()), Some(stamp.clone()));
        assert_eq!(stamp.source_kind, SOURCE_LOCAL);
        assert_eq!(stamp.source, "/home/dev/my \"packages\"/greeter");
    }

    #[test]
    fn a_package_without_a_stamp_reads_as_unrecorded_rather_than_failing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(ProvenanceStamp::read(tmp.path()), None);
    }

    #[test]
    fn a_hand_edited_stamp_keeps_whatever_it_still_has() {
        // `packages list` must describe a mangled stamp, not fail on it — the operator who broke
        // it is exactly the one who needs the report.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(STAMP_FILE),
            "# note\nsource = \"/src/greeter\"\nnonsense\nunknown_key = \"x\"\n",
        )
        .unwrap();

        let stamp = ProvenanceStamp::read(tmp.path()).unwrap();

        assert_eq!(stamp.source, "/src/greeter");
        assert_eq!(stamp.content_identity, "");
        assert_eq!(stamp.source_kind, "");
    }

    #[test]
    fn the_content_identity_covers_every_file_and_its_place_in_the_tree() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/mod.rs", "pub fn a() {}\n");
        write(tmp.path(), "client/addons/Greeter/Greeter.lua", "-- hi\n");
        let original = content_identity(tmp.path()).unwrap();

        // Same bytes, different path: a moved file is drift.
        std::fs::rename(
            tmp.path().join("src/mod.rs"),
            tmp.path().join("src/moved.rs"),
        )
        .unwrap();
        assert_ne!(content_identity(tmp.path()).unwrap(), original);

        std::fs::rename(
            tmp.path().join("src/moved.rs"),
            tmp.path().join("src/mod.rs"),
        )
        .unwrap();
        assert_eq!(content_identity(tmp.path()).unwrap(), original);

        // Edited bytes, same path.
        write(tmp.path(), "src/mod.rs", "pub fn b() {}\n");
        assert_ne!(content_identity(tmp.path()).unwrap(), original);
    }

    #[test]
    fn the_stamp_is_not_part_of_the_identity_it_records() {
        // It carries the hash, so it cannot be inside it.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/mod.rs", "pub fn a() {}\n");
        let before = content_identity(tmp.path()).unwrap();

        ProvenanceStamp::local(Path::new("/src/greeter"), before.clone(), 1_756_000_000)
            .write(tmp.path())
            .unwrap();

        assert_eq!(content_identity(tmp.path()).unwrap(), before);
    }

    #[test]
    fn the_identity_names_the_algorithm_that_produced_it() {
        // A recorded hash whose algorithm is implicit cannot be replaced without silently
        // reinterpreting every old stamp.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/mod.rs", "pub fn a() {}\n");
        let identity = content_identity(tmp.path()).unwrap();
        assert!(identity.starts_with("fnv1a64:"), "{identity}");
        assert_eq!(identity.len(), "fnv1a64:".len() + 16, "{identity}");
    }

    #[test]
    fn install_timestamps_render_as_utc_rfc3339() {
        // Values checked against `date -u -d @<secs>`, not recomputed with this implementation.
        assert_eq!(utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_rfc3339(1_756_051_199), "2025-08-24T15:59:59Z");
        assert_eq!(utc_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
    }
}
