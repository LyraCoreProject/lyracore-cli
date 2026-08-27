//! `lyracore preflight` — the publish-shaped, OFFLINE deploy gate.
//!
//! This is the break class `cargo test` and `cargo check` cannot see. It is a port of the server
//! repo's `scripts/preflight.sh` (and the `validate-rls-filters.py` it drove), and it exists
//! because five breaks reached or threatened a live stack while green under the ordinary tests:
//!
//!   0. the dev toolchain / `spacetime` CLI drifting out from under the versions the checkout pins.
//!      A CLI ahead of the pin can publish a schema the repo never tested against; behind it,
//!      schema extraction rejects valid syntax with a confusing error instead of naming the drift.
//!   1. `module/src/debug.rs` reaching a fn through a private path — it compiles only under
//!      `--features=debug_reducers`, the feature `publish` bakes in and the default test config
//!      never uses.
//!   2. `#[default(0)]` on a u64 column: SpacetimeDB encodes a bare `0` as 4 bytes and REJECTS the
//!      migration. Only real schema extraction sees that.
//!   3. a `#[client_visibility_filter]` naming a nonexistent column: it survives schema extraction
//!      as raw text and then rejects a gateway subscription at LOGIN time.
//!   4. a script with a configurable `DB` target that reaches the assertions but not the tools it
//!      drives — an ETL writing to one database and asserting against another.
//!   5. a generated Package Delta artifact that no longer matches the Datascript source, generated
//!      typings, Base Snapshot, authoring library or toolchain pins it was built from — `publish`
//!      would ship a Package's stale claims to every Shard with nothing catching the drift.
//!
//! NOTHING HERE TOUCHES A NODE. No publish, no call, no sql, no database. It is safe to run against
//! a live stack, and `publish` runs it first for exactly that reason.
//!
//! Like the shell, every check runs even after one fails — a deploy gate that stopped at the first
//! problem would hand back one line at a time.

use crate::cmd::doctor::{parse_version, Version};
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::rls;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Set this to skip check 2 where `spacetimedb-standalone` (which schema extraction shells out to)
/// is unavailable. Same name as the shell's, so a documented workaround keeps working.
pub const SKIP_SCHEMA_VAR: &str = "PREFLIGHT_SKIP_SCHEMA";

/// A scratch directory that removes itself. `spacetime generate`'s output is read by check 3 and
/// then thrown away: this is NOT the gateway-binding regen of danger-zones §1.2, nothing in
/// `gateway/src/stdb/bindings/` is touched.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self> {
        let unique = format!(
            "lyracore-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn step(title: &str) {
    println!();
    println!("== {title}");
}

/// Collects failures so every check reports, then fails once at the end.
#[derive(Default)]
struct Failures(Vec<String>);

impl Failures {
    fn bad(&mut self, message: impl Into<String>) {
        let message = message.into();
        println!();
        println!("FAIL: {message}");
        self.0.push(message);
    }
}

// ---------------------------------------------------------------------------------------------
// check 0 — the pinned versions
// ---------------------------------------------------------------------------------------------

/// `channel = "1.93.0"` out of `rust-toolchain.toml`.
pub fn pinned_rust(toolchain_toml: &str) -> Option<Version> {
    let line = toolchain_toml
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.starts_with("channel"))?;
    version_from(line.split('"').nth(1)?)
}

/// `spacetimedb = { version = "=2.7.1", … }` out of `module/Cargo.toml`.
pub fn pinned_spacetimedb(module_manifest: &str) -> Option<Version> {
    let line = module_manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.starts_with("spacetimedb") && line.contains("version"))?;
    let after = line.split_once("version")?.1;
    let value = after.split('"').nth(1)?;
    version_from(value.trim_start_matches(['=', '^', '~']))
}

/// `1.93.0` or the legal two-component `1.93`.
fn version_from(value: &str) -> Option<Version> {
    let value = value.trim();
    if let Some(version) = parse_version(value) {
        return Some(version);
    }
    let mut parts = value.split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), None) => Some(Version(major, minor, 0)),
        _ => None,
    }
}

/// Whether the `spacetime` CLI is PRESENT, so checks 2 and 3 know whether to skip. Presence, not
/// agreement: a drifted CLI is reported here and still used below.
fn check_versions(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    failures: &mut Failures,
) -> bool {
    step("dev toolchain + spacetime CLI match the versions this repo is pinned to");

    let toolchain = std::fs::read_to_string(project.rust_toolchain_file()).unwrap_or_default();
    match pinned_rust(&toolchain) {
        None => failures.bad(format!(
            "could not read the pinned Rust channel out of {}",
            ProjectLayout::RUST_TOOLCHAIN
        )),
        Some(pinned) => {
            let found = runner
                .run_capturing_stderr(&CommandSpec::new("rustc").arg("--version"))
                .ok()
                .as_deref()
                .and_then(parse_version);
            match found {
                Some(found) if found == pinned => println!("rustc ok ({found})"),
                found => failures.bad(format!(
                    "rustc on PATH reports {}, {} pins {pinned}. If this is a rustup-managed \
                     toolchain, run `rustup show` from the repository root — it should auto-select \
                     {pinned} from {}. If `rustc` resolves to a NON-rustup install ahead of it on \
                     PATH, {} is being silently ignored.",
                    found.map_or_else(|| "unknown".to_string(), |v| v.to_string()),
                    ProjectLayout::RUST_TOOLCHAIN,
                    ProjectLayout::RUST_TOOLCHAIN,
                    ProjectLayout::RUST_TOOLCHAIN,
                )),
            }
        }
    }

    let manifest = std::fs::read_to_string(project.module_manifest()).unwrap_or_default();
    let Some(pinned) = pinned_spacetimedb(&manifest) else {
        failures.bad(format!(
            "could not read the pinned spacetimedb version out of {}/Cargo.toml",
            ProjectLayout::MODULE_DIR
        ));
        return false;
    };
    let Ok(banner) = runner.run_capturing_stderr(&CommandSpec::new("spacetime").arg("--version"))
    else {
        println!("FAIL: no `spacetime` on PATH — CLI version and schema not checked");
        failures.bad(format!(
            "`spacetime` is required for a complete deploy gate. Install the pinned {pinned} CLI:\n    \
             curl -sSf https://install.spacetimedb.com | sh -s -- --version {pinned}\n  \
             Then ensure `$HOME/.local/bin` is on PATH for non-interactive shells."
        ));
        return false;
    };
    match parse_version(&banner) {
        Some(found) if found == pinned => println!("spacetime CLI ok ({found})"),
        found => failures.bad(format!(
            "spacetime CLI reports {}, this repository is pinned to {pinned} \
             ({}/Cargo.toml, gateway/Cargo.toml). Install the matching CLI:\n    \
             curl -sSf https://install.spacetimedb.com | sh -s -- --version {pinned}",
            found.map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            ProjectLayout::MODULE_DIR,
        )),
    }
    // A MISMATCHED CLI still runs checks 2 and 3, deliberately. The drift is already reported
    // above; skipping schema extraction over it would silently drop the two checks that catch the
    // most expensive break classes, every run, until someone reinstalls a CLI. Presence is the
    // gate here, not agreement.
    true
}

// ---------------------------------------------------------------------------------------------
// check 1 — the deploy feature set builds
// ---------------------------------------------------------------------------------------------

fn check_deploy_build(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    failures: &mut Failures,
) {
    step("module builds with --features=debug_reducers (the feature publish bakes in)");
    match runner.run_and_wait(&deploy_check_command(project)) {
        Ok(_) => println!("ok"),
        Err(e) => {
            println!("{e}");
            failures.bad(
                "the module does not compile with --features=debug_reducers — `lyracore publish` \
                 would fail",
            );
        }
    }
}

pub fn deploy_check_command(project: &ProjectLayout) -> CommandSpec {
    // `--manifest-path` rather than a bare `-p`: `lyracore` runs from any subdirectory of a
    // checkout, and cargo would otherwise resolve the workspace from the caller's cwd.
    CommandSpec::new("cargo")
        .arg("check")
        .arg("-q")
        .arg("--manifest-path")
        .arg(
            project
                .root
                .join("Cargo.toml")
                .to_string_lossy()
                .to_string(),
        )
        .arg("-p")
        .arg(ProjectLayout::MODULE_PACKAGE)
        .arg(ProjectLayout::DEPLOY_FEATURES)
}

// ---------------------------------------------------------------------------------------------
// check 2 — real, offline schema extraction
// ---------------------------------------------------------------------------------------------

/// `spacetime generate` builds the wasm with our deploy features and extracts the module schema
/// through it — the same code path that rejects a bad `#[default]` literal, reproducing publish's
/// error verbatim. It needs no node and touches no database.
pub fn schema_command(project: &ProjectLayout, out_dir: &Path) -> CommandSpec {
    schema_command_for_language(project, out_dir, "rust")
}

/// Build the one schema-extraction command shared by preflight and Package Datascript builds.
///
/// The language and destination differ, but the module path, deploy feature set and
/// non-interactive overwrite policy must stay identical: both commands describe the same wasm.
pub(crate) fn schema_command_for_language(
    project: &ProjectLayout,
    out_dir: &Path,
    language: &str,
) -> CommandSpec {
    CommandSpec::new("spacetime")
        .arg("generate")
        .arg("--lang")
        .arg(language)
        .arg("--module-path")
        .arg(project.module_dir().to_string_lossy().to_string())
        .arg("--out-dir")
        .arg(out_dir.to_string_lossy().to_string())
        .arg(format!(
            "--build-options={}",
            ProjectLayout::DEPLOY_FEATURES
        ))
        // The invocation is fully specified. Ignore any spacetime.json found by walking upward
        // from the contributor's caller directory; its generate entries could change the output.
        .arg("--no-config")
        .arg("-y")
        .cwd(&project.root)
}

/// The verdict lines out of a chatty extractor log, falling back to the whole thing.
fn verdict_lines(text: &str) -> String {
    let mut verdicts = Vec::new();
    let mut in_cause = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_verdict = ["error", "Error", "Failed to", "Caused by"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if is_verdict {
            verdicts.push(line);
            in_cause = trimmed.starts_with("Caused by");
        } else if in_cause && trimmed.len() < line.len() && !trimmed.is_empty() {
            verdicts.push(line);
        } else if !trimmed.is_empty() {
            in_cause = false;
        }
        if verdicts.len() == 10 {
            break;
        }
    }
    if verdicts.is_empty() {
        text.trim().to_string()
    } else {
        verdicts.join("\n")
    }
}

fn check_schema(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    scratch: &Path,
    have_cli: bool,
    skip_schema: bool,
    failures: &mut Failures,
) -> bool {
    step("module schema + #[default] values validate (offline wasm schema extraction)");
    if !have_cli {
        println!("SKIP: no `spacetime` on PATH — schema not validated");
        return false;
    }
    if skip_schema {
        println!("SKIP: {SKIP_SCHEMA_VAR} is set — schema not validated");
        failures.bad(format!(
            "{SKIP_SCHEMA_VAR} bypassed the deployment-critical schema check; this preflight \
             cannot approve a publish"
        ));
        return false;
    }
    match runner.run_and_wait(&schema_command(project, scratch)) {
        Ok(_) => {
            println!("ok");
            true
        }
        Err(e) => {
            println!("{}", verdict_lines(&e.to_string()));
            failures.bad(format!(
                "the module schema is invalid — `spacetime publish` would reject this migration \
                 (see above). If the failure above is the EXTRACTOR itself (no \
                 spacetimedb-standalone), re-run with {SKIP_SCHEMA_VAR}=1 to collect the remaining \
                 diagnostics; preflight will still fail because nothing validated your #[default] \
                 encodings."
            ));
            false
        }
    }
}

// ---------------------------------------------------------------------------------------------
// check 3 — RLS filter identifiers
// ---------------------------------------------------------------------------------------------

fn check_rls(project: &ProjectLayout, bindings: Option<&Path>, failures: &mut Failures) {
    step("client visibility filters name real schema tables and columns");
    let Some(bindings) = bindings else {
        println!("SKIP: schema bindings were not generated — RLS identifiers cannot be validated");
        return;
    };
    let (count, errors) = rls::validate(bindings, &project.module_dir());
    if errors.is_empty() {
        println!("RLS filter validation OK — {count} filters checked");
        return;
    }
    for error in &errors {
        println!("RLS ERROR: {error}");
    }
    failures.bad(
        "a client_visibility_filter names an unknown table or column — login subscriptions would \
         fail",
    );
}

// ---------------------------------------------------------------------------------------------
// check 4 — a configurable DB target must reach every tool
// ---------------------------------------------------------------------------------------------

/// Rendered invocation lines in `script` that drive a database-aware tool without threading the
/// script's own `$DB` override into it.
///
/// Only scripts that DEFINE an override are worth checking: one with no override always hits the
/// default database, which is a different (deliberate) thing. Comments are stripped and backslash
/// continuations joined first, because a tool named in prose is not an invocation and a real
/// invocation spans lines and may carry `--db "$DB"` on a later one.
pub fn db_threading_offenders(script: &str) -> Vec<String> {
    if !script.lines().any(defines_db_override) {
        return Vec::new();
    }
    join_continuations(script)
        .into_iter()
        .filter(|line| drives_a_database_tool(line))
        .filter(|line| !threads_db(line))
        .collect()
}

fn defines_db_override(line: &str) -> bool {
    let name: String = line
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect();
    name.ends_with("DB") && line[name.len()..].starts_with('=')
}

/// The shell-visible part of one physical line: everything before an unquoted `#` that starts a
/// word. A `#` inside quotes or glued to the previous word is data, not a comment. Stripping runs
/// before continuation joining because a backslash inside a comment does not continue the line.
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let (mut single, mut double) = (false, false);
    let mut word_start = true;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if !single => {
                word_start = false;
                at += 2;
                continue;
            }
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'#' if !single && !double && word_start => return line[..at].trim_end(),
            _ => {}
        }
        word_start = matches!(bytes[at], b' ' | b'\t');
        at += 1;
    }
    line
}

fn join_continuations(script: &str) -> Vec<String> {
    let mut joined: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for line in script.lines().map(strip_inline_comment) {
        let continues = line.ends_with('\\');
        let piece = if continues {
            format!("{} ", &line[..line.len() - 1])
        } else {
            line.to_string()
        };
        let current = match pending.take() {
            Some(previous) => previous + &piece,
            None => piece,
        };
        if continues {
            pending = Some(current);
        } else {
            joined.push(current);
        }
    }
    if let Some(last) = pending {
        joined.push(last);
    }
    joined
}

fn drives_a_database_tool(line: &str) -> bool {
    if line.contains("target/debug/lyracore-importer") || line.contains("target/debug/bench") {
        return true;
    }
    let Some(after) = line.split_once("spacetime") else {
        return false;
    };
    let rest = after.1.trim_start_matches([' ', '\t']);
    if rest.len() == after.1.len() {
        return false; // `spacetime` must be followed by whitespace, not `spacetimedb`
    }
    ["sql", "call", "logs", "describe"]
        .iter()
        .any(|verb| rest.starts_with(verb))
}

/// `$DB` / `${DB` — with a word boundary, so `"$DBC"` (the DBC path) does not count.
fn threads_db(line: &str) -> bool {
    let bytes = line.as_bytes();
    for (index, _) in line.match_indices("$") {
        let mut at = index + 1;
        if bytes.get(at) == Some(&b'{') {
            at += 1;
        }
        if bytes.get(at..at + 2) != Some(b"DB") {
            continue;
        }
        let after = bytes.get(at + 2).copied();
        if !after.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return true;
        }
    }
    false
}

/// The `*.sh` files in one directory, sorted. A directory that is not there yields none — a
/// checkout predating a move should skip that root, not fail the check.
fn shell_scripts_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut scripts: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "sh"))
        .collect();
    scripts.sort();
    scripts
}

// ---------------------------------------------------------------------------------------------
// check 5 — every generated Package Delta artifact still matches its Build Identity
// ---------------------------------------------------------------------------------------------

/// The same verification `lyracore packages check` runs standalone, folded into the gate `publish`
/// already calls — see `packages::check` for what "stale" means and how a missing Base Snapshot is
/// reported without failing the gate over it.
fn check_datascript_identity(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    failures: &mut Failures,
) {
    step("generated Package Delta artifacts match their recorded Build Identity");
    if let Err(e) = crate::cmd::packages::check::run(project, runner) {
        println!("{e}");
        failures.bad(
            "one or more Package Delta artifacts are stale — `lyracore packages build` \
             regenerates them",
        );
    }
}

fn check_db_threading(project: &ProjectLayout, failures: &mut Failures) {
    step("scripts thread their configurable DB target into every tool invocation");
    // BOTH script roots. The world-import ETL — the scripts that actually take a `DB` override and
    // drive the importer with it — lives with the importer, not in `scripts/`; a scan that looked
    // only at `scripts/` would find no `DB=`-defining script at all and print a vacuous "ok".
    let mut scripts = shell_scripts_in(&project.scripts_dir());
    scripts.extend(shell_scripts_in(&project.importer_scripts_dir()));
    if scripts.is_empty() {
        println!(
            "SKIP: no {}/ or {}/ scripts in this checkout",
            ProjectLayout::SCRIPTS_DIR,
            ProjectLayout::IMPORTER_SCRIPTS_DIR
        );
        return;
    }

    let mut ok = true;
    for path in scripts {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let offenders = db_threading_offenders(&text);
        if offenders.is_empty() {
            continue;
        }
        ok = false;
        // Offenders are reported as TEXT, not line numbers: continuations were joined.
        println!("{}:", path.display());
        for offender in offenders {
            println!("{offender}");
        }
    }
    if ok {
        println!("ok");
    } else {
        failures.bad(
            "the invocation(s) above ignore their script's $DB override — they will hit the \
             default database",
        );
    }
}

// ---------------------------------------------------------------------------------------------

/// Run every check. Returns `Err` if any of them failed.
pub fn run(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    run_with_schema_skip(project, runner, std::env::var_os(SKIP_SCHEMA_VAR).is_some())
}

fn run_with_schema_skip(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    skip_schema: bool,
) -> Result<()> {
    let mut failures = Failures::default();

    let have_cli = check_versions(project, runner, &mut failures);
    check_deploy_build(project, runner, &mut failures);

    let scratch = ScratchDir::new()?;
    let generated = check_schema(
        project,
        runner,
        scratch.path(),
        have_cli,
        skip_schema,
        &mut failures,
    );
    check_rls(project, generated.then(|| scratch.path()), &mut failures);
    check_db_threading(project, &mut failures);
    check_datascript_identity(project, runner, &mut failures);

    println!();
    if failures.0.is_empty() {
        println!("PREFLIGHT OK — safe to run `lyracore publish`");
        return Ok(());
    }
    println!("PREFLIGHT FAILED — do not publish");
    Err(Error::Process(format!(
        "preflight found {} problem(s) — see above. Nothing was published.",
        failures.0.len()
    )))
}

/// What is NOT covered (needs a live node, deliberately out of scope):
///   * whether a migration is additive or breaking — publish decides that against the DEPLOYED
///     schema, which cannot be read offline;
///   * gateway<->module binding drift, RLS semantics beyond identifier validity, reducer runtime
///     behaviour, world data.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{FakeStack, FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
    use tempfile::TempDir;

    // ---- the version gate ----

    #[test]
    fn the_pinned_versions_are_read_out_of_the_checkouts_own_files() {
        assert_eq!(
            pinned_rust("# a comment\n[toolchain]\nchannel = \"1.93.0\"\n"),
            Some(Version(1, 93, 0))
        );
        assert_eq!(pinned_rust("channel = \"1.85\"\n"), Some(Version(1, 85, 0)));
        assert_eq!(pinned_rust("[toolchain]\ncomponents = []\n"), None);
        assert_eq!(pinned_rust("channel = \"stable\"\n"), None);

        assert_eq!(
            pinned_spacetimedb(
                "[dependencies]\nspacetimedb = { version = \"=2.7.1\", features = [\"unstable\"] }\n"
            ),
            Some(Version(2, 7, 1))
        );
        assert_eq!(pinned_spacetimedb("[package]\nname = \"x\"\n"), None);
    }

    #[test]
    fn the_version_gate_is_an_exact_match_not_a_minimum() {
        // A CLI AHEAD of the pin can publish a schema this repository never tested against, so
        // "newer is fine" is exactly the wrong rule here — unlike `doctor`, which asks a floor.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        std::fs::write(
            project.rust_toolchain_file(),
            "[toolchain]\nchannel = \"1.92.0\"\n",
        )
        .unwrap();
        let stack = FakeStack::new();
        let mut failures = Failures::default();
        check_versions(&project, &stack.runner(), &mut failures);
        assert_eq!(failures.0.len(), 1, "{:?}", failures.0);
        assert!(
            failures.0[0].contains(FAKE_RUST_VERSION),
            "{:?}",
            failures.0
        );
        assert!(
            failures.0[0].contains("rust-toolchain.toml"),
            "the message must name the file to fix: {:?}",
            failures.0
        );
    }

    #[test]
    fn a_machine_without_the_spacetime_cli_fails_but_keeps_running_other_checks() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new().fail_on("spacetime --version", "command not found");
        let mut failures = Failures::default();
        let have_cli = check_versions(&project, &stack.runner(), &mut failures);
        assert!(!have_cli);
        assert_eq!(failures.0.len(), 1, "{:?}", failures.0);
        assert!(failures.0[0].contains("required"), "{:?}", failures.0);
        assert!(
            failures.0[0].contains(".local/bin"),
            "missing-tool guidance must name the non-interactive SSH PATH location: {:?}",
            failures.0
        );
    }

    #[test]
    fn a_spacetime_cli_that_does_not_match_the_pin_is_a_failure_with_an_install_line() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new().with_stdout(
            "spacetime --version",
            "spacetimedb tool version 2.4.0; spacetime-lib version 2.4.0",
        );
        let mut failures = Failures::default();
        let have_cli = check_versions(&project, &stack.runner(), &mut failures);
        assert_eq!(failures.0.len(), 1, "{:?}", failures.0);
        assert!(
            failures.0[0].contains("install.spacetimedb.com"),
            "{:?}",
            failures.0
        );
        // ...and the drift does NOT cost you checks 2 and 3. Skipping them over a version this run
        // has already reported would quietly halve the gate on every machine that has drifted —
        // and the shell preflight this ports gates check 2 on PRESENCE, not agreement.
        assert!(
            have_cli,
            "a mismatched CLI must still be used to extract a schema"
        );
    }

    // ---- the commands the checks render ----

    #[test]
    fn extractor_verdict_keeps_an_indented_cause_without_chatty_build_output() {
        let output = verdict_lines(
            "Compiling spacetimedb-sdk v2.7.1\nError: could not extract schema\nCaused by:\n    \
             spacetimedb-standalone exited before writing the schema\n    executable was not found \
             on PATH\nFinished release build\n",
        );

        assert_eq!(
            output,
            "Error: could not extract schema\nCaused by:\n    spacetimedb-standalone exited before \
             writing the schema\n    executable was not found on PATH"
        );
    }

    #[test]
    fn the_deploy_build_uses_the_feature_publish_bakes_in() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let rendered = deploy_check_command(&project).render();
        assert!(rendered.contains("--features=debug_reducers"), "{rendered}");
        assert!(
            rendered.contains(ProjectLayout::MODULE_PACKAGE),
            "{rendered}"
        );
        // cwd-independent: `lyracore` runs from any subdirectory of a checkout.
        assert!(rendered.contains("--manifest-path"), "{rendered}");
    }

    #[test]
    fn schema_extraction_is_offline_and_writes_nothing_into_the_checkout() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let scratch = ScratchDir::new().unwrap();
        let rendered = schema_command(&project, scratch.path()).render();
        assert!(rendered.contains("spacetime generate"), "{rendered}");
        assert!(
            rendered.contains("--build-options=--features=debug_reducers"),
            "{rendered}"
        );
        // NOT the gateway-binding regen: no --include-private, and the output is a scratch dir.
        assert!(!rendered.contains("--include-private"), "{rendered}");
        assert!(
            !rendered.contains("gateway"),
            "nothing in gateway/src/stdb/bindings/ may be touched: {rendered}"
        );
        // Offline: it must never speak to a node.
        for forbidden in ["publish", "call", " sql", "delete", "-c "] {
            assert!(
                !rendered.contains(forbidden),
                "{rendered} must not {forbidden}"
            );
        }
    }

    #[test]
    fn a_scratch_directory_removes_itself() {
        let path = {
            let scratch = ScratchDir::new().unwrap();
            std::fs::write(scratch.path().join("x"), "y").unwrap();
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "{} survived", path.display());
    }

    // ---- check 4 ----

    #[test]
    fn a_script_that_drops_its_db_override_is_an_offender() {
        // The real break: `DB` reached the assertions but not the importer, so the ETL was a
        // silent no-op against the intended database.
        let script = "#!/bin/sh\nDB=${DB:-lyracore}\n\
             ./target/debug/lyracore-importer --creatures\n\
             spacetime sql \"$DB\" 'SELECT 1'\n";
        let offenders = db_threading_offenders(script);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(offenders[0].contains("lyracore-importer"), "{offenders:?}");
    }

    #[test]
    fn a_threaded_target_on_a_continuation_line_is_not_an_offender() {
        // A real invocation spans lines and may carry `--db "$DB"` on a later one.
        let script = "DB=lyracore\n\
             ./target/debug/lyracore-importer \\\n  --creatures \\\n  --db \"$DB\"\n";
        assert!(db_threading_offenders(script).is_empty());
    }

    #[test]
    fn a_script_with_no_override_is_not_checked_at_all() {
        // No override means "always the default database" — deliberate, not a bug.
        let script = "#!/bin/sh\n./target/debug/lyracore-importer --creatures\n";
        assert!(db_threading_offenders(script).is_empty());
    }

    #[test]
    fn a_commented_out_invocation_and_a_dbc_path_are_not_offenders() {
        let script = "DB=lyracore\n\
             # spacetime sql 'SELECT 1'\n\
             ./target/debug/lyracore-importer --dbc \"$DBC\" --db \"$DB\"\n";
        assert!(db_threading_offenders(script).is_empty());

        // ...but `$DBC` alone must NOT satisfy the threading requirement.
        let sneaky = "DB=lyracore\n./target/debug/lyracore-importer --dbc \"$DBC\"\n";
        assert_eq!(db_threading_offenders(sneaky).len(), 1);
    }

    #[test]
    fn a_tool_named_in_an_inline_comment_is_not_an_offender() {
        // The LyraCore#326 false positive: a function-definition line whose trailing comment
        // mentions `spacetime sql` flagged a script that threads $DB in every real invocation.
        let script = "DB=${DB:-lyracore}\n\
             verify_eventai_catalogue() { # O(1) `spacetime sql` calls: six, whatever the rule count is.\n\
             spacetime sql \"$DB\" 'SELECT 1'\n";
        assert!(db_threading_offenders(script).is_empty());
    }

    #[test]
    fn an_inline_comment_neither_hides_an_offender_nor_truncates_its_line() {
        // The code before the comment is still checked...
        let script = "DB=lyracore\nspacetime sql 'SELECT 1' # threads nothing\n";
        assert_eq!(db_threading_offenders(script).len(), 1);

        // ...and a quoted `#` is data: the `$DB` after it must still count as threaded.
        let quoted = "DB=lyracore\nspacetime sql \"SELECT '#'\" \"$DB\"\n";
        assert!(db_threading_offenders(quoted).is_empty());
    }

    #[test]
    fn only_database_aware_verbs_are_matched() {
        let script = "DB=lyracore\nspacetime version\nspacetime logs\n";
        let offenders = db_threading_offenders(script);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(offenders[0].contains("logs"), "{offenders:?}");
    }

    // ---- the whole gate ----

    /// A checkout shaped like the real one: the pinned versions the fake toolchain reports, a
    /// module whose one filter is valid against the bindings the fake generator writes.
    fn fixture(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!(
                "[dependencies]\nspacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst CHARACTER_RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        ProjectLayout::from_root(root).unwrap()
    }

    #[test]
    fn a_healthy_checkout_passes_every_check() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new();
        run(&project, &stack.runner()).unwrap();
    }

    #[test]
    fn every_check_runs_even_after_one_fails() {
        // A deploy gate that stopped at the first problem would hand back one line at a time.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        std::fs::write(project.rust_toolchain_file(), "channel = \"1.0.0\"\n").unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();
        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        let rendered = stack.rendered();
        assert!(
            rendered.iter().any(|r| r.contains("cargo check")),
            "check 1 must still have run: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|r| r.contains("spacetime generate")),
            "check 2 must still have run: {rendered:?}"
        );
    }

    #[test]
    fn missing_spacetime_fails_the_whole_gate_after_independent_checks_run() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new().fail_on("spacetime --version", "command not found");

        let error = run(&project, &stack.runner()).unwrap_err();
        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        let rendered = stack.rendered();
        assert!(
            rendered
                .iter()
                .any(|command| command.contains("cargo check")),
            "the independent deploy build must still run: {rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|command| command.contains("spacetime generate")),
            "schema extraction requires the missing CLI: {rendered:?}"
        );
    }

    #[test]
    fn explicitly_skipped_schema_is_a_blocker_after_independent_checks_run() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new();

        let error = run_with_schema_skip(&project, &stack.runner(), true).unwrap_err();
        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
        let rendered = stack.rendered();
        assert!(
            rendered
                .iter()
                .any(|command| command.contains("cargo check")),
            "the independent deploy build must still run: {rendered:?}"
        );
        assert!(
            !rendered
                .iter()
                .any(|command| command.contains("spacetime generate")),
            "the explicit skip must bypass extraction: {rendered:?}"
        );
    }

    #[test]
    fn a_filter_naming_a_column_that_does_not_exist_fails_the_gate() {
        // The break this check exists for: it survives schema extraction as raw text and rejects a
        // gateway SUBSCRIPTION at login time.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        std::fs::write(
            project.module_sources().join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE no_such_column = :sender\");\n",
        )
        .unwrap();
        let stack = FakeStack::new();
        let error = run(&project, &stack.runner()).unwrap_err();
        assert!(error.to_string().contains("1 problem"), "{error}");
    }

    #[test]
    fn a_module_that_does_not_build_under_the_deploy_features_fails_the_gate() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new().fail_on("cargo check", "private path");
        assert!(run(&project, &stack.runner()).is_err());
    }

    // ---- check 5: the Datascript Build Identity gate ----

    #[test]
    fn the_identity_gate_is_a_no_op_when_no_package_carries_a_generated_artifact() {
        // `fixture()` has no `packages/` at all. `preflight` must still pass — this check exists
        // to catch stale Package Deltas, not to demand that every checkout have one.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new();
        run(&project, &stack.runner()).unwrap();
    }

    #[test]
    fn a_stale_package_delta_fails_the_whole_gate() {
        // A generated artifact with no Build Identity sidecar predates identity tracking — stale by
        // definition (LyraCore#316). `publish` must never ship it to a Shard.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let generated = project.packages_dir().join("fire_nova/data/.generated");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(
            generated.join("spell.json"),
            format!(
                r#"{{"version":1,"package":"fire_nova","source_hash":"{}","claims":[{{"table":"game_spell","key":{{"spell_id":133}},"operation":"update","fields":{{"gcd_ms":{{"type":"u32","value":1500}}}}}}]}}"#,
                "a".repeat(64)
            ),
        )
        .unwrap();
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner()).unwrap_err();

        assert!(
            error.to_string().contains("Nothing was published"),
            "{error}"
        );
    }

    #[test]
    fn nothing_in_the_gate_touches_a_node() {
        // The whole point: it is safe to run against a live stack.
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        let stack = FakeStack::new();
        run(&project, &stack.runner()).unwrap();
        for rendered in stack.rendered() {
            for forbidden in ["publish", "spacetime call", "spacetime sql", "delete"] {
                assert!(
                    !rendered.contains(forbidden),
                    "{rendered} must not {forbidden}"
                );
            }
        }
    }

    #[test]
    fn a_failed_schema_extraction_skips_the_rls_check_rather_than_inventing_a_verdict() {
        let tmp = TempDir::new().unwrap();
        let project = fixture(&tmp);
        // A filter that WOULD fail, plus an extractor that never produced bindings to check it
        // against: the gate must report the extraction, not a fabricated identifier error.
        std::fs::write(
            project.module_sources().join("lib.rs"),
            "#[client_visibility_filter]\nconst RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE no_such_column = :sender\");\n",
        )
        .unwrap();
        let stack = FakeStack::new().fail_on("spacetime generate", "data too short for u64");
        let error = run(&project, &stack.runner()).unwrap_err();
        assert!(error.to_string().contains("1 problem"), "{error}");
    }
}
