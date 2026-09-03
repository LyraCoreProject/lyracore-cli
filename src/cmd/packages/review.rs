//! The trust review: a deterministic, read-only inventory of what a candidate Package would add to
//! the realm.
//!
//! It answers one question — "what does installing this register?" — and it answers it the way the
//! server's own build does. `module/build.rs` discovers a Package by text-scanning
//! `packages/<name>/src/**` for three marker macros on a comment- and string-stripped copy of each
//! file, so a commented-out marker registers nothing. This module ports that scan, because a review
//! that counted markers the build ignores (or missed ones it registers) would describe a different
//! install than the one about to happen.
//!
//! It is an INVENTORY, not a verdict. Tables and reducers are `#[table]`/`#[reducer]` attributes,
//! which the SpacetimeDB macros act on rather than the build script — they are counted here because
//! they are what the Package adds to the schema, which is what an operator is being asked to trust.
//! Everything else in the Rust is code that runs inside the module with full database access, and
//! the report says so.
//!
//! Runtime Scripts ARE counted: a Package ships its `scripts/` sources inside its own folder, and
//! that Lua runs on the realm once the Package is built. Datascripts are not — they live in
//! `datascripts/`, outside any Package folder, so a candidate cannot carry one. That row still
//! prints, as an explicit "none detected", so the review's silence about them is a deliberate
//! statement and not an oversight the reader has to notice.
//!
//! DUPLICATION, ON PURPOSE: `strip_comments_and_strings` and the three marker matchers are a port
//! of `module/build.rs`, which lives in the server repository and is not a dependency of this CLI.
//! The tests below pin the behaviour the port exists for — an inert commented-out marker, and
//! marker syntax quoted inside a doc example.

use super::tree::{self, EntryKind};
use crate::{Error, Result};
use std::path::Path;

/// The catalog of notify-hook events, as `module/build.rs` knows it. A `game_hook!` naming anything
/// else fails the server build; the review reports it as unknown rather than silently counting it.
const HOOK_EVENTS: [&str; 17] = [
    "on_damage_taken",
    "on_creature_spawn",
    "on_levelup",
    "on_group_invite",
    "on_death",
    "on_kill",
    "on_aggro",
    "on_cast_resolved",
    "on_loot",
    "on_quest_accept",
    "on_quest_turnin",
    "on_login",
    "on_logout",
    "on_gossip_select",
    "on_creature_death",
    "on_hp_threshold",
    "on_go_used",
];

/// What a candidate Package would register, and how much unclassified Rust comes with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustReview {
    /// Table accessor names from SpacetimeDB 2.x's required `#[table(accessor = ...)]`.
    pub tables: Vec<String>,
    /// Function names carrying `#[reducer]`.
    pub reducers: Vec<String>,
    /// `game_hook!(EVENT, fn NAME)` — event and handler.
    pub hooks: Vec<(String, String)>,
    /// `game_tick_pass!(fn NAME)` — passes the core scheduler runs every tick.
    pub tick_passes: Vec<String>,
    /// `character_owned!(kind, fn NAME)` — per-character sweeps and cross-shard transport arms.
    pub character_owned: Vec<String>,
    /// Directory names under `client/addons/`.
    pub addons: Vec<String>,
    /// Files under `client/mpq/` — each one shadows a stock client file.
    pub client_overrides: usize,
    /// Runtime Script sources under `scripts/` — Lua this Package would run on the realm.
    pub runtime_scripts: Vec<String>,
    pub rust_files: usize,
    pub rust_lines: usize,
}

impl TrustReview {
    /// Scan `package_dir`. Reads; never writes, never runs anything.
    pub fn scan(package_dir: &Path) -> Result<Self> {
        let mut review = Self::default();
        let entries = tree::collect(package_dir)?;

        let rust_files: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::File
                    && entry.relative.starts_with("src")
                    && entry.relative.extension().map(|ext| ext == "rs") == Some(true)
            })
            .collect();
        review.rust_files = rust_files.len();
        for entry in rust_files {
            let raw = std::fs::read_to_string(&entry.path).map_err(|error| {
                Error::Usage(format!(
                    "cannot read Rust source {} for the trust review: {error}. Nothing was copied.",
                    entry.path.display()
                ))
            })?;
            review.rust_lines += raw.lines().count();
            review.scan_source(&strip_comments_and_strings(&raw));
        }

        review.addons = entries
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::Directory
                    && entry.relative.parent() == Some(Path::new("client/addons"))
            })
            .map(|entry| {
                entry
                    .relative
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        review.addons.sort();
        review.client_overrides = entries
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::File && entry.relative.starts_with("client/mpq")
            })
            .count();

        review.runtime_scripts = entries
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::File
                    && entry.relative.parent() == Some(Path::new("scripts"))
                    && entry
                        .relative
                        .extension()
                        .is_some_and(|ext| ext == "ts" || ext == "lua")
            })
            .map(|entry| {
                entry
                    .relative
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        review.runtime_scripts.sort();

        Ok(review)
    }

    fn scan_source(&mut self, content: &str) {
        for head in attribute_heads(content, "table") {
            let accessor = balanced(head.trim_start())
                .and_then(|args| top_level_named_ident(&args, "accessor"))
                .unwrap_or_else(|| {
                    "(invalid table: missing required `accessor = name`)".to_string()
                });
            self.tables.push(accessor);
        }
        for head in attribute_heads(content, "reducer") {
            self.reducers.push(following_fn_name(head));
        }
        for head in marker_heads(content, "game_hook!") {
            if let Some((event, name)) = match_hook(head) {
                let event = if HOOK_EVENTS.contains(&event.as_str()) {
                    event
                } else {
                    format!("{event} (unknown event — the server build refuses it)")
                };
                self.hooks.push((event, name));
            }
        }
        for head in marker_heads(content, "game_tick_pass!") {
            if let Some(name) = head.strip_prefix('(').and_then(match_fn_name) {
                self.tick_passes.push(name);
            }
        }
        for head in marker_heads(content, "character_owned!") {
            if let Some(name) = match_character_owned(head) {
                self.character_owned.push(name);
            }
        }
        self.tables.sort();
        self.reducers.sort();
        self.hooks.sort();
        self.tick_passes.sort();
        self.character_owned.sort();
    }

    /// Does this Package register anything at all? A folder with neither Rust nor client content
    /// is rejected before the review runs, so a review with nothing in it means Rust that
    /// registers nothing — which is still trusted code.
    pub fn registers_nothing(&self) -> bool {
        self.tables.is_empty()
            && self.reducers.is_empty()
            && self.hooks.is_empty()
            && self.tick_passes.is_empty()
            && self.character_owned.is_empty()
            && self.addons.is_empty()
            && self.client_overrides == 0
            && self.runtime_scripts.is_empty()
    }

    /// The full block `packages add` prints before it asks.
    pub fn render(&self, source: &Path) -> String {
        let hooks: Vec<String> = self
            .hooks
            .iter()
            .map(|(event, name)| format!("{event} -> {name}"))
            .collect();
        let mut out = format!(
            "trust review — deterministic scan of {}\n",
            source.display()
        );
        out.push_str(&row("tables", self.tables.len(), &self.tables));
        out.push_str(&row("reducers", self.reducers.len(), &self.reducers));
        out.push_str(&row("hooks", self.hooks.len(), &hooks));
        out.push_str(&row(
            "tick passes",
            self.tick_passes.len(),
            &self.tick_passes,
        ));
        out.push_str(&row(
            "character-owned",
            self.character_owned.len(),
            &self.character_owned,
        ));
        out.push_str(&row("addons", self.addons.len(), &self.addons));
        out.push_str(&row("client overrides", self.client_overrides, &[]));
        out.push_str(
            "  datascripts        none detected (nothing in this checkout reads them yet)\n",
        );
        out.push_str(&row(
            "runtime scripts",
            self.runtime_scripts.len(),
            &self.runtime_scripts,
        ));
        out.push_str(&format!(
            "  trusted Rust       {} file(s), {} line(s)\n",
            self.rust_files, self.rust_lines
        ));
        out.push_str(
            "\nThe rows above are what the build registers. Everything else in those Rust files is \
             TRUSTED\nCODE: it runs inside the module with full access to every table in the \
             database, and no row\nhere bounds it. This is an inventory, not a security guarantee. \
             Read the source first.\n",
        );
        out
    }

    /// The one-line content-kind summary `packages list` shows per Package.
    pub fn kinds_summary(&self) -> String {
        let mut parts = Vec::new();
        for (label, count) in [
            ("tables", self.tables.len()),
            ("reducers", self.reducers.len()),
            ("hooks", self.hooks.len()),
            ("tick passes", self.tick_passes.len()),
            ("character-owned", self.character_owned.len()),
            ("addons", self.addons.len()),
            ("client overrides", self.client_overrides),
            ("runtime scripts", self.runtime_scripts.len()),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        if self.rust_files > 0 {
            parts.push(format!("{} Rust file(s)", self.rust_files));
        }
        if parts.is_empty() {
            "(nothing detected)".to_string()
        } else {
            parts.join(", ")
        }
    }
}

fn row(label: &str, count: usize, names: &[String]) -> String {
    let detail = if names.is_empty() {
        String::new()
    } else {
        format!("  {}", names.join(", "))
    };
    format!("  {label:<18} {count}{detail}\n")
}

// ---- the marker scan, ported from module/build.rs ---------------------------------------------

/// Every occurrence of `marker`, as the left-trimmed text that follows it.
fn marker_heads<'a>(content: &'a str, marker: &str) -> Vec<&'a str> {
    let mut heads = Vec::new();
    let mut from = 0usize;
    while let Some(index) = content[from..].find(marker) {
        let head_start = from + index + marker.len();
        heads.push(content[head_start..].trim_start());
        from = head_start;
    }
    heads
}

/// Every `#[<attr>` / `#[spacetimedb::<attr>` occurrence, as the text that follows the name.
fn attribute_heads<'a>(content: &'a str, attr: &str) -> Vec<&'a str> {
    let mut heads = Vec::new();
    for prefix in [format!("#[{attr}"), format!("#[spacetimedb::{attr}")] {
        let mut from = 0usize;
        while let Some(index) = content[from..].find(&prefix) {
            let head_start = from + index + prefix.len();
            // `#[tables]` must not match `#[table`. The next character ends the attribute name.
            if !content[head_start..].starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                heads.push(&content[head_start..]);
            }
            from = head_start;
        }
    }
    heads
}

fn balanced(head: &str) -> Option<String> {
    let inner = head.strip_prefix('(')?;
    let mut depth = 1usize;
    for (index, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(inner[..index].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// A top-level `key = identifier` argument. Nested `index(...)`/`scheduled(...)` options can carry
/// their own `accessor` tokens, so commas and equals signs inside delimiters must not be mistaken
/// for the table's required accessor.
fn top_level_named_ident(args: &str, key: &str) -> Option<String> {
    let mut round = 0usize;
    let mut square = 0usize;
    let mut curly = 0usize;
    let mut start = 0usize;
    for (index, c) in args
        .char_indices()
        .chain(std::iter::once((args.len(), ',')))
    {
        match c {
            '(' => round += 1,
            ')' => round = round.saturating_sub(1),
            '[' => square += 1,
            ']' => square = square.saturating_sub(1),
            '{' => curly += 1,
            '}' => curly = curly.saturating_sub(1),
            ',' if round == 0 && square == 0 && curly == 0 => {
                let part = &args[start..index];
                if let Some((left, right)) = part.split_once('=') {
                    if left.trim() == key {
                        let right = right.trim();
                        let ident = read_ident(right)?;
                        let tail = &right[ident.len()..];
                        if tail.trim().is_empty() {
                            return Some(ident);
                        }
                    }
                }
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    None
}

/// The first `fn NAME` after an attribute — the item it applies to, past any further attributes.
fn following_fn_name(head: &str) -> String {
    let mut rest = head;
    while let Some(index) = rest.find("fn ") {
        let after = &rest[index + 3..];
        if let Some(name) = read_ident(after) {
            return name;
        }
        rest = after;
    }
    "(unnamed)".to_string()
}

/// `game_hook!` head: `(EVENT, fn NAME(...`.
fn match_hook(head: &str) -> Option<(String, String)> {
    let rest = head.strip_prefix('(')?.trim_start();
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    if end == 0 {
        return None;
    }
    let event = rest[..end].to_string();
    let rest = rest[end..].trim_start().strip_prefix(',')?;
    Some((event, match_fn_name(rest)?))
}

/// `character_owned!` head: `(KIND, fn NAME(...` for the four known kinds.
fn match_character_owned(head: &str) -> Option<String> {
    for kind in ["delete", "restamp", "transfer", "not_transported"] {
        if let Some(rest) = head.strip_prefix(format!("({kind},").as_str()) {
            if let Some(name) = match_fn_name(rest) {
                return Some(format!("{kind}: {name}"));
            }
        }
    }
    None
}

/// Optional whitespace, `fn NAME`, then `(` — the marker's own parameter list.
fn match_fn_name(rest: &str) -> Option<String> {
    let rest = rest.trim_start().strip_prefix("fn ")?.trim_start();
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    if end == 0 || !rest[end..].trim_start().starts_with('(') {
        return None;
    }
    Some(rest[..end].to_string())
}

fn read_ident(text: &str) -> Option<String> {
    let text = text.trim_start();
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    (end > 0).then(|| text[..end].to_string())
}

/// Blank out comments (line and nested block), string literals (plain, byte, raw) and char
/// literals, preserving newlines and character positions — so the scan sees only real code.
///
/// The port of `module/build.rs`'s function of the same name. Lifetimes (`'a`) survive; only a
/// real char literal is blanked.
fn strip_comments_and_strings(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(b.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(b[i]));
                    i += 1;
                }
            }
            continue;
        }
        // A raw string only when `r`/`br` starts a token, so `for` and `attr` never trigger it.
        let prev_is_ident = i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_');
        if !prev_is_ident && (c == 'r' || (c == 'b' && b.get(i + 1) == Some(&'r'))) {
            let r_at = if c == 'b' { i + 1 } else { i };
            let mut j = r_at + 1;
            while b.get(j) == Some(&'#') {
                j += 1;
            }
            if b.get(j) == Some(&'"') {
                let hashes = j - (r_at + 1);
                for c in &b[i..=j] {
                    out.push(blank(*c));
                }
                i = j + 1;
                'raw: while i < b.len() {
                    if b[i] == '"' {
                        let mut h = 0usize;
                        while h < hashes && b.get(i + 1 + h) == Some(&'#') {
                            h += 1;
                        }
                        if h == hashes {
                            for c in &b[i..=(i + hashes)] {
                                out.push(blank(*c));
                            }
                            i += hashes + 1;
                            break 'raw;
                        }
                    }
                    out.push(blank(b[i]));
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' || (!prev_is_ident && c == 'b' && b.get(i + 1) == Some(&'"')) {
            if c == 'b' {
                out.push(' ');
                i += 1;
            }
            out.push(' ');
            i += 1;
            while i < b.len() {
                if b[i] == '\\' {
                    out.push(' ');
                    if i + 1 < b.len() {
                        out.push(blank(b[i + 1]));
                    }
                    i += 2;
                    continue;
                }
                if b[i] == '"' {
                    out.push(' ');
                    i += 1;
                    break;
                }
                out.push(blank(b[i]));
                i += 1;
            }
            continue;
        }
        if c == '\'' {
            let is_char_lit = match b.get(i + 1) {
                Some('\\') => true,
                Some(_) => b.get(i + 2) == Some(&'\''),
                None => false,
            };
            if is_char_lit {
                out.push(' ');
                i += 1;
                if b.get(i) == Some(&'\\') {
                    out.push(' ');
                    i += 1;
                    if i < b.len() {
                        out.push(blank(b[i]));
                        i += 1;
                    }
                    while i < b.len() && b[i] != '\'' {
                        out.push(blank(b[i]));
                        i += 1;
                    }
                } else {
                    out.push(blank(b[i]));
                    i += 1;
                }
                if b.get(i) == Some(&'\'') {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn package(files: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        for (relative, content) in files {
            let path = tmp.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        tmp
    }

    #[test]
    fn the_review_names_what_the_build_would_register() {
        let tmp = package(&[(
            "src/mod.rs",
            "#[spacetimedb::table(accessor = pkg_greeter_log, public)]\n\
             pub struct GreeterLog { pub id: u64 }\n\
             #[spacetimedb::reducer]\n\
             pub fn pkg_greeter_reset(ctx: &ReducerContext) {}\n\
             game_hook!(on_login, fn greet(ctx, payload) { });\n\
             game_tick_pass!(fn sweep_greetings(ctx) { });\n\
             character_owned!(delete, fn sweep_delete_pkg_greeter_log(ctx, guid) { });\n",
        )]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(review.tables, ["pkg_greeter_log"]);
        assert_eq!(review.reducers, ["pkg_greeter_reset"]);
        assert_eq!(
            review.hooks,
            [("on_login".to_string(), "greet".to_string())]
        );
        assert_eq!(review.tick_passes, ["sweep_greetings"]);
        assert_eq!(
            review.character_owned,
            ["delete: sweep_delete_pkg_greeter_log"]
        );
        assert_eq!(review.rust_files, 1);
    }

    #[test]
    fn tables_use_the_pinned_spacetimedb_two_x_accessor_syntax() {
        let tmp = package(&[(
            "src/mod.rs",
            "#[table(accessor = one)]\n\
             pub struct One { pub id: u64 }\n\
             #[spacetimedb::table(\n\
                 index(name = by_id, btree(columns = [id])),\n\
                 name = \"canonical_two\",\n\
                 accessor = two,\n\
             )]\n\
             pub struct Two { pub id: u64 }\n\
             #[table]\n\
             pub struct Missing { pub id: u64 }\n\
             #[table(name = legacy)]\n\
             pub struct Legacy { pub id: u64 }\n",
        )]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(
            review.tables,
            [
                "(invalid table: missing required `accessor = name`)",
                "(invalid table: missing required `accessor = name`)",
                "one",
                "two",
            ]
        );
    }

    #[test]
    fn a_commented_out_or_quoted_marker_registers_nothing() {
        // The property the ported stripper exists for: the server build ignores both of these, so
        // a review that counted them would describe an install that never happens.
        let tmp = package(&[(
            "src/mod.rs",
            "// game_hook!(on_login, fn commented(ctx, payload) { });\n\
             /// Example: `game_tick_pass!(fn documented(ctx) { })`\n\
             /* character_owned!(delete, fn blocked(ctx, guid) { }); */\n\
             const EXAMPLE: &str = \"game_hook!(on_death, fn quoted(ctx, payload) { });\";\n\
             game_hook!(on_login, fn real(ctx, payload) { });\n",
        )]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(review.hooks, [("on_login".to_string(), "real".to_string())]);
        assert!(review.tick_passes.is_empty(), "{review:?}");
        assert!(review.character_owned.is_empty(), "{review:?}");
    }

    #[test]
    fn non_utf8_rust_is_refused_instead_of_rendered_as_empty_trusted_code() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/mod.rs"), [0xff, 0xfe]).unwrap();

        let error = TrustReview::scan(tmp.path()).unwrap_err();

        assert!(
            error.to_string().contains("cannot read Rust source"),
            "{error}"
        );
    }

    #[test]
    fn a_hook_naming_an_event_the_build_refuses_is_reported_not_counted_as_ordinary() {
        let tmp = package(&[(
            "src/mod.rs",
            "game_hook!(on_teatime, fn brew(ctx, payload) { });\n",
        )]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(review.hooks.len(), 1);
        assert!(review.hooks[0].0.contains("unknown event"), "{review:?}");
    }

    #[test]
    fn client_content_is_counted_from_the_two_channels_the_packer_reads() {
        let tmp = package(&[
            ("client/addons/Greeter/Greeter.lua", "-- hi\n"),
            ("client/addons/Greeter/Greeter.toc", "## Interface: 11200\n"),
            ("client/addons/Bagger/Bagger.lua", "-- bags\n"),
            (
                "client/mpq/Interface/FrameXML/ChatFrame.lua",
                "-- override\n",
            ),
        ]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        // Addons are DIRECTORIES; overrides are individual files that shadow stock ones.
        assert_eq!(review.addons, ["Bagger", "Greeter"]);
        assert_eq!(review.client_overrides, 1);
        assert_eq!(review.rust_files, 0);
    }

    #[test]
    fn the_rendered_review_states_that_unclassified_rust_is_trusted_code() {
        let tmp = package(&[("src/mod.rs", "pub fn helper() {}\n")]);
        let review = TrustReview::scan(tmp.path()).unwrap();

        let text = review.render(tmp.path());

        assert!(text.contains("TRUSTED"), "{text}");
        assert!(text.contains("not a security guarantee"), "{text}");
        // Datascripts, the one kind a candidate cannot carry, are stated rather than omitted.
        assert!(text.contains("datascripts"), "{text}");
        assert!(text.contains("none detected"), "{text}");
        assert!(text.contains("runtime scripts"), "{text}");
        assert!(review.registers_nothing(), "{review:?}");
    }

    /// Runtime Scripts run ON THE REALM once the Package is built, so a candidate carrying them
    /// must never be reviewed as if it carried none.
    #[test]
    fn runtime_script_sources_are_named_in_the_review() {
        let tmp = package(&[
            ("scripts/ember_echo.ts", "// @event on_login\n"),
            ("scripts/bonus.lua", "-- @event on_login\n"),
            ("scripts/README.md", "not a script\n"),
        ]);

        let review = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(review.runtime_scripts, ["bonus.lua", "ember_echo.ts"]);
        assert!(!review.registers_nothing(), "{review:?}");
        let text = review.render(tmp.path());
        assert!(text.contains("ember_echo.ts"), "{text}");
        assert!(
            review.kinds_summary().contains("2 runtime scripts"),
            "{review:?}"
        );
    }

    #[test]
    fn the_scan_is_deterministic_over_the_same_tree() {
        let tmp = package(&[
            ("src/mod.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "game_hook!(on_kill, fn one(ctx, p) { });\n"),
            ("src/b.rs", "game_hook!(on_kill, fn two(ctx, p) { });\n"),
        ]);

        let first = TrustReview::scan(tmp.path()).unwrap();
        let second = TrustReview::scan(tmp.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.hooks,
            [
                ("on_kill".to_string(), "one".to_string()),
                ("on_kill".to_string(), "two".to_string())
            ]
        );
    }
}
