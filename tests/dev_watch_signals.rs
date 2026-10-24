#![cfg(unix)]

use lyracore_cli::proc::{CommandSpec, ProcessRunner, RealProcessRunner};
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const FIXTURE: &str = "LYRACORE_WATCH_SIGNAL_FIXTURE";

#[test]
fn watcher_signal_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE) else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let pid = RealProcessRunner
        .spawn_logged(
            &CommandSpec::new("sleep").arg("60").own_process_group(),
            &directory.join("host.log"),
        )
        .unwrap();
    fs::write(directory.join("host.pid"), pid.to_string()).unwrap();
    RealProcessRunner
        .run_streaming(
            &CommandSpec::new("sh")
                .arg("-c")
                .arg("printf '%s' \"$$\" > \"$1\"; exec sleep 60")
                .arg("watch-build")
                .arg(directory.join("build.pid").to_string_lossy()),
        )
        .unwrap();
}

struct Processes {
    watcher: Child,
    host: Option<u32>,
}

impl Drop for Processes {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.watcher.id())])
            .stderr(Stdio::null())
            .status();
        let _ = self.watcher.kill();
        let _ = self.watcher.wait();
        if let Some(host) = self.host {
            let _ = RealProcessRunner.terminate(host);
        }
    }
}

#[test]
fn terminal_interrupt_stops_watcher_and_foreground_build_but_leaves_background_host() {
    let directory = tempfile::tempdir().unwrap();
    let watcher = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "watcher_signal_fixture", "--nocapture"])
        .env(FIXTURE, directory.path())
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut processes = Processes {
        watcher,
        host: None,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid_path = directory.path().join("host.pid");
    let build_path = directory.path().join("build.pid");
    while (!pid_path.exists() || !build_path.exists()) && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    let host: u32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    processes.host = Some(host);
    assert!(build_path.is_file());
    let watcher_group = format!("-{}", processes.watcher.id());
    assert!(Command::new("kill")
        .args(["-INT", "--", &watcher_group])
        .status()
        .unwrap()
        .success());
    let status = processes.watcher.wait().unwrap();
    assert_eq!(status.signal(), Some(2));
    assert!(Command::new("kill")
        .args(["-0", &host.to_string()])
        .status()
        .unwrap()
        .success());
    let members = Command::new("ps")
        .args(["-eo", "pgid=,stat="])
        .output()
        .unwrap();
    let watcher_group = processes.watcher.id().to_string();
    assert!(!String::from_utf8_lossy(&members.stdout)
        .lines()
        .any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some(watcher_group.as_str())
                && fields.next().is_some_and(|state| !state.starts_with('Z'))
        }));
}
