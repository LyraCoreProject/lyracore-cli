//! Resolve the PINNED wire-harness checkout that `dev smoke` runs through.
//!
//! The harness — the headless build-5875 wire client and the LyraCore orchestrators around it — is
//! its own repository (LyraCoreProject/wire-harness). A LyraCore checkout consumes a pinned RELEASE
//! of it, recorded in `.wire-harness-rev` as `<tag> <full sha>`. This module owns that consume path,
//! which used to be the server repo's `scripts/wire-harness.sh`:
//!
//! * the TAG is what gets cloned — a release, never a branch. There is deliberately no way to ask
//!   for `main` here;
//! * the SHA is what the checkout is then VERIFIED against, because a git tag is a mutable ref and
//!   "pinned to a tag someone moved" is not pinned;
//! * the clone lands in the git-ignored `.lyracore/wire-harness/<sha>/`, so a harness checkout can
//!   never show up in the server repo's `git status`;
//! * `LYRACORE_WIRE_HARNESS_DIR` overrides all of it with a local working tree — validated, and
//!   ANNOUNCED on stderr every time, because a stale local checkout silently substituted for the
//!   pin is a measurement nobody can reproduce.
//!
//! The clone uses ssh and the caller's existing git credentials (the repository is private).
//! Nothing here embeds a token.

use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Point the CLI at a local harness working tree instead of the pin.
pub const OVERRIDE_VAR: &str = "LYRACORE_WIRE_HARNESS_DIR";
/// Where the pinned release is cloned from. Overridable for mirrors and tests.
pub const REMOTE_VAR: &str = "LYRACORE_WIRE_HARNESS_REMOTE";
pub const DEFAULT_REMOTE: &str = "ssh://git@github.com/LyraCoreProject/wire-harness.git";

/// One `<tag> <sha>` pin line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub tag: String,
    pub sha: String,
}

/// A resolved harness checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness {
    pub dir: PathBuf,
    /// `None` when `LYRACORE_WIRE_HARNESS_DIR` was used: an override is not a pin.
    pub pin: Option<Pin>,
}

impl Harness {
    /// The adapter seam this CLI drives for `dev smoke`: the harness's own generic login smoke.
    pub fn smoke_seam(&self) -> PathBuf {
        self.dir.join(ProjectLayout::HARNESS_SMOKE_SEAM)
    }

    /// A full-suite entrypoint, if the release carries one. Historically this script lived in the
    /// SERVER repo (`adapters/lyracore/run-suite.sh`); a release that has absorbed it is preferred
    /// over driving the seam directly, because then the harness owns its own orchestration.
    pub fn suite_script(&self) -> Option<PathBuf> {
        let path = self.dir.join(ProjectLayout::HARNESS_SUITE_SCRIPT);
        path.is_file().then_some(path)
    }

    pub fn manifest(&self) -> PathBuf {
        self.dir.join("Cargo.toml")
    }

    pub fn client_bin(&self) -> PathBuf {
        self.dir
            .join("target/debug")
            .join(ProjectLayout::HARNESS_CLIENT_BIN)
    }
}

/// A harness checkout, identified by the things this CLI actually reaches for. Mirrors
/// `adapter-env.sh`'s `_is_lyracore` on the other side of the seam.
pub fn is_harness(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
        && dir.join("src").is_dir()
        && dir.join("adapters/lyracore").is_dir()
}

/// Parse `.wire-harness-rev`. Comments and blank lines are skipped; the first real line is the pin.
pub fn parse_pin(text: &str) -> Result<Pin> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .ok_or_else(|| {
            Error::ProjectLayout(
                "the wire-harness pin file has no pin line (want: `<tag> <40-hex sha>`)"
                    .to_string(),
            )
        })?;

    let mut fields = line.split_whitespace();
    let (Some(tag), Some(sha)) = (fields.next(), fields.next()) else {
        return Err(Error::ProjectLayout(format!(
            "the wire-harness pin must read `<tag> <40-hex sha>`, got '{line}'"
        )));
    };
    // The pinned ref must be a RELEASE, not a moving one. Refusing `main`/`HEAD` mechanically is
    // what makes "no unpinned default-branch checkout" a property rather than a review habit.
    if !(tag.starts_with('v') && tag[1..].starts_with(|c: char| c.is_ascii_digit())) {
        return Err(Error::ProjectLayout(format!(
            "the pinned wire-harness ref must be a version tag (got '{tag}'); a branch is not a pin"
        )));
    }
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::ProjectLayout(format!(
            "the pinned wire-harness sha must be 40 lowercase hex characters (got '{sha}')"
        )));
    }
    Ok(Pin {
        tag: tag.to_string(),
        sha: sha.to_string(),
    })
}

pub fn override_from_env() -> Option<PathBuf> {
    std::env::var_os(OVERRIDE_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn remote_from_env() -> String {
    std::env::var(REMOTE_VAR)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE.to_string())
}

/// Resolve the harness this checkout is pinned to, cloning it on first use.
pub fn resolve(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    override_dir: Option<&Path>,
    remote: &str,
) -> Result<Harness> {
    if let Some(given) = override_dir {
        let dir = std::fs::canonicalize(given).map_err(|_| {
            Error::ProjectLayout(format!("{OVERRIDE_VAR}={} does not exist", given.display()))
        })?;
        if !is_harness(&dir) {
            return Err(Error::ProjectLayout(format!(
                "{OVERRIDE_VAR}={} is not a wire-harness checkout (no Cargo.toml + src/ + \
                 adapters/lyracore/)",
                dir.display()
            )));
        }
        eprintln!(
            "[harness] OVERRIDE: using the local checkout {}",
            dir.display()
        );
        eprintln!(
            "[harness] OVERRIDE: this is NOT the pin — results are not reproducible from this \
             checkout alone"
        );
        return Ok(Harness { dir, pin: None });
    }

    let pin_file = project.wire_harness_pin();
    let text = std::fs::read_to_string(&pin_file)
        .map_err(|_| Error::ProjectLayout(format!("missing {}", pin_file.display())))?;
    let pin = parse_pin(&text)?;
    let dir = project.harness_cache().join(&pin.sha);

    if !dir.join(".git").is_dir() {
        eprintln!(
            "[harness] fetching wire-harness {} (first run for this pin)",
            pin.tag
        );
        let _ = std::fs::remove_dir_all(&dir);
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        runner
            .run_and_wait(&clone_command(remote, &pin, &dir))
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&dir);
                Error::Process(format!(
                "could not clone {remote} at {} — the wire-harness repository is private, so this \
                 needs your git credentials ({e})",
                pin.tag
            ))
            })?;
    }

    let head = runner
        .run_and_wait(&head_command(&dir))
        .map_err(|_| {
            Error::Process(format!(
                "{} is not a git checkout — delete it and re-run",
                dir.display()
            ))
        })?
        .trim()
        .to_string();
    if head != pin.sha {
        return Err(Error::Process(format!(
            "{} resolved to {head} but the pin says {}.\n  A release tag was re-pointed, or the \
             cache is corrupt. Delete {} to re-fetch; if the tag really moved, that is a \
             supply-chain event, not a pin bump.",
            pin.tag,
            pin.sha,
            dir.display()
        )));
    }
    if !is_harness(&dir) {
        return Err(Error::Process(format!(
            "{} does not look like a wire-harness checkout — delete it and re-run",
            dir.display()
        )));
    }
    Ok(Harness {
        dir,
        pin: Some(pin),
    })
}

/// `--branch <tag>` checks out exactly that tag; `--depth 1` keeps it a download, not a history.
/// A tag checkout IS detached by design, so the ten-line lecture about it is suppressed.
fn clone_command(remote: &str, pin: &Pin, dir: &Path) -> CommandSpec {
    CommandSpec::new("git")
        .arg("-c")
        .arg("advice.detachedHead=false")
        .arg("clone")
        .arg("--quiet")
        .arg("--depth")
        .arg("1")
        .arg("--branch")
        .arg(&pin.tag)
        .arg(remote)
        .arg(dir.to_string_lossy().to_string())
        // The repository is private: let git use the CLI's own credential handling, exactly like
        // the `./lyracore` shim does for this crate.
        .env("GIT_TERMINAL_PROMPT", "0")
}

fn head_command(dir: &Path) -> CommandSpec {
    CommandSpec::new("git")
        .arg("-C")
        .arg(dir.to_string_lossy().to_string())
        .arg("rev-parse")
        .arg("HEAD")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    const SHA: &str = "30e18083c8df705a484f157bd16a3f12b1aeb5ba";

    fn project(tmp: &TempDir, pin: &str) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(tmp.path().join(ProjectLayout::WIRE_HARNESS_PIN), pin).unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    /// Materialize something that passes `is_harness`, with a `.git` so no clone is attempted.
    fn cached_checkout(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("adapters/lyracore")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(dir.join(ProjectLayout::HARNESS_SMOKE_SEAM), "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn a_pin_is_a_release_tag_and_a_full_sha() {
        let pin = parse_pin(&format!("# a comment\n\nv0.1.0-alpha.2 {SHA}\n")).unwrap();
        assert_eq!(pin.tag, "v0.1.0-alpha.2");
        assert_eq!(pin.sha, SHA);
    }

    #[test]
    fn a_branch_is_not_a_pin() {
        // The whole point of #246: there must be no way to say "main" here.
        for line in [
            format!("main {SHA}"),
            format!("HEAD {SHA}"),
            format!("release {SHA}"),
        ] {
            let error = parse_pin(&line).unwrap_err();
            assert!(
                error.to_string().contains("a branch is not a pin"),
                "{line} must be refused: {error}"
            );
        }
    }

    #[test]
    fn a_short_or_uppercase_sha_is_refused() {
        for sha in [
            "30e18083",
            "30E18083C8DF705A484F157BD16A3F12B1AEB5BA",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(parse_pin(&format!("v1.0.0 {sha}")).is_err(), "{sha}");
        }
        assert!(
            parse_pin("v1.0.0").is_err(),
            "a tag with no sha is not a pin"
        );
        assert!(parse_pin("# only comments\n").is_err());
    }

    #[test]
    fn a_cached_checkout_at_the_pinned_sha_is_used_without_a_clone() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        cached_checkout(&project.harness_cache().join(SHA));
        let stack = FakeStack::new().with_stdout("rev-parse HEAD", &format!("{SHA}\n"));

        let harness = resolve(&project, &stack.runner(), None, DEFAULT_REMOTE).unwrap();
        assert_eq!(harness.dir, project.harness_cache().join(SHA));
        assert_eq!(harness.pin.unwrap().sha, SHA);
        assert!(
            !stack.rendered().iter().any(|r| r.contains("clone")),
            "a cached pin must not be re-cloned: {:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_repointed_tag_is_refused_as_a_supply_chain_event() {
        // The reason the sha is recorded at all: a tag is a mutable ref.
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        cached_checkout(&project.harness_cache().join(SHA));
        let other = "1111111111111111111111111111111111111111";
        let stack = FakeStack::new().with_stdout("rev-parse HEAD", other);

        let error = resolve(&project, &stack.runner(), None, DEFAULT_REMOTE).unwrap_err();
        assert!(error.to_string().contains("supply-chain"), "{error}");
        assert!(error.to_string().contains(other), "{error}");
    }

    #[test]
    fn a_missing_cache_clones_exactly_the_pinned_tag() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        // Nothing cached: the clone is attempted, and the fake does not materialize a checkout, so
        // resolution then fails at the verification step. What is asserted here is the COMMAND.
        let stack = FakeStack::new();
        let _ = resolve(&project, &stack.runner(), None, "ssh://example/harness.git");

        let clone = stack
            .rendered()
            .into_iter()
            .find(|r| r.contains("clone"))
            .expect("a clone must have been attempted");
        assert!(clone.contains("--depth 1"), "{clone}");
        assert!(clone.contains("--branch v0.1.0-alpha.2"), "{clone}");
        assert!(
            clone.contains(SHA),
            "the cache directory is the sha: {clone}"
        );
        assert!(
            !clone.contains("main") && !clone.contains("HEAD"),
            "a branch must never be cloned: {clone}"
        );
    }

    #[test]
    fn a_local_override_is_used_verbatim_and_is_not_a_pin() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        let local = tmp.path().join("local-harness");
        cached_checkout(&local);
        let stack = FakeStack::new();

        let harness = resolve(&project, &stack.runner(), Some(&local), DEFAULT_REMOTE).unwrap();
        assert_eq!(harness.dir, std::fs::canonicalize(&local).unwrap());
        assert_eq!(harness.pin, None, "an override is not a pin");
        assert!(stack.calls().is_empty(), "nothing is cloned or verified");
    }

    #[test]
    fn an_override_that_is_not_a_harness_is_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        let not_a_harness = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&not_a_harness).unwrap();
        let stack = FakeStack::new();

        let error = resolve(
            &project,
            &stack.runner(),
            Some(&not_a_harness),
            DEFAULT_REMOTE,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("not a wire-harness checkout"),
            "{error}"
        );

        let missing = tmp.path().join("nope");
        let error = resolve(&project, &stack.runner(), Some(&missing), DEFAULT_REMOTE).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn a_missing_pin_file_names_the_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = FakeStack::new();
        let error = resolve(&project, &stack.runner(), None, DEFAULT_REMOTE).unwrap_err();
        assert!(
            error.to_string().contains(ProjectLayout::WIRE_HARNESS_PIN),
            "{error}"
        );
    }

    #[test]
    fn the_cache_lives_inside_the_ignored_state_directory() {
        // A harness checkout must never be able to show up in the server repo's `git status`.
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp, &format!("v0.1.0-alpha.2 {SHA}\n"));
        assert_eq!(
            project.harness_cache(),
            tmp.path().join(".lyracore/wire-harness")
        );
    }
}
