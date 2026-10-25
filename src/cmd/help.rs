//! A single command catalogue for full and focused help.
use crate::{Error, Result};

pub const USAGE: &str = "\
lyracore: run a local LyraCore vanilla WoW Realm

USAGE:
  lyracore doctor          check the prerequisites for `dev up`
  lyracore dev up          publish and start the local Realm for setup
  lyracore account create USER
                           provision an Account with your chosen password
  lyracore import          import world content from public game data and
                           your own 1.12.1 client before play
  lyracore dev check       check imported content on every playable Shard
  lyracore dev watch       rebuild and reload Module source in a local Native Host
  lyracore dev status      report the running processes and published Shards
  lyracore dev down        stop the local Realm

Run `lyracore help --all` to see every command.";

struct Entry {
    path: &'static str,
    usage: &'static str,
    summary: &'static str,
}
macro_rules! commands {
    ($( $path:literal, $usage:literal, $summary:literal; )*) => {
        const ENTRIES: &[Entry] = &[$(Entry { path: $path, usage: $usage, summary: $summary },)*];
        pub const COMMAND_REFERENCE: &str = concat!("lyracore help [--all | COMMAND ...]\n", $("lyracore ", $usage, "\n",)*);
        pub const USAGE_ALL: &str = concat!(
            "lyracore: local Realm development\n\nUSAGE:\n",
            $("  lyracore ", $usage, "\n    ", $summary, "\n\n",)*
            "Use `lyracore COMMAND --help` for focused help.\nHost administration: lyracore-ops service reconcile.\nFull reference: docs/commands.md"
        );
    }
}
commands! {
    "doctor", "doctor", "Check prerequisites for dev up.";
    "bindings", "bindings [--check]", "Prepare Gateway Rust bindings outside the source tree. --check reports freshness as JSON without generating files.";
    "preflight", "preflight", "Validate the deploy build, schema, filters and artifact identities offline.";
    "publish", "publish [DATABASE ...] [--skip-preflight] [--server URL]", "Publish the Module. Native checkouts accept an explicit loopback HTTP URL with --server and use their private publisher credential. --skip-preflight skips the offline checks. Legacy checkouts retain their upgrade Gates. Data-wiping flags are refused.";
    "dev up", "dev up [--single] [--lan IP]", "Start the local Realm. Native Host checkouts use one Shard; older checkouts support --single. --lan serves game clients on a private address.";
    "dev watch", "dev watch", "Start or reuse a local Native Host with Development reload and watch Module source. Requires native-host/dev-reload.patch. Module definition changes are refused. Ctrl+C leaves the Host running; dev down stops it.";
    "dev status", "dev status", "Report local processes, listeners and published Shards.";
    "dev logs", "dev logs [spacetime|gateway] [--all]", "Show the last 100 lines (up to 64 KiB) per log. --all streams the complete files.";
    "dev check", "dev check", "Alias for import check using the recorded development topology.";
    "dev smoke", "dev smoke ACCOUNT CHARACTER [--password-stdin]", "Check content, then exercise login/world entry through the pinned Headless Client. Passwords use a hidden prompt or one stdin line.";
    "dev down", "dev down [--forget]", "Stop only processes this CLI owns. --forget drops their records without signalling them.";
    "account create", "account create USER [--password-stdin]", "Provision an Account using a hidden password prompt or one stdin line; never a password argument.";
    "character gm", "character gm NAME true|false", "Grant or revoke GM commands on the world shard holding the Character.";
    "import", "import [--accept] [--client-data PATH] [--profile-shard PROFILE=SHARD ...]", "Alias for import world. Import content before play from public game data and your own client archives.";
    "import world", "import world [--accept] [--client-data PATH] [--profile-shard PROFILE=SHARD ...]", "Import Eastern Kingdoms and Kalimdor content plus instances. --accept answers consent. Explicit profiles require a complete assignment; otherwise use the development topology.";
    "import check", "import check [--profile-shard PROFILE=SHARD ...]", "Check content readiness at the import destinations without changing their content.";
    "import vmaps", "import vmaps [--client-data PATH] [--profile-shard PROFILE=SHARD ...]", "Refresh collision geometry on each populated World Shard. Reads your client archives; clears content readiness until a full world import completes.";
    "config", "config", "Show the persisted client-data path.";
    "config set client-data", "config set client-data PATH", "Validate and remember your client Data directory for import and client sync.";
    "client sync", "client sync", "Pack and install patch-3.MPQ and enabled addons into the configured client. Reports leftover addons; does not delete them.";
    "client pack", "client pack --out DIR [--zip]", "Build a distributable artifact of package-authored content. --zip also writes DIR.zip; never installs into the configured client.";
    "packages add", "packages add FOLDER|GIT-URL|NAME [--yes]", "Review and install source; --yes confirms installation. Server content runs preflight; client-only content is validated when packed. Does not activate it.";
    "packages update", "packages update [NAME] [--yes]", "Update Git/official sources with drift protection and rollback. Disabled and client-only changes skip module preflight. Does not activate them.";
    "packages enable", "packages enable NAME", "Return disabled source to the build. Run packages apply to activate server content, client sync for client content.";
    "packages disable", "packages disable NAME [--yes]", "Tear down a recorded realm’s Package data and config, then exclude its source from the build. Re-enabling does not restore deleted data. --yes confirms teardown.";
    "packages remove", "packages remove NAME [--yes]", "Delete a disabled Package after drift checks and confirmation. Disable it first; --yes confirms deletion.";
    "packages apply", "packages apply [DATABASE ...] [--check] [--yes] [--force-all] [--client-data PATH]", "Prepare sources, publish Rust Packages and apply artifacts. --check may build local files but performs no Realm writes. --force-all reapplies matching artifacts; --yes confirms Realm writes.";
    "packages config", "packages config NAME [KEY [VALUE]] [--new] [--database NAME ...]", "Read or write Package settings across named Shards or the development topology. --new permits a key the Package did not seed.";
    "packages build", "packages build", "Build enabled Package sources. Datascripts require schema typings and typechecking; runtime scripts need only their builder. Empty/source-free inventories need no Bun.";
    "packages check", "packages check", "Check existing artifacts against their Build Identity without rebuilding artifacts. Distinct from packages apply --check, which prepares an activation plan.";
    "packages list", "packages list", "List enabled and disabled Packages, origins and source drift.";
    "packages new", "packages new NAME [--from RUNG]", "Scaffold a compatible Reference Package. Defaults to example-script; --from selects a different reference.";
    "production status", "production status --server URI --gateway-log PATH --realm-core DB DATABASE ...", "Read-only verification of the complete explicit topology and latest gateway startup. Include realm-core in DATABASE ...; gateway-log is a local file.";
    "update", "update [--reset-mirror]", "Update Core without dropping local commits. --reset-mirror explicitly replaces mirror history and saves a recovery branch. This does not restart the realm.";
}

pub fn topic(topic: &str) -> Result<String> {
    let selected: Vec<_> = ENTRIES
        .iter()
        .filter(|entry| entry.path == topic || entry.path.starts_with(&format!("{topic} ")))
        .collect();
    if selected.is_empty() {
        return Err(Error::Usage(format!(
            "unknown help topic '{topic}'; use `lyracore help --all`"
        )));
    }
    let mut text = String::from("USAGE:\n");
    for entry in selected {
        text.push_str(&format!(
            "  lyracore {}\n    {}\n\n",
            entry.usage, entry.summary
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::Command;
    #[test]
    fn every_registered_command_has_focused_help_without_execution() {
        for entry in ENTRIES {
            let mut args: Vec<_> = entry.path.split_whitespace().map(str::to_string).collect();
            args.push("--help".into());
            assert_eq!(
                Command::parse(&args).unwrap(),
                Command::HelpTopic(entry.path.into())
            );
            assert!(topic(entry.path).unwrap().contains(entry.usage));
        }
    }
    #[test]
    fn command_reference_matches_the_help_catalogue() {
        let reference = include_str!("../../docs/commands.md");
        let block = reference
            .split("<!-- command-reference:start -->\n```text\n")
            .nth(1)
            .unwrap()
            .split("```\n<!-- command-reference:end -->")
            .next()
            .unwrap();
        assert_eq!(
            block, COMMAND_REFERENCE,
            "update the command list from the help catalogue"
        );
    }
    #[test]
    fn group_help_does_not_include_other_groups() {
        let help = topic("packages").unwrap();
        assert!(help.contains("packages apply"));
        assert!(!help.contains("lyracore dev up"));
        assert!(topic("packages invented").is_err());
    }
}
