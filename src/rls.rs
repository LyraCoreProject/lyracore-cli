//! Validate every `#[client_visibility_filter]`'s SQL identifiers against the generated bindings.
//!
//! This is a port of the server repo's `scripts/validate-rls-filters.py` (LyraCore preflight check
//! 3). It exists because SpacetimeDB stores a visibility filter's SQL as raw TEXT during schema
//! extraction: a filter naming a column that does not exist passes `spacetime publish` and then
//! rejects a gateway *subscription* — i.e. it breaks login, and only login, on a live stack.
//!
//! WHY IT IS HAND-ROLLED. The Python leans on `re`; this crate deliberately carries four
//! dependencies and none of them is a regex engine. Every pattern the validator needs is a word
//! scan over ASCII, so the port is a tokenizer rather than a new dependency in a CLI that is
//! installed with `cargo install --locked` on a contributor's first run.
//!
//! Behaviour is kept verdict-for-verdict identical to the Python, including its non-overlapping
//! scan for `FROM`/`JOIN` table references — see `table_refs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Words that are SQL, not identifiers. Lowercase; comparisons are case-insensitive.
const SQL_KEYWORDS: [&str; 32] = [
    "all", "and", "as", "asc", "between", "by", "desc", "distinct", "false", "from", "full",
    "group", "having", "in", "inner", "is", "join", "left", "like", "limit", "not", "null",
    "offset", "on", "or", "order", "outer", "right", "select", "true", "union", "where",
];

fn is_keyword(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    SQL_KEYWORDS.contains(&lower.as_str())
}

/// One `Filter::Sql("…")` found in the module sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSql {
    pub path: PathBuf,
    pub line: usize,
    pub sql: String,
}

impl FilterSql {
    fn location(&self) -> String {
        format!("{}:{}", self.path.display(), self.line)
    }
}

/// The generated schema: table name -> its column names.
pub type Schema = BTreeMap<String, BTreeSet<String>>;

// ---------------------------------------------------------------------------------------------
// tokenizing
// ---------------------------------------------------------------------------------------------

/// A maximal run of `[A-Za-z0-9_]`, which is what a `\b\w+\b` match is over ASCII.
#[derive(Debug, Clone, Copy)]
struct Word {
    start: usize,
    end: usize,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn words(text: &[u8]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if is_word_byte(text[i]) {
            let start = i;
            while i < text.len() && is_word_byte(text[i]) {
                i += 1;
            }
            out.push(Word { start, end: i });
        } else {
            i += 1;
        }
    }
    out
}

fn word_text<'a>(text: &'a [u8], word: &Word) -> &'a str {
    std::str::from_utf8(&text[word.start..word.end]).unwrap_or("")
}

/// A run that could be an identifier: `[A-Za-z_][A-Za-z0-9_]*`. A run starting with a digit is not
/// one, and the Python's `\b` boundary means it is not an identifier *match* either — `123abc`
/// yields nothing, not `abc`.
fn is_identifier(text: &[u8], word: &Word) -> bool {
    let first = text[word.start];
    first.is_ascii_alphabetic() || first == b'_'
}

/// Is everything between `from` and `to` whitespace, and is there at least one byte of it? That is
/// the Python's `\s+` between two capture groups.
fn separated_by_space(text: &[u8], from: usize, to: usize) -> bool {
    to > from && text[from..to].iter().all(|b| b.is_ascii_whitespace())
}

fn blank(text: &mut [u8], start: usize, end: usize) {
    for byte in &mut text[start..end] {
        *byte = b' ';
    }
}

// ---------------------------------------------------------------------------------------------
// finding the filters in the module sources
// ---------------------------------------------------------------------------------------------

/// A line that is nothing but the attribute, in either spelling. The Python anchors this with
/// `^\s*#\[(?:spacetimedb::)?client_visibility_filter\]\s*$`.
fn is_attribute_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "#[client_visibility_filter]"
        || trimmed == "#[spacetimedb::client_visibility_filter]"
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Decode a Rust string literal anchored at `at`, returning its value and the byte after it.
///
/// Handles both forms the module uses: a raw literal (`r"…"`, `r#"…"#`) whose body is verbatim,
/// and a quoted literal whose escapes are resolved.
fn rust_string_literal(text: &[u8], at: usize) -> Option<String> {
    if at >= text.len() {
        return None;
    }
    if text[at] == b'r' {
        let mut i = at + 1;
        let hashes = {
            let start = i;
            while i < text.len() && text[i] == b'#' {
                i += 1;
            }
            i - start
        };
        if i >= text.len() || text[i] != b'"' {
            return None;
        }
        i += 1;
        let body_start = i;
        // The terminator is `"` followed by exactly the opening run of hashes.
        while i < text.len() {
            if text[i] == b'"' {
                let closing = text.get(i + 1..i + 1 + hashes)?;
                if closing.iter().all(|b| *b == b'#') {
                    return String::from_utf8(text[body_start..i].to_vec()).ok();
                }
            }
            i += 1;
        }
        return None;
    }
    if text[at] != b'"' {
        return None;
    }
    let mut i = at + 1;
    let mut out = Vec::new();
    while i < text.len() {
        match text[i] {
            b'"' => return String::from_utf8(out).ok(),
            b'\\' => {
                i += 1;
                let escaped = *text.get(i)?;
                out.push(match escaped {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'0' => b'\0',
                    other => other,
                });
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    None
}

/// Every filter declared under `module_dir`, plus the parse failures that are themselves errors.
pub fn find_filters(module_dir: &Path) -> (Vec<FilterSql>, Vec<String>) {
    let mut filters = Vec::new();
    let mut errors = Vec::new();

    for path in rust_files(module_dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            errors.push(format!("{}: could not be read", path.display()));
            continue;
        };
        let bytes = text.as_bytes();

        // Byte offsets of every line that is exactly the attribute.
        let mut declarations = Vec::new();
        let mut offset = 0usize;
        for line in text.split_inclusive('\n') {
            if is_attribute_line(line) {
                declarations.push(offset);
            }
            offset += line.len();
        }

        for (index, marker) in declarations.iter().copied().enumerate() {
            // The Python's window: from this attribute to the NEXT one (or end of file).
            let end = declarations.get(index + 1).copied().unwrap_or(bytes.len());
            let region = &bytes[marker..end];
            let line = text[..marker].matches('\n').count() + 1;

            let Some(after_open) = filter_sql_call(region) else {
                errors.push(format!(
                    "{}:{line}: filter has no literal Filter::Sql(...) expression",
                    path.display()
                ));
                continue;
            };
            match rust_string_literal(region, after_open) {
                Some(sql) => filters.push(FilterSql {
                    path: path.clone(),
                    line,
                    sql,
                }),
                None => errors.push(format!(
                    "{}:{line}: Filter::Sql argument is not a string literal",
                    path.display()
                )),
            }
        }
    }

    if filters.is_empty() && errors.is_empty() {
        errors.push(format!(
            "{}: no #[client_visibility_filter] declarations found; parser or source layout drifted",
            module_dir.display()
        ));
    }
    (filters, errors)
}

/// Offset just past `Filter::Sql` `\s*` `(` `\s*`, i.e. where the literal must begin.
fn filter_sql_call(region: &[u8]) -> Option<usize> {
    const NEEDLE: &[u8] = b"Filter::Sql";
    let mut i = 0;
    while i + NEEDLE.len() <= region.len() {
        if &region[i..i + NEEDLE.len()] == NEEDLE {
            let mut j = i + NEEDLE.len();
            while j < region.len() && region[j].is_ascii_whitespace() {
                j += 1;
            }
            if region.get(j) == Some(&b'(') {
                j += 1;
                while j < region.len() && region[j].is_ascii_whitespace() {
                    j += 1;
                }
                return Some(j);
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// reading the generated bindings
// ---------------------------------------------------------------------------------------------

/// Read `<bindings>/*_table.rs` into `table -> columns`, following each table handle to the row
/// struct it names. Same shape as the Python: a `*_table.rs` that does not carry both the doc
/// comment and the row `use` is skipped, not failed.
pub fn generated_schema(bindings_dir: &Path) -> (Schema, Vec<String>) {
    let mut schema: Schema = BTreeMap::new();
    let mut errors = Vec::new();

    let mut table_files: Vec<PathBuf> = std::fs::read_dir(bindings_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_table.rs"))
        })
        .collect();
    table_files.sort();

    for table_path in table_files {
        let Ok(table_text) = std::fs::read_to_string(&table_path) else {
            continue;
        };
        let (Some(table), Some((module, row))) =
            (documented_table(&table_text), row_use(&table_text))
        else {
            continue;
        };
        let type_path = bindings_dir.join(format!("{module}.rs"));
        if !type_path.is_file() {
            errors.push(format!(
                "{}: generated row module {module}.rs is missing",
                table_path.display()
            ));
            continue;
        }
        let Ok(type_text) = std::fs::read_to_string(&type_path) else {
            errors.push(format!(
                "{}: generated row module could not be read",
                type_path.display()
            ));
            continue;
        };
        let Some(body) = struct_body(&type_text, &row) else {
            errors.push(format!(
                "{}: generated row struct {row} is missing",
                type_path.display()
            ));
            continue;
        };
        let columns = public_fields(body);
        if columns.is_empty() {
            errors.push(format!(
                "{}: generated row struct has no columns",
                type_path.display()
            ));
            continue;
        }
        schema.insert(table, columns);
    }

    if schema.is_empty() {
        errors.push(format!(
            "{}: no generated table bindings found",
            bindings_dir.display()
        ));
    }
    (schema, errors)
}

/// ``Table handle for the table `game_character`.``
fn documented_table(text: &str) -> Option<String> {
    let after = text.split_once("Table handle for the table `")?.1;
    let name = after.split_once('`')?.0;
    (!name.is_empty() && name.bytes().all(is_word_byte)).then(|| name.to_string())
}

/// `use super::character_type::Character;`
fn row_use(text: &str) -> Option<(String, String)> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("use super::") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(';') else {
            continue;
        };
        let Some((module, row)) = rest.split_once("::") else {
            continue;
        };
        if !module.is_empty()
            && !row.is_empty()
            && module.bytes().all(is_word_byte)
            && row.bytes().all(is_word_byte)
        {
            return Some((module.to_string(), row.to_string()));
        }
    }
    None
}

/// The body of `pub struct <row> { … }`, up to the first line that begins with `}` in column 0.
fn struct_body<'a>(text: &'a str, row: &str) -> Option<&'a str> {
    let needle = format!("pub struct {row}");
    let mut search = 0usize;
    while let Some(found) = text[search..].find(&needle) {
        let at = search + found;
        // `pub struct Foo` must not match `pub struct FooBar`.
        let after = at + needle.len();
        let next = text.as_bytes().get(after).copied();
        if next.is_some_and(is_word_byte) {
            search = after;
            continue;
        }
        let mut i = after;
        while text
            .as_bytes()
            .get(i)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            i += 1;
        }
        if text.as_bytes().get(i) != Some(&b'{') {
            search = after;
            continue;
        }
        let body_start = i + 1;
        // `^\}` with MULTILINE: a closing brace at the start of a line.
        let rest = &text[body_start..];
        let mut offset = 0usize;
        for line in rest.split_inclusive('\n') {
            if line.starts_with('}') {
                return Some(&rest[..offset]);
            }
            offset += line.len();
        }
        return Some(rest);
    }
    None
}

/// `    pub guid: u64,` -> `guid`.
fn public_fields(body: &str) -> BTreeSet<String> {
    let mut columns = BTreeSet::new();
    for line in body.lines() {
        let Some(rest) = line.trim_start().strip_prefix("pub ") else {
            continue;
        };
        let rest = rest.trim_start();
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if rest[name.len()..].trim_start().starts_with(':') {
            columns.insert(name);
        }
    }
    columns
}

// ---------------------------------------------------------------------------------------------
// validating one filter's SQL
// ---------------------------------------------------------------------------------------------

struct TableRef {
    table_start: usize,
    table_end: usize,
    alias: Option<(usize, usize)>,
    /// Where the whole `FROM …`/`JOIN …` declaration starts (the keyword itself).
    start: usize,
}

/// Every `FROM`/`JOIN <table> [[AS] <alias>]` in `sql`.
///
/// The scan is NON-OVERLAPPING, exactly like the Python's `finditer`: a match consumes its optional
/// alias slot even when that slot turns out to hold a keyword. `FROM a JOIN b` therefore swallows
/// the `JOIN` (as a rejected alias) and never registers `b`. That is upstream behaviour, preserved
/// deliberately — every filter in the module is a single-table `SELECT … FROM t WHERE …`, so no
/// verdict in this repository depends on it, and diverging would make the two validators disagree.
fn table_refs(sql: &[u8], tokens: &[Word]) -> Vec<TableRef> {
    let mut refs = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let keyword = word_text(sql, &tokens[index]);
        if !(keyword.eq_ignore_ascii_case("from") || keyword.eq_ignore_ascii_case("join")) {
            index += 1;
            continue;
        }
        let Some(table) = tokens.get(index + 1) else {
            index += 1;
            continue;
        };
        if !separated_by_space(sql, tokens[index].end, table.start) || !is_identifier(sql, table) {
            index += 1;
            continue;
        }

        // `(?:\s+(?:AS\s+)?(?P<alias>IDENT))?`
        let mut consumed = index + 1;
        let mut alias = None;
        if let Some(next) = tokens.get(index + 2) {
            if separated_by_space(sql, table.end, next.start) && is_identifier(sql, next) {
                if word_text(sql, next).eq_ignore_ascii_case("as") {
                    if let Some(after_as) = tokens.get(index + 3) {
                        if separated_by_space(sql, next.end, after_as.start)
                            && is_identifier(sql, after_as)
                        {
                            alias = Some((after_as.start, after_as.end));
                            consumed = index + 3;
                        }
                    }
                } else {
                    alias = Some((next.start, next.end));
                    consumed = index + 2;
                }
            }
        }

        refs.push(TableRef {
            start: tokens[index].start,
            table_start: table.start,
            table_end: table.end,
            alias,
        });
        index = consumed + 1;
    }
    refs
}

/// Blank every `'…'` SQL string literal (with `''` as the escape), so its contents are never read
/// as identifiers.
fn blank_sql_strings(sql: &mut [u8]) {
    let mut i = 0;
    while i < sql.len() {
        if sql[i] != b'\'' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        loop {
            match sql.get(i) {
                None => {
                    // Unterminated: the Python's regex would not match it either.
                    return;
                }
                Some(b'\'') if sql.get(i + 1) == Some(&b'\'') => i += 2,
                Some(b'\'') => {
                    i += 1;
                    blank(sql, start, i);
                    break;
                }
                Some(_) => i += 1,
            }
        }
    }
}

/// Blank every `:parameter`.
fn blank_parameters(sql: &mut [u8]) {
    let mut i = 0;
    while i < sql.len() {
        if sql[i] == b':'
            && sql
                .get(i + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            let start = i;
            i += 1;
            while i < sql.len() && is_word_byte(sql[i]) {
                i += 1;
            }
            blank(sql, start, i);
        } else {
            i += 1;
        }
    }
}

pub fn validate_sql(item: &FilterSql, schema: &Schema) -> Vec<String> {
    let mut errors = Vec::new();
    let mut sql = item.sql.clone().into_bytes();
    blank_sql_strings(&mut sql);

    let tokens = words(&sql);
    let refs = table_refs(&sql, &tokens);
    if refs.is_empty() {
        return vec![format!(
            "{}: filter SQL has no FROM table: '{}'",
            item.location(),
            item.sql
        )];
    }

    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut referenced: Vec<String> = Vec::new();
    let mut declaration_spans: Vec<(usize, usize)> = Vec::new();

    for reference in &refs {
        let table = std::str::from_utf8(&sql[reference.table_start..reference.table_end])
            .unwrap_or("")
            .to_string();
        let alias = reference
            .alias
            .map(|(start, end)| {
                std::str::from_utf8(&sql[start..end])
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|alias| !is_keyword(alias));
        let declaration_end = match (alias.is_some(), reference.alias) {
            (true, Some((_, end))) => end,
            _ => reference.table_end,
        };
        declaration_spans.push((reference.start, declaration_end));

        if !schema.contains_key(&table) {
            errors.push(format!(
                "{}: unknown table `{table}` in filter SQL",
                item.location()
            ));
            continue;
        }
        referenced.push(table.clone());
        aliases.insert(table.clone(), table.clone());
        if let Some(alias) = alias {
            aliases.insert(alias, table);
        }
    }

    // `owner.column` / `owner.*`
    let mut qualified_spans: Vec<(usize, usize)> = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let owner_token = tokens[index];
        if !is_identifier(&sql, &owner_token) {
            index += 1;
            continue;
        }
        let mut i = owner_token.end;
        while sql.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
            i += 1;
        }
        if sql.get(i) != Some(&b'.') {
            index += 1;
            continue;
        }
        i += 1;
        while sql.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
            i += 1;
        }
        let (column, span_end, next_index) = if sql.get(i) == Some(&b'*') {
            ("*".to_string(), i + 1, index + 1)
        } else {
            match tokens.get(index + 1).filter(|t| t.start == i) {
                Some(token) => (word_text(&sql, token).to_string(), token.end, index + 2),
                None => {
                    index += 1;
                    continue;
                }
            }
        };
        qualified_spans.push((owner_token.start, span_end));
        let owner = word_text(&sql, &owner_token).to_string();
        let rendered = std::str::from_utf8(&sql[owner_token.start..span_end]).unwrap_or("");
        match aliases.get(&owner) {
            None => errors.push(format!(
                "{}: unknown table or alias `{owner}` in `{rendered}`",
                item.location()
            )),
            Some(table) => {
                if column != "*" && !schema[table].contains(&column) {
                    errors.push(format!(
                        "{}: table `{table}` has no column `{column}`",
                        item.location()
                    ));
                }
            }
        }
        index = next_index;
    }

    // Whatever is left must be a column of one of the referenced tables.
    let mut remaining = sql.clone();
    blank_parameters(&mut remaining);
    for (start, end) in declaration_spans.into_iter().chain(qualified_spans) {
        blank(&mut remaining, start, end);
    }
    let columns: BTreeSet<&String> = referenced
        .iter()
        .flat_map(|table| schema[table].iter())
        .collect();

    for token in words(&remaining) {
        if !is_identifier(&remaining, &token) {
            continue;
        }
        let name = word_text(&remaining, &token);
        if is_keyword(name) || aliases.contains_key(name) || referenced.iter().any(|t| t == name) {
            continue;
        }
        // A SQL function name; its identifier arguments are validated on their own.
        let tail = std::str::from_utf8(&remaining[token.end..]).unwrap_or("");
        if tail.trim_start().starts_with('(') {
            continue;
        }
        if !columns.iter().any(|column| column.as_str() == name) {
            errors.push(format!(
                "{}: referenced tables have no column `{name}`",
                item.location()
            ));
        }
    }
    errors
}

/// Validate every filter under `module_dir` against the bindings in `bindings_dir`.
///
/// Returns how many filters were checked, and every error found — the Python reports all of them
/// rather than stopping at the first, and so does this.
pub fn validate(bindings_dir: &Path, module_dir: &Path) -> (usize, Vec<String>) {
    let (schema, mut errors) = generated_schema(bindings_dir);
    let (filters, filter_errors) = find_filters(module_dir);
    errors.extend(filter_errors);
    if !schema.is_empty() {
        for item in &filters {
            errors.extend(validate_sql(item, &schema));
        }
    }
    (filters.len(), errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The Python's `write_fixture`: one generated table with two columns, and one module source
    /// declaring a single filter over it.
    fn fixture(root: &Path, sql: &str, attribute: &str) -> (PathBuf, PathBuf) {
        let bindings = root.join("bindings");
        let module = root.join("module");
        std::fs::create_dir_all(&bindings).unwrap();
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(
            bindings.join("game_character_table.rs"),
            "use super::character_type::Character;\n\
             /// Table handle for the table `game_character`.\n",
        )
        .unwrap();
        std::fs::write(
            bindings.join("character_type.rs"),
            "pub struct Character {\n pub guid: u64,\n pub owner_identity: Identity,\n}\n",
        )
        .unwrap();
        std::fs::write(
            module.join("lib.rs"),
            format!("{attribute}\nconst RLS: Filter = Filter::Sql(\"{sql}\");\n"),
        )
        .unwrap();
        (bindings, module)
    }

    fn check(sql: &str) -> (usize, Vec<String>) {
        check_with(sql, "#[client_visibility_filter]")
    }

    fn check_with(sql: &str, attribute: &str) -> (usize, Vec<String>) {
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), sql, attribute);
        validate(&bindings, &module)
    }

    /// The Python's own `--self-test` table, case for case. These are the contract.
    #[test]
    fn the_python_validators_self_test_cases_get_the_same_verdicts() {
        let cases: [(&str, bool, &str); 8] = [
            (
                "SELECT * FROM game_character WHERE owner_identity = :sender",
                true,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT c.* FROM game_character AS c WHERE c.owner_identity = :sender",
                true,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT c.* FROM game_character c JOIN game_character o ON o.guid = c.guid \
                 WHERE c.owner_identity = :sender",
                true,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT * FROM game_character WHERE no_such_column = :sender",
                false,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT * FROM no_such_table WHERE owner_identity = :sender",
                false,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT * FROM game_character c WHERE nope.owner_identity = :sender",
                false,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT * FROM game_character WHERE owner_identity = 'no_such_column'",
                true,
                "#[client_visibility_filter]",
            ),
            (
                "SELECT * FROM game_character WHERE owner_identity = :sender",
                true,
                "#[spacetimedb::client_visibility_filter]",
            ),
        ];
        for (sql, expected_valid, attribute) in cases {
            let (count, errors) = check_with(sql, attribute);
            let valid = count == 1 && errors.is_empty();
            assert_eq!(valid, expected_valid, "case {sql:?}: errors={errors:?}");
        }
    }

    #[test]
    fn an_unknown_column_names_the_column_and_the_source_line() {
        let (_, errors) = check("SELECT * FROM game_character WHERE nope = :sender");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("no column `nope`"), "{errors:?}");
        // The location is the ATTRIBUTE's line, which is where a reader looks for the filter.
        assert!(errors[0].contains("lib.rs:1"), "{errors:?}");
    }

    #[test]
    fn a_qualified_column_is_checked_against_the_table_its_alias_names() {
        let (_, errors) = check("SELECT * FROM game_character AS c WHERE c.no_such = :sender");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("table `game_character` has no column `no_such`"),
            "{errors:?}"
        );
    }

    #[test]
    fn a_filter_with_no_from_clause_is_reported_rather_than_passed() {
        let (_, errors) = check("SELECT 1");
        assert!(
            errors.iter().any(|e| e.contains("no FROM table")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_function_call_is_not_mistaken_for_a_column() {
        // `lower(owner_identity)` — the function name is skipped, the argument is still checked.
        let (_, ok) = check("SELECT * FROM game_character WHERE lower(owner_identity) = :sender");
        assert!(ok.is_empty(), "{ok:?}");
        let (_, bad) = check("SELECT * FROM game_character WHERE lower(nope) = :sender");
        assert!(bad.iter().any(|e| e.contains("`nope`")), "{bad:?}");
    }

    #[test]
    fn a_module_with_no_filters_at_all_is_an_error_not_a_pass() {
        // The parser drifting away from the source layout must never read as "nothing to check".
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), "SELECT * FROM game_character", "#[table]");
        let (count, errors) = validate(&bindings, &module);
        assert_eq!(count, 0);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("parser or source layout drifted")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_filter_whose_argument_is_not_a_literal_is_reported() {
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), "SELECT * FROM game_character", "#[table]");
        std::fs::write(
            module.join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter = Filter::Sql(SQL_TEXT);\n",
        )
        .unwrap();
        let (_, errors) = validate(&bindings, &module);
        assert!(
            errors.iter().any(|e| e.contains("not a string literal")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_filter_with_no_filter_sql_call_at_all_is_reported() {
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), "SELECT * FROM game_character", "#[table]");
        std::fs::write(
            module.join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter = SOMETHING_ELSE;\n",
        )
        .unwrap();
        let (_, errors) = validate(&bindings, &module);
        assert!(
            errors.iter().any(|e| e.contains("no literal Filter::Sql")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_raw_string_literal_is_read_verbatim() {
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), "SELECT * FROM game_character", "#[table]");
        std::fs::write(
            module.join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter = Filter::Sql(r#\"SELECT * FROM \
             game_character WHERE owner_identity = :sender\"#);\n",
        )
        .unwrap();
        let (count, errors) = validate(&bindings, &module);
        assert_eq!(count, 1);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn empty_bindings_are_an_error_rather_than_a_silent_pass() {
        // Check 3 depends on check 2's output; an empty directory must not read as "all clear".
        let tmp = TempDir::new().unwrap();
        let (_, module) = fixture(
            tmp.path(),
            "SELECT * FROM game_character",
            "#[client_visibility_filter]",
        );
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let (_, errors) = validate(&empty, &module);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("no generated table bindings found")),
            "{errors:?}"
        );
    }

    #[test]
    fn several_filters_in_one_file_are_all_found() {
        let tmp = TempDir::new().unwrap();
        let (bindings, module) = fixture(tmp.path(), "SELECT * FROM game_character", "#[table]");
        std::fs::write(
            module.join("lib.rs"),
            "#[client_visibility_filter]\n\
             const A: Filter = Filter::Sql(\"SELECT * FROM game_character WHERE guid = :sender\");\n\
             \n\
             #[client_visibility_filter]\n\
             const B: Filter = Filter::Sql(\"SELECT * FROM game_character WHERE nope = :sender\");\n",
        )
        .unwrap();
        let (count, errors) = validate(&bindings, &module);
        assert_eq!(count, 2);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("lib.rs:4"), "{errors:?}");
    }

    #[test]
    fn a_row_struct_whose_module_is_missing_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let bindings = tmp.path().join("bindings");
        std::fs::create_dir_all(&bindings).unwrap();
        std::fs::write(
            bindings.join("game_character_table.rs"),
            "use super::character_type::Character;\n\
             /// Table handle for the table `game_character`.\n",
        )
        .unwrap();
        let (schema, errors) = generated_schema(&bindings);
        assert!(schema.is_empty());
        assert!(
            errors
                .iter()
                .any(|e| e.contains("character_type.rs is missing")),
            "{errors:?}"
        );
    }
}
