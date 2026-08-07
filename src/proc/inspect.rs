//! Process identity and liveness, portably.
//!
//! A bare PID is not an identity: PIDs are reused, and stopping a recycled one kills a stranger's
//! process. We pair the PID with the kernel's start time plus the command name, both read through
//! POSIX `ps` — no `/proc` (absent on macOS), no `grep -P`, no GNU-only flags.

use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

/// The portion of an `identity()` string that survives an `exec` in the same process.
///
/// `identity()` joins `lstart` and `comm` into one whitespace-separated string, with `comm` last
/// (it is a single token — executable names never contain spaces). SpacetimeDB's version-manager
/// shim keeps its PID and start time across the `exec` into the versioned binary, but that `exec`
/// **replaces** `comm` (`spacetime` -> kernel-truncated `spacetimedb-sta`). A caller pinned to the
/// startup window — before the process has settled into whatever it is going to run as — must
/// compare only this prefix, or an ordinary `exec` reads as the process having died (#431).
pub fn start_signature(identity: &str) -> &str {
    identity
        .rsplit_once(' ')
        .map_or(identity, |(start, _comm)| start)
}

pub trait ProcessInspector {
    /// A stable identity for a live PID, or `None` if no such process exists.
    fn identity(&self, pid: u32) -> Option<String>;

    /// Is something accepting connections on `host:port`? The host matters since `--lan`: a
    /// gateway bound to a LAN address does not answer on loopback.
    fn serving(&self, host: &str, port: u16) -> bool;
}

pub struct RealProcessInspector;

impl ProcessInspector for RealProcessInspector {
    fn identity(&self, pid: u32) -> Option<String> {
        // `lstart` is the absolute start time; `comm` the executable name. Both are present on
        // Linux and macOS. An exited PID makes `ps` print nothing and exit nonzero.
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart=,comm="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let identity = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        (!identity.is_empty()).then_some(identity)
    }

    fn serving(&self, host: &str, port: u16) -> bool {
        // `to_socket_addrs` rather than a parsed literal: every host this CLI probes is one it
        // chose itself (loopback, or the `--lan` address it validated), so this is a formatting
        // convenience, not a name-resolution feature.
        let Ok(mut addrs) = (host, port).to_socket_addrs() else {
            return false;
        };
        addrs.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_pid_has_an_identity_and_a_bogus_one_does_not() {
        let inspector = RealProcessInspector;
        let ours = std::process::id();
        assert!(
            inspector.identity(ours).is_some(),
            "the running test process must be visible to ps"
        );
        // PID 0 is never a real user process to `ps -p`.
        assert_eq!(inspector.identity(0), None);
    }

    #[test]
    fn identity_is_stable_across_reads() {
        let inspector = RealProcessInspector;
        let ours = std::process::id();
        assert_eq!(inspector.identity(ours), inspector.identity(ours));
    }

    #[test]
    fn start_signature_ignores_only_the_trailing_comm_token() {
        // The exact shape #431 is about: same start time, `comm` replaced by an `exec`.
        assert_eq!(
            start_signature("Thu Aug  7 10:23:45 2026 spacetime"),
            start_signature("Thu Aug  7 10:23:45 2026 spacetimedb-sta")
        );
        // A different start time is a different process (dead PID reused by someone else), even
        // with the same `comm` — this must NOT collapse to equal.
        assert_ne!(
            start_signature("Thu Aug  7 10:23:45 2026 spacetime"),
            start_signature("Fri Aug  8 09:00:00 2026 spacetime")
        );
    }

    #[test]
    fn a_listener_is_seen_on_the_host_it_bound_and_not_on_another() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let inspector = RealProcessInspector;
        assert!(inspector.serving("127.0.0.1", port));
        // Nothing is listening on this address here, and an unroutable/unassigned host must read
        // as "not serving" rather than hanging or panicking.
        assert!(!inspector.serving("192.0.2.1", port));
        assert!(!inspector.serving("not a host", port));
    }
}
