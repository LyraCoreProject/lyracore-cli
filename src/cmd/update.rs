//! `lyracore update` — pull the latest checkout in place, and tell you how to restart the stack.
//!
//! A user clone's plain `git pull` is broken by design today: `scripts/mirror-publish.sh` squashes
//! every publish into a new single-root commit and force-pushes, so the local and remote histories
//! are unrelated. `git reset --hard origin/main` is the operation that works across BOTH that
//! shape and the fast-forward one a future append-forward mirror will have, which is why this
//! reaches for it rather than a merge — and why it refuses outright over anything a hard reset
//! would discard.
//!
//! This does NOT restart the dev stack itself — it only prints the command to. Running `dev
//! down`/`dev up` unasked would spin up the whole fixture on a checkout that has no stack running
//! at all, and shelling out to a `lyracore` on PATH assumes the launcher shim exists, which a
//! contributor driving this binary directly may not have.
//!
//! `--install-service` is the one exception, and it is opt-in for that reason: on a production
//! host it also reconciles the tracked `spacetimedb-standalone` systemd unit (see
//! [`crate::cmd::systemd`]). Plain `update` remains a contributor's git-pull replacement and
//! touches no system service.

use crate::cmd::systemd;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// The lyracore-cli release pin, at the checkout root — the same file `Phase B` of the sharded
/// fixture's mirror work bumps on every landed release. Read before and after the reset purely to
/// report whether it moved; installing a new pin is the `lyracore` shim's job, not this command's.
const LYRACORE_CLI_PIN: &str = ".lyracore-cli-rev";

/// `install_service` adds the systemd reconciliation described in the module docs. Everything
/// before it — the dirty-tree refusal above all — is unchanged by the flag: local work still
/// blocks the reset AND the service change.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    install_service: bool,
) -> Result<()> {
    // Fail fast, before the network and before anything on disk moves: every step of the
    // reconciliation writes to /etc or drives systemd, and a half-done privileged plan is worse
    // than one that never started.
    if install_service {
        require_root(runner)?;
    }

    println!("· fetching origin...");
    runner.run_and_wait(&git(project).arg("fetch").arg("origin"))?;

    let porcelain = runner.run_and_wait(&git(project).arg("status").arg("--porcelain"))?;
    refuse_if_dirty(&porcelain)?;

    let head = runner.run_and_wait(&git(project).arg("rev-parse").arg("HEAD"))?;
    let upstream = runner.run_and_wait(&git(project).arg("rev-parse").arg("origin/main"))?;
    let head = head.trim().to_string();
    let upstream = upstream.trim().to_string();
    if head == upstream {
        println!("already up to date.");
        // Deployment drift is independent of git drift: a host can sit exactly on origin/main
        // with the wrong unit installed, or none at all. So this reconciles anyway.
        if install_service {
            return systemd::reconcile_standalone(project, runner);
        }
        return Ok(());
    }

    let old_pin = read_pin(project);

    println!("· updating to origin/main...");
    runner.run_and_wait(&git(project).arg("reset").arg("--hard").arg("origin/main"))?;

    let new_pin = read_pin(project);
    println!("updated {} → {}", short(&head), short(&upstream));
    if old_pin != new_pin {
        println!(
            "  the lyracore-cli pin changed — the next `lyracore` invocation installs the new \
             release automatically."
        );
    }

    println!();
    if install_service {
        return systemd::reconcile_standalone(project, runner);
    }
    println!("this does not restart anything for you. To rebuild the gateway and republish");
    println!("every fixture database, run:");
    println!("  ./lyracore dev down && ./lyracore dev up");
    Ok(())
}

/// `--install-service` writes to `/etc/systemd/system` and drives `systemctl`. Asking for the
/// privilege up front, rather than per command, keeps the plan one atomic decision: no mid-run
/// password prompt, and no half-installed unit because the fifth command was the first to be
/// denied.
fn require_root(runner: &dyn ProcessRunner) -> Result<()> {
    let euid = runner.run_and_wait(&CommandSpec::new("id").arg("-u"))?;
    match euid.trim().parse::<u32>() {
        Ok(0) => Ok(()),
        Ok(other) => Err(Error::Process(format!(
            "`update --install-service` installs a systemd unit and restarts the standalone node, \
             which needs root — this process runs as uid {other}. Re-run it as \
             `sudo ./lyracore update --install-service`."
        ))),
        Err(_) => Err(Error::Process(format!(
            "could not read the effective user id (`id -u` said {euid:?}), so \
             `--install-service` cannot confirm it may write to {}. Re-run as root.",
            "/etc/systemd/system"
        ))),
    }
}

/// A commit sha, shortened for the "updated X → Y" line — the conventional 7 characters, or
/// whatever `git rev-parse` gave us if it was somehow shorter than that.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn git(project: &ProjectLayout) -> CommandSpec {
    CommandSpec::new("git").cwd(project.root.clone())
}

/// Any `git status --porcelain` line that does not start with `??` describes something a `reset
/// --hard` would discard: a modification, a staged change, a rename, a deletion. Untracked files
/// are `??` and survive a reset untouched, so they alone must never block an update.
fn refuse_if_dirty(porcelain: &str) -> Result<()> {
    let dirty: Vec<&str> = porcelain
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("??"))
        .collect();
    if dirty.is_empty() {
        return Ok(());
    }
    Err(Error::Process(format!(
        "commit or stash first — update uses `git reset --hard`, which would discard:\n{}",
        dirty
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

fn read_pin(project: &ProjectLayout) -> Option<String> {
    std::fs::read_to_string(project.root.join(LYRACORE_CLI_PIN))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeStack};
    use tempfile::TempDir;

    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn same_sha_stack() -> FakeStack {
        FakeStack::new()
            .with_stdout(
                "rev-parse HEAD",
                "aaaa1111111111111111111111111111111111111\n",
            )
            .with_stdout(
                "rev-parse origin/main",
                "aaaa1111111111111111111111111111111111111\n",
            )
    }

    fn ahead_stack() -> FakeStack {
        FakeStack::new()
            .with_stdout(
                "rev-parse HEAD",
                "aaaa1111111111111111111111111111111111111\n",
            )
            .with_stdout(
                "rev-parse origin/main",
                "bbbb2222222222222222222222222222222222222\n",
            )
    }

    // ---- the dirty-tree refusal ----

    #[test]
    fn a_dirty_tracked_file_is_refused_before_any_reset() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = FakeStack::new().with_stdout("status --porcelain", " M module/src/foo.rs\n");

        let error = run(&project, &stack.runner(), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("module/src/foo.rs"), "{error}");
        assert!(error.contains("commit or stash"), "{error}");
        assert_eq!(
            stack.rendered(),
            vec![
                "git fetch origin".to_string(),
                "git status --porcelain".to_string()
            ],
            "no reset may run against a dirty tree"
        );
    }

    #[test]
    fn untracked_files_alone_do_not_block_an_update() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = ahead_stack().with_stdout("status --porcelain", "?? scratch/notes.txt\n");

        run(&project, &stack.runner(), false).unwrap();
        assert!(
            stack
                .rendered()
                .iter()
                .any(|r| r == "git reset --hard origin/main"),
            "{:?}",
            stack.rendered()
        );
    }

    #[test]
    fn refuse_if_dirty_treats_only_non_double_question_mark_lines_as_dirty() {
        assert!(refuse_if_dirty("?? new.txt\n?? another.txt\n").is_ok());
        assert!(refuse_if_dirty("").is_ok());
        assert!(refuse_if_dirty(" M changed.rs\n").is_err());
        assert!(refuse_if_dirty("A  staged.rs\n").is_err());
        assert!(refuse_if_dirty(" D removed.rs\n").is_err());
        assert!(refuse_if_dirty("?? untracked.rs\n M changed.rs\n").is_err());
    }

    // ---- the already-current short-circuit ----

    #[test]
    fn head_equal_to_upstream_short_circuits_before_any_reset() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = same_sha_stack();

        run(&project, &stack.runner(), false).unwrap();

        assert_eq!(
            stack.rendered(),
            vec![
                "git fetch origin".to_string(),
                "git status --porcelain".to_string(),
                "git rev-parse HEAD".to_string(),
                "git rev-parse origin/main".to_string(),
            ]
        );
    }

    // ---- the happy path ----

    #[test]
    fn the_happy_path_runs_the_five_git_steps_and_never_executes_the_restart() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = ahead_stack();

        run(&project, &stack.runner(), false).unwrap();

        // `update` only PRINTS the follow-up (`./lyracore dev down && ./lyracore dev up`) — it
        // must never run it. Auto-running would spin up the whole fixture on a checkout with no
        // stack at all, and depends on a `lyracore` launcher being on PATH that a contributor
        // driving this binary directly may not have.
        assert_eq!(
            stack.rendered(),
            vec![
                "git fetch origin".to_string(),
                "git status --porcelain".to_string(),
                "git rev-parse HEAD".to_string(),
                "git rev-parse origin/main".to_string(),
                "git reset --hard origin/main".to_string(),
            ]
        );
        assert!(
            stack.calls().iter().all(|c| matches!(c, Call::Wait(_))),
            "no streamed or spawned call may appear: {:?}",
            stack.calls()
        );
    }

    #[test]
    fn every_git_call_runs_from_the_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = ahead_stack();

        run(&project, &stack.runner(), false).unwrap();

        for call in stack.calls() {
            let Call::Wait(spec) = call else { continue };
            if spec.program() == "git" {
                assert_eq!(
                    spec.cwd_value(),
                    Some(project.root.as_path()),
                    "{}",
                    spec.render()
                );
            }
        }
    }

    // ---- the pin readout ----

    #[test]
    fn the_pin_is_read_trimmed_or_reads_as_absent() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        assert_eq!(read_pin(&project), None);
        std::fs::write(project.root.join(LYRACORE_CLI_PIN), "abc123\n").unwrap();
        assert_eq!(read_pin(&project).as_deref(), Some("abc123"));
    }

    // ---- the "updated X → Y" line's shas ----

    #[test]
    fn short_truncates_a_full_sha_to_seven_characters() {
        assert_eq!(
            short("aaaa1111111111111111111111111111111111111"),
            "aaaa111"
        );
    }

    #[test]
    fn short_leaves_anything_already_seven_characters_or_shorter_alone() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
        assert_eq!(short("1234567"), "1234567");
    }

    // ---- `--install-service` ----

    fn as_root(stack: FakeStack) -> FakeStack {
        systemd::testing::reconcilable_host(stack.with_stdout("id -u", "0\n"))
    }

    fn service_project(tmp: &TempDir) -> ProjectLayout {
        let project = project(tmp);
        systemd::testing::write_unit(&project.root);
        project
    }

    #[test]
    fn plain_update_never_touches_a_system_service() {
        let tmp = TempDir::new().unwrap();
        let project = service_project(&tmp);
        let stack = ahead_stack();

        run(&project, &stack.runner(), false).unwrap();

        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.starts_with("systemctl") || r.starts_with("install ")),
            "{:?}",
            stack.rendered()
        );
    }

    #[test]
    fn a_non_root_invocation_is_refused_before_anything_else_runs() {
        let tmp = TempDir::new().unwrap();
        let project = service_project(&tmp);
        let stack = as_root(ahead_stack()).with_stdout("id -u", "1000\n");

        let error = run(&project, &stack.runner(), true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("sudo ./lyracore update --install-service"),
            "{error}"
        );
        assert_eq!(
            stack.rendered(),
            vec!["id -u".to_string()],
            "not even the fetch may run without the privilege the plan needs"
        );
    }

    #[test]
    fn a_dirty_tree_blocks_the_service_change_too() {
        let tmp = TempDir::new().unwrap();
        let project = service_project(&tmp);
        let stack =
            as_root(ahead_stack()).with_stdout("status --porcelain", " M module/src/foo.rs\n");

        let error = run(&project, &stack.runner(), true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("commit or stash"), "{error}");
        assert_eq!(
            stack.rendered(),
            vec![
                "id -u".to_string(),
                "git fetch origin".to_string(),
                "git status --porcelain".to_string(),
            ],
            "a dirty tree stops the reset AND the systemd mutation"
        );
    }

    #[test]
    fn an_updated_checkout_reconciles_the_supervisor() {
        let tmp = TempDir::new().unwrap();
        let project = service_project(&tmp);
        let stack = as_root(ahead_stack());

        run(&project, &stack.runner(), true).unwrap();

        let rendered = stack.rendered();
        let reset = rendered
            .iter()
            .position(|r| r == "git reset --hard origin/main")
            .expect("the checkout still updates");
        let restart = rendered
            .iter()
            .position(|r| r == "systemctl --no-pager restart spacetimedb-standalone.service")
            .expect("the supervisor is restarted");
        assert!(reset < restart, "{rendered:?}");
        assert!(
            rendered
                .iter()
                .any(|r| r.contains("--property=ActiveState")),
            "{rendered:?}"
        );
    }

    #[test]
    fn an_already_current_checkout_still_reconciles_the_supervisor() {
        let tmp = TempDir::new().unwrap();
        let project = service_project(&tmp);
        let stack = as_root(same_sha_stack());

        run(&project, &stack.runner(), true).unwrap();

        let rendered = stack.rendered();
        assert!(
            !rendered.iter().any(|r| r.contains("reset --hard")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|r| r == "systemctl --no-pager restart spacetimedb-standalone.service"),
            "deployment drift is repaired without a new commit: {rendered:?}"
        );
    }
}
