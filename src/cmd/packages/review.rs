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
//! Datascripts and Runtime Scripts have no tooling in this checkout yet. They are reported as an
//! explicit "none detected" row rather than omitted, so the review's silence about them is a
//! deliberate statement and not an oversight the reader has to notice.
//!
//! DUPLICATION, ON PURPOSE: `strip_comments_and_strings` and the three marker matchers are a port
//! of `module/build.rs`, which lives in the server repository and is not a dependency of this CLI.
//! The tests below pin the behaviour the port exists for — an inert commented-out marker, and
//! marker syntax quoted inside a doc example.

use crate::Result;
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
    /// Table names from `#[table(name = ...)]`.
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
    pub rust_files: usize,
    pub rust_lines: usize,
}

impl TrustReview {
    /// Scan `package_dir`. Reads; never writes, never runs anything.
    pub fn scan(package_dir: &Path) -> Result<Self> {
        let mut review = Self::default();

        let src = package_dir.join("src");
        if src.is_dir() {
            let mut files = Vec::new();
            collect_rs_files(&src, &mut files)?;
            files.sort();
            review.rust_files = files.len();
            for file in &files {
                let raw = std::fs::read_to_string(file).unwrap_or_default();
                review.rust_lines += raw.lines().count();
                review.scan_source(&strip_comments_and_strings(&raw));
            }
        }

        let addons = package_dir.join("client").join("addons");
        if addons.is_dir() {
            for entry in std::fs::read_dir(&addons)? {
                let path = entry?.path();
                if path.is_dir() {
                    review.addons.push(
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into(),
                    );
                }
            }
            review.addons.sort();
        }

        let mpq = package_dir.join("client").join("mpq");
        if mpq.is_dir() {
            let mut overrides = Vec::new();
            collect_files(&mpq, &mut overrides)?;
            review.client_overrides = overrides.len();
        }

        Ok(review)
    }

    fn scan_source(&mut self, content: &str) {
        for args in attribute_args(content, "table") {
            self.tables.push(named_argument(&args, "name"));
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
        out.push_str(
            "  runtime scripts    none detected (nothing in this checkout reads them yet)\n",
        );
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

/// The argument list of each `#[<attr>(...)]`, with balanced parentheses.
fn attribute_args(content: &str, attr: &str) -> Vec<String> {
    attribute_heads(content, attr)
        .into_iter()
        .filter_map(|head| balanced(head.trim_start()))
        .collect()
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

/// `name = <ident>` out of an attribute argument list. Absent means the macro defaults the table
/// name to the struct's; the review says so rather than guessing at it.
fn named_argument(args: &str, key: &str) -> String {
    for part in args.split(',') {
        if let Some((left, right)) = part.split_once('=') {
            if left.trim() == key {
                return right.trim().to_string();
            }
        }
    }
    "(name defaulted to the struct)".to_string()
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
    let end = text.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
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

fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
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
            "#[spacetimedb::table(name = pkg_greeter_log, public)]\n\
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
        // The two kinds with no tooling are stated, not omitted.
        assert!(text.contains("datascripts"), "{text}");
        assert!(text.contains("runtime scripts"), "{text}");
        assert!(text.contains("none detected"), "{text}");
        assert!(review.registers_nothing(), "{review:?}");
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
