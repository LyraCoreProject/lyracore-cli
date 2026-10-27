//! Exercise update against real, local-only repositories, including work a reset could lose.
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=CLI test",
            "-c",
            "user.email=cli@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Checkout {
    tmp: TempDir,
}

impl Checkout {
    fn new() -> Self {
        let this = Self {
            tmp: TempDir::new().unwrap(),
        };
        let root = this.tmp.path();
        git(root, &["init", "-q", "-b", "main", "origin"]);
        let origin = root.join("origin");
        std::fs::write(origin.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(origin.join(".gitignore"), "ignored.txt\n").unwrap();
        git(&origin, &["add", "."]);
        git(
            &origin,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "initial"],
        );
        git(root, &["clone", "-q", "origin", "work"]);
        this
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.tmp.path().join(name)
    }

    fn commit(&self, repo: &str, file: &str, content: &str) -> String {
        let root = self.path(repo);
        std::fs::write(root.join(file), content).unwrap();
        git(&root, &["add", "-f", file]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-qm", file],
        );
        git(&root, &["rev-parse", "HEAD"])
    }

    fn update(&self, flags: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lyracore"))
            .arg("update")
            .args(flags)
            .current_dir(self.path("work"))
            .output()
            .unwrap()
    }
}

#[test]
fn dirty_tracked_work_blocks_both_update_modes() {
    for flags in [vec![], vec!["--reset-mirror"]] {
        let checkout = Checkout::new();
        let head = git(&checkout.path("work"), &["rev-parse", "HEAD"]);
        checkout.commit("origin", "remote.txt", "remote");
        std::fs::write(
            checkout.path("work/Cargo.toml"),
            "[workspace]\n# local edit\n",
        )
        .unwrap();
        assert!(!checkout.update(&flags).status.success());
        assert_eq!(git(&checkout.path("work"), &["rev-parse", "HEAD"]), head);
        assert!(std::fs::read_to_string(checkout.path("work/Cargo.toml"))
            .unwrap()
            .contains("local edit"));
    }
}

#[test]
fn a_failed_recovery_ref_prevents_the_mirror_reset() {
    let checkout = Checkout::new();
    let head = checkout.commit("work", "local.txt", "keep me");
    // A branch at this prefix prevents creating a recovery branch beneath it.
    git(&checkout.path("work"), &["branch", "lyracore-backup"]);
    assert!(!checkout.update(&["--reset-mirror"]).status.success());
    assert_eq!(git(&checkout.path("work"), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(checkout.path("work/local.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn mirror_reset_never_moves_an_existing_recovery_branch() {
    for active_backup in [false, true] {
        let checkout = Checkout::new();
        let work = checkout.path("work");
        let head = checkout.commit("work", "local.txt", "recover me");
        let backup = format!("lyracore-backup/{head}");
        git(&work, &["checkout", "-b", &backup]);
        let saved = if active_backup {
            head.clone()
        } else {
            let advanced = checkout.commit("work", "recovered.txt", "more work");
            git(&work, &["checkout", "main"]);
            advanced
        };
        let output = checkout.update(&["--reset-mirror"]);
        assert!(!output.status.success(), "must refuse an existing backup");
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&work, &["rev-parse", &backup]), saved);
    }
}

#[test]
fn update_keeps_clean_local_commits() {
    let checkout = Checkout::new();
    let head = checkout.commit("work", "local.txt", "keep me");
    let output = checkout.update(&[]);
    assert!(!output.status.success(), "update must refuse local commits");
    assert_eq!(git(&checkout.path("work"), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(checkout.path("work/local.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn update_fast_forwards_without_touching_untracked_work() {
    let checkout = Checkout::new();
    let upstream = checkout.commit("origin", "remote.txt", "upstream");
    std::fs::write(checkout.path("work/notes.txt"), "notes").unwrap();
    let output = checkout.update(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(&checkout.path("work"), &["rev-parse", "HEAD"]),
        upstream
    );
    assert_eq!(
        std::fs::read_to_string(checkout.path("work/notes.txt")).unwrap(),
        "notes"
    );
}

#[test]
fn explicit_mirror_reset_keeps_a_recovery_branch() {
    let checkout = Checkout::new();
    let old = checkout.commit("work", "local.txt", "recover me");
    let origin = checkout.path("origin");
    git(&origin, &["checkout", "--orphan", "replacement"]);
    git(
        &origin,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "new mirror root",
        ],
    );
    git(&origin, &["branch", "-M", "main"]);
    let upstream = git(&origin, &["rev-parse", "HEAD"]);
    assert!(!checkout.update(&[]).status.success());
    let output = checkout.update(&["--reset-mirror"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let work = checkout.path("work");
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), upstream);
    assert_eq!(
        git(
            &work,
            &["rev-parse", &format!("refs/heads/lyracore-backup/{old}")]
        ),
        old
    );
    assert_eq!(
        git(
            &work,
            &["show", &format!("lyracore-backup/{old}:local.txt")]
        ),
        "recover me"
    );
}

#[test]
fn update_refuses_untracked_and_ignored_files_in_the_way() {
    for file in ["notes.txt", "ignored.txt"] {
        for flags in [vec![], vec!["--reset-mirror"]] {
            let checkout = Checkout::new();
            let work = checkout.path("work");
            let head = git(&work, &["rev-parse", "HEAD"]);
            checkout.commit("origin", file, "remote version");
            std::fs::write(work.join(file), "local version").unwrap();
            let output = checkout.update(&flags);
            assert!(
                !output.status.success(),
                "must preserve {file} with {flags:?}"
            );
            assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                std::fs::read_to_string(work.join(file)).unwrap(),
                "local version"
            );
        }
    }
}

#[test]
fn nested_workspace_updates_protect_files_outside_the_workspace() {
    for flags in [vec![], vec!["--reset-mirror"]] {
        let checkout = Checkout::new();
        let work = checkout.path("work");
        std::fs::create_dir(checkout.path("origin/core")).unwrap();
        let head = checkout.commit("origin", "core/Cargo.toml", "[workspace]\n");
        git(&work, &["fetch", "origin"]);
        git(&work, &["reset", "--hard", &head]);
        std::fs::write(work.join("ignored.txt"), "local work").unwrap();
        checkout.commit("origin", "ignored.txt", "incoming file");
        let output = Command::new(env!("CARGO_BIN_EXE_lyracore"))
            .arg("update")
            .args(flags)
            .current_dir(work.join("core"))
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "must protect the entire repository"
        );
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(work.join("ignored.txt")).unwrap(),
            "local work"
        );
    }
}
