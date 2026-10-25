use std::process::Command;

#[test]
fn focused_help_needs_no_checkout_and_performs_no_work() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["packages", "--help"],
        vec!["import", "--help"],
        vec!["dev", "up", "--help"],
        vec!["dev", "watch", "--help"],
        vec!["help", "packages", "apply"],
        vec!["update", "--help"],
        vec!["bindings", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lyracore"))
            .args(&args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE:"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_lyracore-ops"))
        .args(["service", "reconcile", "--help"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = Command::new(env!("CARGO_BIN_EXE_lyracore"))
        .args(["service", "reconcile"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("lyracore-ops service reconcile"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
