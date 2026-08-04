//! Process identity and liveness, portably.
//!
//! A bare PID is not an identity: PIDs are reused, and stopping a recycled one kills a stranger's
//! process. We pair the PID with the kernel's start time plus the command name, both read through
//! POSIX `ps` — no `/proc` (absent on macOS), no `grep -P`, no GNU-only flags.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::process::Command;
use std::time::Duration;

pub trait ProcessInspector {
    /// A stable identity for a live PID, or `None` if no such process exists.
    fn identity(&self, pid: u32) -> Option<String>;

    /// Is something accepting connections on this loopback port?
    fn port_serving(&self, port: u16) -> bool;
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

    fn port_serving(&self, port: u16) -> bool {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        TcpStream::connect_timeout(&addr.into(), Duration::from_millis(250)).is_ok()
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
}
