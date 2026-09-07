//! `lyracore service reconcile` — make this host's standalone supervisor match the tracked unit.
//!
//! The server repository's `docs/danger-zones.md` §3 installs
//! `deploy/systemd/spacetimedb-standalone.service` by hand: `install`, `systemctl daemon-reload`,
//! `systemctl enable`, restart, then read the effective properties back. A host reconciled by hand
//! matches whatever its last operator typed; this encodes the same ordered steps so it matches the
//! artifact in the checkout instead.
//!
//! Deployment reconciliation is ONE job, so this verb owns the git steps too: it moves the
//! checkout to `origin/main` (the same work `update` does, shared with it) and then reconciles the
//! host against what that checkout now ships. It is a separate verb from `update` because it is a
//! root-only host mutator, and `update` is a contributor's git-pull replacement.
//!
//! Two rules shape everything below. Every step goes through [`ProcessRunner`], so the whole plan
//! is an ordered, assertable list of commands rather than side effects on a machine. And the
//! service contract it verifies — start command, descriptor limit, stderr destination, data
//! directory, listen address — is READ OUT of the tracked unit, never duplicated here, so this CLI
//! cannot claim a host is reconciled against a contract the checkout no longer ships.
//!
//! It manages the supervisor only. The persistent database directory is checked for existence and
//! otherwise never touched: no create, no move, no delete.

use crate::cmd::update;
use crate::proc::{CommandSpec, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// Where a system unit lives on the host. Its `/usr/lib` counterpart belongs to packages; a unit
/// an operator installs from a checkout belongs here, and this one overrides any packaged file of
/// the same name.
const SYSTEMD_UNIT_DIR: &str = "/etc/systemd/system";

/// Update the checkout, install the tracked unit, reload systemd, enable it, restart it, and
/// verify the result.
///
/// Refuses — before the HOST is touched — when the invocation is not root, when the checkout has
/// local work a reset would discard, when a host prerequisite is missing, or when another active
/// service already owns the node's data directory or listen address.
///
/// "Before the host is touched" and not "before anything": the checkout reset below runs first, on
/// purpose. The unit and the contract to check the host against are read out of the UPDATED
/// checkout, so checking the host before the reset would check it against a contract this command
/// is not about to install. A refusal after that point therefore leaves the checkout on
/// `origin/main` with the host untouched, which is the state an operator can simply re-run from.
pub fn reconcile(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    // Fail fast, before the network and before anything on disk moves: this plan resets the
    // checkout and then writes to /etc, and a half-done privileged plan is worse than one that
    // never started.
    require_root(runner)?;
    // Deployment drift is independent of git drift: a host can sit exactly on origin/main with the
    // wrong unit installed, or none at all. So the reconciliation below runs either way.
    update::pull(project, runner)?;
    reconcile_unit(project, runner)
}

/// `service reconcile` writes to `/etc/systemd/system` and drives `systemctl`. Asking for the
/// privilege up front, rather than per command, keeps the plan one atomic decision: no mid-run
/// password prompt, and no half-installed unit because the fifth command was the first to be
/// denied.
fn require_root(runner: &dyn ProcessRunner) -> Result<()> {
    let euid = runner.run_and_wait(&CommandSpec::new("id").arg("-u"))?;
    match euid.trim().parse::<u32>() {
        Ok(0) => Ok(()),
        Ok(other) => Err(Error::Process(format!(
            "`service reconcile` installs a systemd unit and restarts the standalone node, which \
             needs root — this process runs as uid {other}. Re-run it as `sudo ./lyracore service \
             reconcile`."
        ))),
        Err(_) => Err(Error::Process(format!(
            "could not read the effective user id (`id -u` said {euid:?}), so `service reconcile` \
             cannot confirm it may write to {SYSTEMD_UNIT_DIR}. Re-run as root."
        ))),
    }
}

fn reconcile_unit(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    let source = project.standalone_unit();
    let text = std::fs::read_to_string(&source).map_err(|e| {
        Error::PrerequisiteMissing(format!(
            "this checkout has no {} ({e}). `service reconcile` installs the unit tracked in the \
             checkout, so there is nothing to reconcile against.",
            ProjectLayout::STANDALONE_UNIT
        ))
    })?;
    let contract = UnitContract::parse(&text)?;
    let unit = source
        .file_name()
        .expect("STANDALONE_UNIT names a file")
        .to_string_lossy()
        .to_string();
    let target = format!("{SYSTEMD_UNIT_DIR}/{unit}");

    println!("· checking host prerequisites...");
    check_prerequisites(&contract, runner)?;

    println!("· looking for a service that already owns this node...");
    refuse_conflicting_service(&unit, &contract, runner)?;

    println!("· installing and enabling {unit}...");
    runner.run_and_wait(
        &CommandSpec::new("install")
            .arg("-o")
            .arg("root")
            .arg("-g")
            .arg("root")
            .arg("-m")
            .arg("0644")
            .arg(source.display().to_string())
            .arg(&target),
    )?;
    runner.run_and_wait(&systemctl().arg("daemon-reload"))?;
    runner.run_and_wait(&systemctl().arg("enable").arg(&unit))?;

    println!("· restarting {unit}...");
    runner
        .run_and_wait(&systemctl().arg("restart").arg(&unit))
        .map_err(|e| {
            Error::Process(format!(
                "{unit} did not restart: {e}\nThe tracked unit is installed at {target} and \
                 systemd has reloaded it, but no standalone node is running. Read why with \
                 `journalctl -u {unit} --no-pager -n 100`{}.",
                contract
                    .log_path()
                    .map(|log| format!(" and `tail -n 100 {log}`"))
                    .unwrap_or_default()
            ))
        })?;

    verify(&unit, &contract, runner)?;
    println!(
        "{unit} is active, with the start command, descriptor limit and stderr destination the \
         tracked unit declares."
    );
    Ok(())
}

fn systemctl() -> CommandSpec {
    CommandSpec::new("systemctl").arg("--no-pager")
}

fn busctl_exec_start(unit: &str) -> CommandSpec {
    CommandSpec::new("busctl")
        .arg("--json=short")
        .arg("get-property")
        .arg("org.freedesktop.systemd1")
        .arg(systemd_unit_object_path(unit))
        .arg("org.freedesktop.systemd1.Service")
        .arg("ExecStart")
}

/// systemd maps a unit name to its D-Bus object with `bus_label_escape`: ASCII letters and
/// non-leading digits stay as-is, and every other byte becomes an underscore plus two hex digits.
fn systemd_unit_object_path(unit: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut path = String::from("/org/freedesktop/systemd1/unit/");
    for (index, byte) in unit.bytes().enumerate() {
        if byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()) {
            path.push(char::from(byte));
        } else {
            path.push('_');
            path.push(char::from(HEX[usize::from(byte >> 4)]));
            path.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    path
}

/// The service contract, as the tracked unit states it.
#[derive(Debug, Default, PartialEq, Eq)]
struct UnitContract {
    user: Option<String>,
    exec_start: String,
    working_directory: Option<String>,
    limit_nofile: Option<String>,
    standard_error: Option<String>,
}

impl UnitContract {
    fn parse(text: &str) -> Result<Self> {
        let mut contract = UnitContract::default();
        for line in logical_lines(text) {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with('#')
                || line.starts_with(';')
                || line.starts_with('[')
            {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "User" => contract.user = Some(value),
                "ExecStart" => contract.exec_start = value,
                "WorkingDirectory" => contract.working_directory = Some(value),
                "LimitNOFILE" => contract.limit_nofile = Some(value),
                "StandardError" => contract.standard_error = Some(value),
                _ => {}
            }
        }
        if contract.exec_start.is_empty() {
            return Err(Error::PrerequisiteMissing(format!(
                "the tracked {} declares no ExecStart, so there is no standalone binary, data \
                 directory or listen address to reconcile against.",
                ProjectLayout::STANDALONE_UNIT
            )));
        }
        tracked_exec_start(&contract.exec_start)?;
        Ok(contract)
    }

    /// The executable the unit supervises — the first word of `ExecStart`.
    fn binary(&self) -> &str {
        self.exec_start
            .split_whitespace()
            .next()
            .unwrap_or_default()
    }

    /// The node's persistent state, as `--data-dir` names it (falling back to `WorkingDirectory`).
    fn data_dir(&self) -> Option<&str> {
        self.exec_arg("--data-dir")
            .or(self.working_directory.as_deref())
    }

    /// The endpoint the node serves, as `--listen-addr` names it.
    fn listen_addr(&self) -> Option<&str> {
        self.exec_arg("--listen-addr")
    }

    /// Where standalone's stderr is appended, for the pointer in a restart failure.
    fn log_path(&self) -> Option<&str> {
        self.standard_error
            .as_deref()
            .and_then(|value| value.strip_prefix("append:"))
    }

    fn exec_arg(&self, flag: &str) -> Option<&str> {
        let mut words = self.exec_start.split_whitespace();
        words.find(|word| *word == flag)?;
        words.next()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExecCommand {
    path: String,
    argv: Vec<String>,
    ignore_errors: bool,
}

impl ExecCommand {
    fn describe(&self) -> String {
        format!(
            "path={} argv[]={:?}; ignore_errors={}",
            self.path,
            self.argv,
            if self.ignore_errors { "yes" } else { "no" }
        )
    }
}

/// Parse the subset of ExecStart syntax used by the tracked service: plain whitespace-separated
/// words. Refusing quoted, escaped, expanded, or prefixed commands avoids implementing systemd's
/// full command-line grammar here.
fn tracked_exec_start(value: &str) -> Result<ExecCommand> {
    if value
        .chars()
        .any(|character| matches!(character, '\'' | '"' | '\\' | '$' | '%' | ';'))
    {
        return Err(Error::PrerequisiteMissing(format!(
            "the tracked {} uses ExecStart quoting, escaping, expansion, or multiple commands that \
             `service reconcile` cannot compare safely. Use plain arguments in the tracked unit.",
            ProjectLayout::STANDALONE_UNIT
        )));
    }
    let argv: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    let Some(path) = argv.first() else {
        return Err(Error::PrerequisiteMissing(format!(
            "the tracked {} declares no ExecStart, so there is no standalone binary, data \
             directory or listen address to reconcile against.",
            ProjectLayout::STANDALONE_UNIT
        )));
    };
    if !path.starts_with('/') {
        return Err(Error::PrerequisiteMissing(format!(
            "the tracked {} uses an ExecStart command prefix or non-absolute executable that \
             `service reconcile` cannot compare safely. Use an absolute executable path.",
            ProjectLayout::STANDALONE_UNIT
        )));
    }
    Ok(ExecCommand {
        path: path.clone(),
        argv,
        ignore_errors: false,
    })
}

/// Join physical lines the way systemd's configuration parser does before it reads directives.
/// An odd run of trailing backslashes continues the line; the last backslash becomes a space.
/// Comment lines are discarded before continuation handling, so a continued value resumes after
/// an intervening `#` or `;` comment.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut continued = String::new();

    for physical in text.lines() {
        let leading_trimmed = physical.trim_start();
        if leading_trimmed.starts_with('#') || leading_trimmed.starts_with(';') {
            continue;
        }

        continued.push_str(physical);
        let trailing_backslashes = physical
            .as_bytes()
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if trailing_backslashes % 2 == 1 {
            continued.pop();
            continued.push(' ');
            continue;
        }

        lines.push(std::mem::take(&mut continued));
    }

    if !continued.is_empty() {
        lines.push(continued);
    }
    lines
}

/// Everything the unit needs from the host before it can start: the service account, the
/// standalone binary, the persistent data directory, and the stderr log's directory.
///
/// Each is a refusal, not a repair. Creating a data directory or a service account here would
/// silently give a node a home the operator never chose.
fn check_prerequisites(contract: &UnitContract, runner: &dyn ProcessRunner) -> Result<()> {
    runner
        .run_and_wait(
            &CommandSpec::new("busctl")
                .arg("--json=short")
                .arg("--version"),
        )
        .map_err(|error| {
            Error::PrerequisiteMissing(format!(
                "`service reconcile` needs `busctl --json=short` to verify the effective start \
                 command without losing argument boundaries ({error}). Install a systemd busctl \
                 build with JSON output support before reconciling the unit."
            ))
        })?;

    if let Some(user) = &contract.user {
        runner
            .run_and_wait(&CommandSpec::new("id").arg(user))
            .map_err(|_| {
                Error::PrerequisiteMissing(format!(
                    "the unit runs as `{user}`, and this host has no such account. Create the \
                     non-login service account first: `sudo useradd --system --shell \
                     /usr/sbin/nologin {user}`."
                ))
            })?;
    }

    let binary = contract.binary();
    require_path(
        runner,
        "-x",
        binary,
        &format!(
            "the unit supervises {binary}, which is missing or not executable. Install the pinned \
             spacetimedb-standalone build there before reconciling the unit."
        ),
    )?;

    if let Some(dir) = contract.data_dir() {
        let user = contract.user.as_deref().unwrap_or("lyracore");
        require_path(
            runner,
            "-d",
            dir,
            &format!(
                "the node's persistent database directory {dir} does not exist. Create it, owned \
                 by the service account: `sudo install -d -o {user} -g {user} {dir}`. \
                 `service reconcile` never creates, moves or deletes this directory."
            ),
        )?;
    }

    if let Some(log) = contract.log_path() {
        let dir = log.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("/");
        let user = contract.user.as_deref().unwrap_or("lyracore");
        require_path(
            runner,
            "-d",
            dir,
            &format!(
                "the unit appends standalone's stderr to {log}, and {dir} does not exist. A \
                 restart would fail and leave no evidence. Create it: `sudo install -d -o {user} \
                 -g {user} {dir}`."
            ),
        )?;
    }
    Ok(())
}

fn require_path(
    runner: &dyn ProcessRunner,
    test_flag: &str,
    path: &str,
    message: &str,
) -> Result<()> {
    runner
        .run_and_wait(&CommandSpec::new("test").arg(test_flag).arg(path))
        .map(|_| ())
        .map_err(|_| Error::PrerequisiteMissing(message.to_string()))
}

/// Refuse when another ACTIVE service already owns this node's persistent state or its endpoint.
///
/// The drift this exists for is a hand-managed `spacetimedb.service` predating the tracked unit.
/// Starting the tracked unit beside it would put two nodes on one data directory and one port.
/// v1 names the offending unit and stops; it never migrates or stops someone else's service.
fn refuse_conflicting_service(
    unit: &str,
    contract: &UnitContract,
    runner: &dyn ProcessRunner,
) -> Result<()> {
    let listed = runner.run_and_wait(
        &systemctl()
            .arg("list-units")
            .arg("--type=service")
            .arg("--state=active")
            .arg("--no-legend")
            .arg("--plain"),
    )?;
    let others: Vec<String> = listed
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find(|word| word.ends_with(".service"))
        })
        .filter(|name| *name != unit)
        .map(str::to_string)
        .collect();
    if others.is_empty() {
        return Ok(());
    }

    let mut show = systemctl().arg("show");
    for name in &others {
        show = show.arg(name);
    }
    let shown = runner.run_and_wait(
        &show
            .arg("--property=Id")
            .arg("--property=ExecStart")
            .arg("--property=WorkingDirectory"),
    )?;

    for block in shown.split("\n\n") {
        let mut id = String::new();
        let mut exec_start = String::new();
        let mut working_directory = String::new();
        for line in block.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            match key {
                "Id" => id = value.to_string(),
                "ExecStart" => exec_start = value.to_string(),
                "WorkingDirectory" => working_directory = value.to_string(),
                _ => {}
            }
        }
        if id.is_empty() || id == unit {
            continue;
        }
        let owns = |claim: Option<&str>| {
            claim.is_some_and(|claim| {
                !claim.is_empty()
                    && (exec_start.contains(claim) || working_directory.trim() == claim)
            })
        };
        let reason = if owns(contract.data_dir()) {
            format!(
                "the node's persistent data directory {}",
                contract.data_dir().unwrap_or_default()
            )
        } else if owns(contract.listen_addr()) {
            format!(
                "the node's listen address {}",
                contract.listen_addr().unwrap_or_default()
            )
        } else {
            continue;
        };
        return Err(Error::Process(format!(
            "refusing to install {unit}: the active service `{id}` already owns {reason}. Two \
             node services would race for the same state and port. Stop and disable the old one \
             yourself, confirm the node is down, then re-run this command:\n  sudo systemctl \
             disable --now {id}"
        )));
    }
    Ok(())
}

/// Read the effective properties back after the restart.
///
/// A successful `systemctl restart` only means systemd accepted the job. A unit that starts and
/// exits reports `failed` here. The start command comes from the loaded unit plus its drop-ins, so
/// comparing it here catches an override that survived installation and `daemon-reload`.
fn verify(unit: &str, contract: &UnitContract, runner: &dyn ProcessRunner) -> Result<()> {
    println!("· verifying the effective service contract...");
    let shown = runner.run_and_wait(
        &systemctl()
            .arg("show")
            .arg(unit)
            .arg("--property=ActiveState")
            .arg("--property=DropInPaths")
            .arg("--property=LimitNOFILE")
            .arg("--property=StandardError"),
    )?;
    let property = |name: &str| -> Option<String> {
        shown.lines().find_map(|line| {
            line.trim()
                .strip_prefix(&format!("{name}="))
                .map(str::to_string)
        })
    };

    let mut wrong: Vec<String> = Vec::new();
    match property("ActiveState").as_deref() {
        Some("active") => {}
        other => wrong.push(format!(
            "ActiveState is {} (expected active)",
            other.unwrap_or("unreported")
        )),
    }
    let expected_exec_start = tracked_exec_start(&contract.exec_start)
        .expect("UnitContract::parse accepts only supported ExecStart syntax");
    // `systemctl show` flattens argv into one string, so a one-argument `"two words"` override
    // looks identical to two separate arguments. D-Bus keeps the argument array typed.
    let exec_start_property = runner.run_and_wait(&busctl_exec_start(unit)).map_err(|error| {
        Error::Process(format!(
            "{unit} was installed and restarted, but its typed ExecStart property could not be \
             read through busctl: {error}\nThis host is NOT reconciled. Inspect it with `systemctl status \
             {unit}` and `journalctl -u {unit} --no-pager -n 100`."
        ))
    })?;
    let actual_exec_start = effective_exec_start(&exec_start_property);
    if actual_exec_start.as_ref() != Ok(&expected_exec_start) {
        wrong.push(format!(
            "ExecStart is {} (the tracked unit requires {})",
            match &actual_exec_start {
                Ok(actual) => actual.describe(),
                Err(reason) => reason.clone(),
            },
            expected_exec_start.describe()
        ));
        if let Some(paths) = property("DropInPaths").filter(|paths| !paths.trim().is_empty()) {
            wrong.push(format!("applicable drop-ins: {paths}"));
        }
    }
    for (name, expected) in [
        ("LimitNOFILE", contract.limit_nofile.as_deref()),
        ("StandardError", contract.standard_error.as_deref()),
    ] {
        let Some(expected) = expected else { continue };
        match property(name) {
            Some(actual) if actual == expected => {}
            other => wrong.push(format!(
                "{name} is {} (the tracked unit requires {expected})",
                other.as_deref().unwrap_or("unreported")
            )),
        }
    }
    if wrong.is_empty() {
        return Ok(());
    }
    Err(Error::Process(format!(
        "{unit} was installed and restarted, but the host does not match the tracked service \
         contract:\n{}\nThis host is NOT reconciled. Inspect it with `systemctl status {unit}` \
         and `journalctl -u {unit} --no-pager -n 100`.",
        wrong
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

/// Parse busctl's JSON representation of systemd's typed ExecStart D-Bus property. Only the path,
/// argv array, and ignore flag affect the contract; the remaining fields are runtime status. One
/// service command is the supported shape for this Type=simple unit.
fn effective_exec_start(property: &str) -> std::result::Result<ExecCommand, String> {
    let property: serde_json::Value = serde_json::from_str(property)
        .map_err(|error| format!("unreadable from systemd D-Bus ({error})"))?;
    let signature = property
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing a D-Bus type".to_string())?;
    if signature != "a(sasbttttuii)" {
        return Err(format!("in unsupported D-Bus type {signature}"));
    }
    let commands = property
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "missing the D-Bus command array".to_string())?;
    if commands.len() != 1 {
        return Err(format!("{} effective commands", commands.len()));
    }
    let command = commands[0]
        .as_array()
        .ok_or_else(|| "with an invalid D-Bus command record".to_string())?;
    let path = command
        .first()
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "without an executable path".to_string())?;
    let argv = command
        .get(1)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "without an argv[] array".to_string())?
        .iter()
        .map(|argument| {
            argument
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "with a non-string argv[] entry".to_string())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let ignore_errors = command
        .get(2)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "without an ignore_errors flag".to_string())?;
    Ok(ExecCommand {
        path: path.to_string(),
        argv,
        ignore_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::update::testing::{ahead_stack, same_sha_stack};
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    /// A copy of the server repository's `deploy/systemd/spacetimedb-standalone.service`. The
    /// artifact test in that repository owns its contract; this is the input the CLI reads.
    const UNIT_TEXT: &str = "\
[Unit]
Description=LyraCore SpacetimeDB standalone
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=lyracore
Group=lyracore
WorkingDirectory=/var/lib/lyracore/spacetimedb
ExecStart=/opt/lyracore/spacetimedb/spacetimedb-standalone start --listen-addr 127.0.0.1:3000 \
--data-dir /var/lib/lyracore/spacetimedb --non-interactive
Restart=always
RestartSec=2s
LimitNOFILE=524288
StandardError=append:/var/log/lyracore/spacetimedb-standalone.log

[Install]
WantedBy=multi-user.target
";

    const TRACKED_BINARY: &str = "/opt/lyracore/spacetimedb/spacetimedb-standalone";
    const TRACKED_ARGV: &[&str] = &[
        TRACKED_BINARY,
        "start",
        "--listen-addr",
        "127.0.0.1:3000",
        "--data-dir",
        "/var/lib/lyracore/spacetimedb",
        "--non-interactive",
    ];
    const TRACKED_STDERR: &str = "append:/var/log/lyracore/spacetimedb-standalone.log";

    fn effective_properties(
        active_state: &str,
        drop_in_paths: &str,
        limit_nofile: &str,
        standard_error: &str,
    ) -> String {
        format!(
            "ActiveState={active_state}\n\
             DropInPaths={drop_in_paths}\n\
             LimitNOFILE={limit_nofile}\n\
             StandardError={standard_error}\n"
        )
    }

    fn exec_start_property(path: &str, argv: &[&str]) -> String {
        serde_json::json!({
            "type": "a(sasbttttuii)",
            "data": [[path, argv, false, 0, 0, 0, 0, 0, 0, 0]],
        })
        .to_string()
    }

    /// A checkout that tracks the unit, at a root the fake git stacks agree with.
    fn project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let unit = tmp.path().join(ProjectLayout::STANDALONE_UNIT);
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(unit, UNIT_TEXT).unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn unit_path(project: &ProjectLayout) -> String {
        project.standalone_unit().display().to_string()
    }

    /// A root invocation on a host that has every prerequisite, runs no conflicting service, and
    /// comes back matching the tracked contract.
    fn reconcilable_host(stack: FakeStack) -> FakeStack {
        stack
            .with_stdout("id -u", "0\n")
            .with_stdout(
                "list-units",
                "spacetimedb-standalone.service loaded active running LyraCore standalone\n\
                 sshd.service                   loaded active running OpenSSH\n",
            )
            .with_stdout(
                "--property=Id",
                "Id=sshd.service\n\
                 ExecStart={ path=/usr/sbin/sshd ; argv[]=/usr/sbin/sshd -D ; }\n\
                 WorkingDirectory=\n",
            )
            .with_stdout(
                "--property=ActiveState",
                &effective_properties("active", "", "524288", TRACKED_STDERR),
            )
            .with_stdout(
                "busctl --json=short get-property",
                &exec_start_property(TRACKED_BINARY, TRACKED_ARGV),
            )
    }

    /// The git half of the plan, as `update` runs it: fetch, the dirty-tree check, and the two
    /// revisions it compares.
    fn git_steps() -> Vec<String> {
        vec![
            "git fetch origin".to_string(),
            "git status --porcelain".to_string(),
            "git rev-parse HEAD".to_string(),
            "git rev-parse origin/main".to_string(),
        ]
    }

    // ---- what the tracked unit says ----

    #[test]
    fn the_contract_is_read_out_of_the_tracked_unit() {
        let contract = UnitContract::parse(UNIT_TEXT).unwrap();
        assert_eq!(contract.user.as_deref(), Some("lyracore"));
        assert_eq!(
            contract.binary(),
            "/opt/lyracore/spacetimedb/spacetimedb-standalone"
        );
        assert_eq!(contract.data_dir(), Some("/var/lib/lyracore/spacetimedb"));
        assert_eq!(contract.listen_addr(), Some("127.0.0.1:3000"));
        assert_eq!(contract.limit_nofile.as_deref(), Some("524288"));
        assert_eq!(
            contract.log_path(),
            Some("/var/log/lyracore/spacetimedb-standalone.log")
        );
    }

    #[test]
    fn continued_directives_parse_like_their_single_line_form() {
        let single = UnitContract::parse(UNIT_TEXT).unwrap();
        let continued = UnitContract::parse(
            r#"[Service]
Environment=FIRST=one \
    SECOND=two
ExecStart=/opt/lyracore/spacetimedb/spacetimedb-standalone start \
    --listen-addr 127.0.0.1:3000 \
# Comments do not end the continuation.
; Neither do semicolon comments.
    --data-dir /var/lib/lyracore/spacetimedb --non-interactive
WorkingDirectory=/var/lib/lyracore/spacetimedb
LimitNOFILE=524288
StandardError=append:/var/log/lyracore/spacetimedb-standalone.log
"#,
        )
        .unwrap();

        assert_eq!(continued.binary(), single.binary());
        assert_eq!(continued.data_dir(), single.data_dir());
        assert_eq!(continued.listen_addr(), single.listen_addr());
        assert_eq!(
            tracked_exec_start(&continued.exec_start).unwrap(),
            tracked_exec_start(&single.exec_start).unwrap()
        );
        assert_eq!(continued.limit_nofile, single.limit_nofile);
        assert_eq!(continued.standard_error, single.standard_error);
    }

    #[test]
    fn an_escaped_terminal_backslash_does_not_continue_the_directive() {
        let contract = UnitContract::parse(
            r#"[Service]
Environment=WINDOWS_PATH=C:\\
ExecStart=/usr/bin/standalone start
"#,
        )
        .unwrap();

        assert_eq!(contract.binary(), "/usr/bin/standalone");
    }

    #[test]
    fn an_exec_start_outside_the_supported_source_syntax_is_refused() {
        for exec_start in [
            "/usr/bin/standalone --label='two words'",
            "/usr/bin/standalone $EXTRA",
            "@/usr/bin/standalone custom-argv-zero",
        ] {
            let error = UnitContract::parse(&format!("[Service]\nExecStart={exec_start}\n"))
                .unwrap_err()
                .to_string();

            assert!(
                error.contains("cannot compare safely"),
                "{exec_start}: {error}"
            );
        }
    }

    #[test]
    fn a_unit_without_an_exec_start_is_refused() {
        let error = UnitContract::parse("[Service]\nUser=lyracore\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no ExecStart"), "{error}");
    }

    #[test]
    fn a_checkout_without_the_tracked_unit_is_refused_before_any_host_command() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = reconcilable_host(same_sha_stack());

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains(ProjectLayout::STANDALONE_UNIT), "{error}");
        let mut expected = vec!["id -u".to_string()];
        expected.extend(git_steps());
        assert_eq!(stack.rendered(), expected);
    }

    /// The same refusal from a checkout that was BEHIND `origin/main`, which is the case that
    /// shows the real ordering: the reset lands first, deliberately, because the unit to install
    /// is read out of the UPDATED checkout. The host is still untouched afterwards.
    #[test]
    fn the_reset_lands_before_the_unit_is_read_and_leaves_the_host_untouched() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        let stack = reconcilable_host(ahead_stack());

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains(ProjectLayout::STANDALONE_UNIT), "{error}");
        let mut expected = vec!["id -u".to_string()];
        expected.extend(git_steps());
        expected.push("git reset --hard origin/main".to_string());
        assert_eq!(stack.rendered(), expected);
    }

    // ---- the root check ----

    #[test]
    fn a_non_root_invocation_is_refused_before_anything_else_runs() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout("id -u", "1000\n");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("sudo ./lyracore service reconcile"),
            "{error}"
        );
        assert_eq!(
            stack.rendered(),
            vec!["id -u".to_string()],
            "not even the fetch may run without the privilege the plan needs"
        );
    }

    #[test]
    fn an_unreadable_user_id_is_refused_rather_than_assumed_to_be_root() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout("id -u", "nobody\n");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("/etc/systemd/system"), "{error}");
        assert_eq!(stack.rendered(), vec!["id -u".to_string()]);
    }

    // ---- the git half ----

    #[test]
    fn a_dirty_tree_blocks_the_service_change_too() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout("status --porcelain", " M module/src/foo.rs\n");

        let error = reconcile(&project, &stack.runner())
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
    fn an_already_current_checkout_still_reconciles_the_supervisor() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(same_sha_stack());

        reconcile(&project, &stack.runner()).unwrap();

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

    // ---- the happy path ----

    #[test]
    fn reconciliation_runs_the_runbook_steps_in_order() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack());

        reconcile(&project, &stack.runner()).unwrap();

        let mut expected = vec!["id -u".to_string()];
        expected.extend(git_steps());
        expected.extend([
            "git reset --hard origin/main".to_string(),
            "busctl --json=short --version".to_string(),
            "id lyracore".to_string(),
            "test -x /opt/lyracore/spacetimedb/spacetimedb-standalone".to_string(),
            "test -d /var/lib/lyracore/spacetimedb".to_string(),
            "test -d /var/log/lyracore".to_string(),
            "systemctl --no-pager list-units --type=service --state=active --no-legend --plain"
                .to_string(),
            "systemctl --no-pager show sshd.service --property=Id --property=ExecStart \
             --property=WorkingDirectory"
                .to_string(),
            format!(
                "install -o root -g root -m 0644 {} \
                 /etc/systemd/system/spacetimedb-standalone.service",
                unit_path(&project)
            ),
            "systemctl --no-pager daemon-reload".to_string(),
            "systemctl --no-pager enable spacetimedb-standalone.service".to_string(),
            "systemctl --no-pager restart spacetimedb-standalone.service".to_string(),
            "systemctl --no-pager show spacetimedb-standalone.service --property=ActiveState \
             --property=DropInPaths --property=LimitNOFILE --property=StandardError"
                .to_string(),
            "busctl --json=short get-property org.freedesktop.systemd1 \
             /org/freedesktop/systemd1/unit/spacetimedb_2dstandalone_2eservice \
             org.freedesktop.systemd1.Service ExecStart"
                .to_string(),
        ]);
        assert_eq!(stack.rendered(), expected);
    }

    #[test]
    fn the_persistent_data_directory_is_only_ever_read() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack());

        reconcile(&project, &stack.runner()).unwrap();

        for rendered in stack.rendered() {
            if !rendered.contains("/var/lib/lyracore/spacetimedb") {
                continue;
            }
            assert_eq!(
                rendered, "test -d /var/lib/lyracore/spacetimedb",
                "only an existence check may name the node's persistent state"
            );
        }
    }

    // ---- missing host prerequisites ----

    /// Each prerequisite failure must stop BEFORE the host changes, so a broken host is never left
    /// with a new unit file, a boot-time symlink, or a restarted service.
    ///
    /// `enable` and `daemon-reload` count: `enable` writes a `multi-user.target.wants` symlink that
    /// outlives the run.
    fn assert_refused_before_mutation(stack: &FakeStack, error: &str, needle: &str) {
        assert!(error.contains(needle), "{error}");
        for forbidden in ["install ", "daemon-reload", "enable ", "restart"] {
            assert!(
                !stack.rendered().iter().any(|r| r.contains(forbidden)),
                "{forbidden}: {:?}",
                stack.rendered()
            );
        }
    }

    #[test]
    fn missing_busctl_json_support_is_refused_before_mutation() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .fail_on("busctl --json=short --version", "unknown option --json");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "busctl --json=short");
    }

    #[test]
    fn a_missing_service_account_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).fail_on("id lyracore", "no such user");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "useradd");
    }

    #[test]
    fn a_missing_standalone_binary_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).fail_on("test -x", "not executable");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(
            &stack,
            &error,
            "/opt/lyracore/spacetimedb/spacetimedb-standalone",
        );
    }

    #[test]
    fn a_missing_data_directory_is_refused_and_never_created() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .fail_on("test -d /var/lib/lyracore/spacetimedb", "no such directory");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "never creates, moves or deletes");
    }

    #[test]
    fn a_missing_stderr_log_directory_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack =
            reconcilable_host(ahead_stack()).fail_on("test -d /var/log/lyracore", "no such dir");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "/var/log/lyracore");
    }

    // ---- the legacy service refusal ----

    #[test]
    fn an_active_service_owning_the_data_directory_is_named_and_refused() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "list-units",
                "spacetimedb.service loaded active running node\n",
            )
            .with_stdout(
                "--property=Id",
                "Id=spacetimedb.service\n\
                 ExecStart={ path=/usr/local/bin/spacetimedb ; argv[]=/usr/local/bin/spacetimedb \
                 start --data-dir /var/lib/lyracore/spacetimedb ; }\n\
                 WorkingDirectory=\n",
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("spacetimedb.service"), "{error}");
        assert!(
            error.contains("disable --now spacetimedb.service"),
            "{error}"
        );
        assert_refused_before_mutation(&stack, &error, "persistent data directory");
    }

    #[test]
    fn an_active_service_owning_the_listen_address_is_named_and_refused() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "list-units",
                "stdb-old.service loaded active running node\n",
            )
            .with_stdout(
                "--property=Id",
                "Id=stdb-old.service\n\
                 ExecStart={ path=/usr/bin/stdb ; argv[]=/usr/bin/stdb start --listen-addr \
                 127.0.0.1:3000 --data-dir /srv/stdb ; }\n\
                 WorkingDirectory=/srv/stdb\n",
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "listen address 127.0.0.1:3000");
    }

    #[test]
    fn a_continued_tracked_command_still_detects_a_conflicting_active_service() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let continued_exec = ["spacetimedb-standalone start \\", "    --listen-addr"].join("\n");
        let continued = UNIT_TEXT.replace(
            "spacetimedb-standalone start --listen-addr",
            &continued_exec,
        );
        std::fs::write(project.standalone_unit(), continued).unwrap();
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "list-units",
                "stdb-old.service loaded active running node\n",
            )
            .with_stdout(
                "--property=Id",
                "Id=stdb-old.service\n\
                 ExecStart={ path=/usr/bin/stdb ; argv[]=/usr/bin/stdb start --listen-addr \
                 127.0.0.1:3000 --data-dir /srv/stdb ; }\n\
                 WorkingDirectory=/srv/stdb\n",
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert_refused_before_mutation(&stack, &error, "listen address 127.0.0.1:3000");
    }

    #[test]
    fn an_unrelated_active_service_does_not_block_reconciliation() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack());

        reconcile(&project, &stack.runner()).unwrap();

        assert!(
            stack
                .rendered()
                .iter()
                .any(|r| r == "systemctl --no-pager restart spacetimedb-standalone.service"),
            "{:?}",
            stack.rendered()
        );
    }

    // ---- the restart and its verification ----

    #[test]
    fn a_failed_restart_stops_with_the_log_pointers_and_never_verifies() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .fail_on("restart spacetimedb-standalone", "Job failed");

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("journalctl -u spacetimedb-standalone"),
            "{error}"
        );
        assert!(
            error.contains("/var/log/lyracore/spacetimedb-standalone.log"),
            "{error}"
        );
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.contains("--property=ActiveState")),
            "{:?}",
            stack.rendered()
        );
    }

    #[test]
    fn an_inactive_unit_after_restart_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout(
            "--property=ActiveState",
            &effective_properties("failed", "", "524288", TRACKED_STDERR),
        );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("ActiveState is failed"), "{error}");
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn an_unreadable_typed_command_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).fail_on(
            "busctl --json=short get-property",
            "D-Bus property read failed",
        );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("typed ExecStart property"), "{error}");
        assert!(error.contains("D-Bus property read failed"), "{error}");
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn an_effective_command_changed_by_a_drop_in_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "--property=ActiveState",
                &effective_properties(
                    "active",
                    "/etc/systemd/system/spacetimedb-standalone.service.d/override.conf",
                    "524288",
                    TRACKED_STDERR,
                ),
            )
            .with_stdout(
                "busctl --json=short get-property",
                &exec_start_property(
                    TRACKED_BINARY,
                    &[
                        TRACKED_BINARY,
                        "start",
                        "--listen-addr",
                        "127.0.0.1:3000",
                        "--data-dir",
                        "/srv/other",
                        "--non-interactive",
                    ],
                ),
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("ExecStart is"), "{error}");
        assert!(error.contains("/srv/other"), "{error}");
        assert!(
            error.contains("/etc/systemd/system/spacetimedb-standalone.service.d/override.conf")
        );
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn an_effective_executable_path_drift_is_reported_even_when_argv_matches() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "--property=ActiveState",
                &effective_properties(
                    "active",
                    "/etc/systemd/system/spacetimedb-standalone.service.d/override.conf",
                    "524288",
                    TRACKED_STDERR,
                ),
            )
            .with_stdout(
                "busctl --json=short get-property",
                &exec_start_property("/srv/override/spacetimedb-standalone", TRACKED_ARGV),
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("path=/srv/override/spacetimedb-standalone"),
            "{error}"
        );
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn effective_argument_boundaries_are_compared_without_flattening() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "--property=ActiveState",
                &effective_properties(
                    "active",
                    "/etc/systemd/system/spacetimedb-standalone.service.d/override.conf",
                    "524288",
                    TRACKED_STDERR,
                ),
            )
            .with_stdout(
                "busctl --json=short get-property",
                &exec_start_property(
                    TRACKED_BINARY,
                    &[
                        TRACKED_BINARY,
                        "start --listen-addr",
                        "127.0.0.1:3000",
                        "--data-dir",
                        "/var/lib/lyracore/spacetimedb",
                        "--non-interactive",
                    ],
                ),
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("\"start --listen-addr\""), "{error}");
        assert!(error.contains("\"start\", \"--listen-addr\""), "{error}");
        assert!(error.contains("override.conf"), "{error}");
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn multiple_effective_start_commands_are_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack())
            .with_stdout(
                "--property=ActiveState",
                &effective_properties(
                    "active",
                    "/etc/systemd/system/spacetimedb-standalone.service.d/override.conf",
                    "524288",
                    TRACKED_STDERR,
                ),
            )
            .with_stdout(
                "busctl --json=short get-property",
                &serde_json::json!({
                    "type": "a(sasbttttuii)",
                    "data": [
                        [TRACKED_BINARY, TRACKED_ARGV, false, 0, 0, 0, 0, 0, 0, 0],
                        ["/usr/bin/extra", ["/usr/bin/extra"], false, 0, 0, 0, 0, 0, 0, 0],
                    ],
                })
                .to_string(),
            );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("ExecStart is 2 effective commands"),
            "{error}"
        );
        assert!(error.contains("override.conf"), "{error}");
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn the_inherited_1024_descriptor_ceiling_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout(
            "--property=ActiveState",
            &effective_properties("active", "", "1024", TRACKED_STDERR),
        );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("LimitNOFILE is 1024 (the tracked unit requires 524288)"),
            "{error}"
        );
    }

    #[test]
    fn a_stderr_destination_that_keeps_no_evidence_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout(
            "--property=ActiveState",
            &effective_properties("active", "", "524288", "inherit"),
        );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("StandardError is inherit"), "{error}");
        assert!(
            error.contains("append:/var/log/lyracore/spacetimedb-standalone.log"),
            "{error}"
        );
    }
}
