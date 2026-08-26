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
//! service contract it verifies — descriptor limit, stderr destination, data directory, listen
//! address — is READ OUT of the tracked unit, never duplicated here, so this CLI cannot claim a
//! host is reconciled against a contract the checkout no longer ships.
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
/// Refuses — before touching anything — when the invocation is not root, when the checkout has
/// local work a reset would discard, when a host prerequisite is missing, or when another active
/// service already owns the node's data directory or listen address.
pub fn reconcile(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    // Fail fast, before the network and before anything on disk moves: every later step writes to
    // /etc or drives systemd, and a half-done privileged plan is worse than one that never
    // started.
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
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ProjectLayout::STANDALONE_UNIT.to_string());
    let target = format!("{SYSTEMD_UNIT_DIR}/{unit}");

    println!("· checking host prerequisites...");
    check_prerequisites(&contract, runner)?;

    println!("· looking for a service that already owns this node...");
    refuse_conflicting_service(&unit, &contract, runner)?;

    println!("· installing {unit}...");
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
    println!("{unit} is active and matches the tracked service contract.");
    Ok(())
}

fn systemctl() -> CommandSpec {
    CommandSpec::new("systemctl").arg("--no-pager")
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
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
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

/// Everything the unit needs from the host before it can start: the service account, the
/// standalone binary, the persistent data directory, and the stderr log's directory.
///
/// Each is a refusal, not a repair. Creating a data directory or a service account here would
/// silently give a node a home the operator never chose.
fn check_prerequisites(contract: &UnitContract, runner: &dyn ProcessRunner) -> Result<()> {
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

/// Read the EFFECTIVE properties back after the restart — the same three the runbook checks.
///
/// A successful `systemctl restart` only means systemd accepted the job. A unit that starts and
/// exits reports `failed` here, and a descriptor limit or stderr destination that did not take
/// effect is exactly the drift #194 was filed over.
fn verify(unit: &str, contract: &UnitContract, runner: &dyn ProcessRunner) -> Result<()> {
    println!("· verifying the effective service contract...");
    let shown = runner.run_and_wait(
        &systemctl()
            .arg("show")
            .arg(unit)
            .arg("--property=ActiveState")
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
                "ActiveState=active\n\
                 LimitNOFILE=524288\n\
                 StandardError=append:/var/log/lyracore/spacetimedb-standalone.log\n",
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
             --property=LimitNOFILE --property=StandardError"
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

    /// Each prerequisite failure must stop BEFORE the install, so a broken host is never left with
    /// a new unit file or a restarted service.
    fn assert_refused_before_mutation(stack: &FakeStack, error: &str, needle: &str) {
        assert!(error.contains(needle), "{error}");
        assert!(
            !stack
                .rendered()
                .iter()
                .any(|r| r.starts_with("install ") || r.contains("restart")),
            "{:?}",
            stack.rendered()
        );
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
            "ActiveState=failed\n\
             LimitNOFILE=524288\n\
             StandardError=append:/var/log/lyracore/spacetimedb-standalone.log\n",
        );

        let error = reconcile(&project, &stack.runner())
            .unwrap_err()
            .to_string();

        assert!(error.contains("ActiveState is failed"), "{error}");
        assert!(error.contains("NOT reconciled"), "{error}");
    }

    #[test]
    fn the_inherited_1024_descriptor_ceiling_is_reported_as_unreconciled() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let stack = reconcilable_host(ahead_stack()).with_stdout(
            "--property=ActiveState",
            "ActiveState=active\n\
             LimitNOFILE=1024\n\
             StandardError=append:/var/log/lyracore/spacetimedb-standalone.log\n",
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
            "ActiveState=active\nLimitNOFILE=524288\nStandardError=inherit\n",
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
