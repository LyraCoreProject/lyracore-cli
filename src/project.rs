//! The ONE adapter holding the server project's internal package, database, script, and bind names.
//!
//! Everything the public `lyracore` command contract touches routes through the constants below, so
//! renaming the server's internals is a single-file edit here and no command surface moves with it.
//!
//! That is not theoretical: LyraCore#241 renamed the database (`spacetime-core` → `lyracore`), the
//! gateway package and binary, and every `GW_*` environment variable to `LYRACORE_*`. Absorbing it
//! meant editing these constants and the environment names in `cmd/dev.rs` — no command, flag, exit
//! code, or output format changed.

use crate::proc::CommandSpec;
use crate::{Error, Result};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Where the CLIENT-FACING listeners (logon + world) bind.
///
/// SpacetimeDB is deliberately absent from this choice: it is loopback in *every* mode. The
/// database speaks an unauthenticated-by-default admin protocol on :3000, and `--lan` exists so a
/// second machine can run a 1.12.1 client — not so it can publish modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientBind {
    /// The default: only this machine can reach the realm.
    Loopback,
    /// A private-LAN IPv4 address of this machine, from `dev up --lan <IP>`.
    Lan(Ipv4Addr),
}

impl ClientBind {
    /// The host the logon/world listeners bind to, and the host a client must connect to.
    pub fn host(&self) -> String {
        match self {
            ClientBind::Loopback => Ipv4Addr::LOCALHOST.to_string(),
            ClientBind::Lan(ip) => ip.to_string(),
        }
    }

    /// Parse `--lan <IP>`.
    ///
    /// Only RFC1918 addresses are accepted. A public address would put an alpha game server with
    /// no rate limiting and a 2004 password hash on the internet from a `dev` command; `0.0.0.0`
    /// would do the same by accident, on every interface at once. Neither is a private LAN, and
    /// neither is something this fixture should be able to do by typo.
    pub fn parse_lan(raw: &str) -> Result<Self> {
        let ip: Ipv4Addr = raw.trim().parse().map_err(|_| {
            Error::Usage(format!(
                "--lan needs a private IPv4 address of this machine, got '{raw}'"
            ))
        })?;
        if !ip.is_private() {
            return Err(Error::Usage(format!(
                "--lan refuses {ip}: it is not a private-LAN address. Use one of this machine's \
                 own addresses in 10.0.0.0/8, 172.16.0.0/12, or 192.168.0.0/16 (`ip addr` / \
                 `ifconfig`). Loopback is already the default, and this fixture must never be \
                 published to a public address."
            )));
        }
        Ok(ClientBind::Lan(ip))
    }

    /// A restored `client_host` from `state.json`. Anything that is not a private address — a
    /// hand-edited state file, or the loopback default — reads back as loopback.
    pub fn from_recorded(host: &str) -> Self {
        match host.parse::<Ipv4Addr>() {
            Ok(ip) if ip.is_private() => ClientBind::Lan(ip),
            _ => ClientBind::Loopback,
        }
    }
}

/// Which DATABASE topology `dev up` brings up.
///
/// This is the one thing every database-shaped decision in the CLI branches on — what to publish,
/// what to claim the operator on, what to report in `dev status`, and which topology variables the
/// child gateway is handed (or has actively unset).
///
/// The default is [`Sharded`](Topology::Sharded) (#11): a contributor's first `dev up` produces a
/// multi-database realm, because a topology that only production has is a topology nobody develops
/// against. [`Single`](Topology::Single) is `dev up --single`, and is the pre-#11 fixture unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// Four databases: the default world shard, a map-1 shard, the instance pool, and realm-core —
    /// one per production tier in core `docs/architecture.md` §3.1.
    ///
    /// The instance pool joined in #108, for the reason the region shard left: a tier that only
    /// production has is a tier nobody develops against, and instance routing was the one split a
    /// fresh clone could not exercise at all. It is a real fourth CONNECTION, unlike the retired
    /// region shard — the gateway routes map 36 to it from an ordinary shard-map rule, so
    /// `verify_topology` sees it connect rather than reporting a collapsed realm.
    Sharded,
    /// One database. Every topology variable is actively unset for the child, so a contributor with
    /// the production recipe exported still gets the fixture.
    Single,
}

impl Topology {
    /// What `dev up` picked, as recorded in `state.json` — so `status`, `logs`, `down` and
    /// `account create` all describe the stack that is actually running.
    ///
    /// Anything unrecognised — including the empty string a pre-#11 `state.json` has — reads back
    /// as the DEFAULT. A stale single-database state file then reports the other unpublished
    /// databases as unreachable, which is exactly the advice a contributor needs after the default
    /// changed under them; the one answer that would be wrong is "everything is fine".
    pub fn from_recorded(recorded: &str) -> Self {
        match recorded {
            "single" => Topology::Single,
            _ => Topology::Sharded,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Topology::Sharded => "sharded",
            Topology::Single => "single",
        }
    }

    /// Every database this fixture publishes, claims the operator on, and reports the health of —
    /// in the ORDER the gateway builds its own list (`ShardMap::databases`: the default database,
    /// then the databases shard-map rules name, then realm-core).
    ///
    /// ⚠ The default world shard being FIRST is load-bearing: a publish walks this list in order,
    /// so the database whose failure matters most is the one that fails first, and it is the
    /// `LYRACORE_DATABASE` every other lookup collapses to.
    pub fn databases(&self) -> Vec<&'static str> {
        let mut dbs = self.world_shards();
        dbs.extend(self.realm_core());
        dbs
    }

    /// The world shards — the databases that hold characters — in the gateway's own order: the
    /// default, then what a shard-map rule names.
    pub fn world_shards(&self) -> Vec<&'static str> {
        match self {
            Topology::Single => vec![ProjectLayout::DATABASE],
            Topology::Sharded => vec![
                ProjectLayout::DATABASE,
                ProjectLayout::KALIMDOR_SHARD,
                ProjectLayout::INSTANCE_POOL,
            ],
        }
    }

    /// The realm-core database, which owns accounts and sessions.
    ///
    /// MANDATORY in the sharded fixture rather than optional: a gateway serving more than one world
    /// shard with no realm-core is refused per shard (core `docs/design/guid-ranges.md`).
    pub fn realm_core(&self) -> Option<&'static str> {
        match self {
            Topology::Single => None,
            Topology::Sharded => Some(ProjectLayout::REALM_CORE),
        }
    }

    /// `LYRACORE_SHARD_MAP` — the (map, instance-bucket) → database rules. Two of them, and map 0 is
    /// in NEITHER: Eastern Kingdoms is one piece on the default database, which is what a `0:*=`
    /// rule could never express without handing a second database every map-0 location there is.
    ///
    /// Both rules are the same shape, `<map>:*=<db>`, because an instance map routes exactly like a
    /// continent one — the bucket half of a rule splits ONE map's instances across a pool of several
    /// databases, and a one-database pool needs no split (gateway `config.rs`, `instance_pool`). The
    /// `*` covers bucket 0, which for a dungeon map is the open-world instance id nothing ever
    /// stands in; the map-36 rows it routes are the spawn TEMPLATES `create_instance` copies.
    pub fn shard_map(&self) -> Option<String> {
        match self {
            Topology::Single => None,
            Topology::Sharded => Some(format!(
                "{}:*={}, {}:*={}",
                ProjectLayout::KALIMDOR_MAP,
                ProjectLayout::KALIMDOR_SHARD,
                ProjectLayout::INSTANCE_MAP,
                ProjectLayout::INSTANCE_POOL
            )),
        }
    }

    /// Hand a child process THIS topology and nothing else.
    ///
    /// The one place the four `LYRACORE_*` topology variables are decided, because every one of
    /// them fails SILENTLY when it is wrong and the failure looks identical from outside: a
    /// malformed shard-map rule is logged and DROPPED, an absent — or default-equal —
    /// `LYRACORE_REALM_CORE` reads as "unconfigured", an empty `LYRACORE_SHARD_MAP` still counts as
    /// set and suppresses `_FILE`. The result starts, binds, answers, and serves ONE database.
    ///
    /// So nothing is left to inheritance in either mode:
    ///
    /// - `Single` unsets all four, which is the pre-#11 fixture exactly — a contributor with the
    ///   production recipe exported still gets one database.
    /// - `Sharded` SETS the two it means and unsets the other two. `LYRACORE_SHARD_MAP_FILE` the
    ///   env var would win over anyway; leaving it inherited would only make a stale file look
    ///   load-bearing. `LYRACORE_REGION_SHARDS` names a topology this fixture no longer builds and
    ///   the gateway no longer reads — but an inherited one is still a string this CLI did not
    ///   decide, and the whole point of this function is that there are none of those.
    ///
    /// A variable is never both set and removed: `CommandSpec::build` applies the removals last, so
    /// that combination would silently unset it.
    pub fn apply_env(&self, cmd: CommandSpec) -> CommandSpec {
        let mut cmd = cmd
            .env_remove("LYRACORE_SHARD_MAP_FILE")
            .env_remove("LYRACORE_REGION_SHARDS");
        match self {
            Topology::Single => {
                for var in ProjectLayout::TOPOLOGY_VARS {
                    if !cmd.removes_env(var) {
                        cmd = cmd.env_remove(var);
                    }
                }
            }
            Topology::Sharded => {
                cmd = cmd
                    .env("LYRACORE_SHARD_MAP", self.shard_map().expect("sharded"))
                    .env("LYRACORE_REALM_CORE", self.realm_core().expect("sharded"));
            }
        }
        cmd
    }
}

/// A component the CLI can own a process for. The string form is also the log file stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Spacetime,
    Gateway,
}

impl Component {
    pub const ALL: [Component; 2] = [Component::Spacetime, Component::Gateway];

    pub fn as_str(&self) -> &'static str {
        match self {
            Component::Spacetime => "spacetime",
            Component::Gateway => "gateway",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Component::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The port whose answering means "this component is serving".
    pub fn health_port(&self) -> u16 {
        match self {
            Component::Spacetime => ProjectLayout::STDB_PORT,
            Component::Gateway => ProjectLayout::WORLD_PORT,
        }
    }

    /// The host that port must be probed on. The database is loopback in every mode; the gateway
    /// is wherever `--lan` bound it — probing 127.0.0.1 for a LAN-bound gateway reports a healthy
    /// process as permanently "starting".
    pub fn health_host(&self, bind: &ClientBind) -> String {
        match self {
            Component::Spacetime => std::net::Ipv4Addr::LOCALHOST.to_string(),
            Component::Gateway => bind.host(),
        }
    }
}

pub struct ProjectLayout {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl ProjectLayout {
    // ---- the #241 rename seam: internal names live here and nowhere else ----

    /// The DEFAULT WORLD SHARD: Eastern Kingdoms in one piece, and the only database the `--single`
    /// fixture has. It is `LYRACORE_DATABASE` in every topology, and it is first in every list of
    /// databases this CLI builds (see [`Topology::databases`]).
    pub const DATABASE: &'static str = "lyracore";
    /// The map-1 shard, reached by an ordinary `LYRACORE_SHARD_MAP` rule.
    pub const KALIMDOR_SHARD: &'static str = "lyracore-kalimdor";
    pub const KALIMDOR_MAP: u32 = 1;
    /// The INSTANCE POOL (#108): a one-database pool that owns every instance of every dungeon map,
    /// reached by an ordinary shard-map rule like the continent shard is.
    ///
    /// This name is shared with production, and deliberately — so is `lyracore-realm`. What keeps a
    /// fixture off a production node is the node it is published to (`-s local`, loopback:3000), not
    /// a name nobody can collide with.
    pub const INSTANCE_POOL: &'static str = "lyracore-instances";
    /// Deadmines — the one entry in the server's `DUNGEON_MAPS`, and the only instanced map the
    /// world import carries. Duplicated here rather than shared: this CLI does not depend on the
    /// server workspace. A dungeon map the server adds and this list misses routes to the default
    /// database instead, which is the pre-#108 behaviour and not a collapse.
    pub const INSTANCE_MAP: u32 = 36;
    /// Realm-core: accounts, sessions, and the character→shard index.
    pub const REALM_CORE: &'static str = "lyracore-realm";

    /// Every variable that decides a child's database topology — see [`Topology::apply_env`],
    /// which is the only thing allowed to act on them.
    ///
    /// ONE list. `cmd/dev.rs` and `cmd/account.rs` each used to carry their own copy, one as a
    /// constant and one as four inline `env_remove` calls, and that is exactly how they drifted:
    /// the gateway's fixture was hermetic and the provisioning child's was not.
    ///
    /// `LYRACORE_REGION_SHARDS` is still on the list although nothing sets it any more: the alpha
    /// topology reversal retired region sharding, but a variable this CLI stopped writing is
    /// exactly the one an old exported recipe can still smuggle in, so it stays something we
    /// actively unset rather than something we merely no longer mention.
    pub const TOPOLOGY_VARS: [&'static str; 4] = [
        "LYRACORE_SHARD_MAP",
        "LYRACORE_SHARD_MAP_FILE",
        "LYRACORE_REALM_CORE",
        "LYRACORE_REGION_SHARDS",
    ];

    /// The SpacetimeDB server alias. `publish-module.sh` pins `-s local`; we never re-select it.
    pub const STDB_SERVER: &'static str = "local";
    pub const GATEWAY_PACKAGE: &'static str = "lyracore-gateway";
    pub const GATEWAY_BIN: &'static str = "target/debug/lyracore-gateway";

    /// The SpacetimeDB module: the directory `spacetime publish -p` is pointed at, and the Cargo
    /// package `preflight` builds under the deploy feature set.
    pub const MODULE_DIR: &'static str = "module";
    pub const MODULE_PACKAGE: &'static str = "lyracore-module";
    /// The Cargo feature `publish` bakes in. `module/src/debug.rs` is only ever compiled by a
    /// publish (or by `preflight`), and a plain build omits it — which makes `publish` report a
    /// FALSE "Removed table" breaking change and abort.
    pub const DEPLOY_FEATURES: &'static str = "--features=debug_reducers";

    /// The workspace manifest and the toolchain file `preflight` reads the pinned versions out of.
    pub const RUST_TOOLCHAIN: &'static str = "rust-toolchain.toml";
    pub const SCRIPTS_DIR: &'static str = "scripts";

    /// The world-import tooling, which lives with the importer it belongs to rather than in the
    /// server repo's private `scripts/`. Since #104 `lyracore import world` drives the importer
    /// BINARY's modes itself and shells to only two of the scripts (the dump pull and the curated
    /// class-spell overlay) plus the manifest of assertion floors. `import-world.sh` itself is no
    /// longer driven — it stays in the checkout as the by-hand advanced path (several shards, a
    /// non-default box, a second continent) until the Rust flow has proven parity on a fresh
    /// provision, and this CLI deliberately does not address it.
    pub const IMPORTER_SCRIPTS_DIR: &'static str = "importer/scripts";
    pub const PULL_CLASSIC_DB_SCRIPT: &'static str = "importer/scripts/pull-classic-db.sh";
    pub const IMPORT_CLASS_SPELLS_SCRIPT: &'static str = "importer/scripts/import-class-spells.sh";
    /// The FLOOR_* count minimums the post-import assertions compare against. A DATA file the
    /// core repo tunes per dump — the CLI sources it fresh on every run rather than baking the
    /// numbers in, so retuning a floor never needs a CLI release.
    pub const IMPORT_MANIFEST_SCRIPT: &'static str = "importer/scripts/import-manifest.sh";
    /// The importer binary `import world` / `import vmaps` build and drive — the machinery stays
    /// in the core repo; this CLI only orders its modes.
    pub const IMPORTER_BIN: &'static str = "target/debug/lyracore-importer";

    /// The standalone supervisor artifact tracked in the checkout, installed by
    /// `update --install-service`. The service contract a host must end up with — descriptor
    /// limit, stderr destination, data directory, listen address — is read out of THIS file, so
    /// the CLI never carries a second copy of it that can drift.
    pub const STANDALONE_UNIT: &'static str = "deploy/systemd/spacetimedb-standalone.service";

    /// The pinned wire-harness release this checkout consumes (#246).
    pub const WIRE_HARNESS_PIN: &'static str = ".wire-harness-rev";
    /// Paths INSIDE a harness checkout. `dev smoke` drives the generic login smoke through the
    /// harness's own adapter seam; a release that also carries a suite entrypoint is preferred.
    pub const HARNESS_SMOKE_SEAM: &'static str = "adapters/lyracore/wire.sh";
    pub const HARNESS_SUITE_SCRIPT: &'static str = "adapters/lyracore/run-suite.sh";
    pub const HARNESS_CLIENT_BIN: &'static str = "vanilla-wire";
    /// The fixture account and character the login smoke signs in as.
    pub const SMOKE_ACCOUNT: &'static str = "TEST";
    pub const SMOKE_CHARACTER: &'static str = "Ginger";

    // The database is loopback in every mode. Only the two client-facing ports follow the
    // `ClientBind` chosen by `dev up [--lan IP]`.
    pub const STDB_PORT: u16 = 3000;
    pub const LOGON_PORT: u16 = 3724;
    pub const WORLD_PORT: u16 = 8085;

    pub fn stdb_listen() -> String {
        format!("127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn stdb_uri() -> String {
        format!("http://127.0.0.1:{}", Self::STDB_PORT)
    }
    pub fn logon_bind(bind: &ClientBind) -> String {
        format!("{}:{}", bind.host(), Self::LOGON_PORT)
    }
    pub fn world_bind(bind: &ClientBind) -> String {
        format!("{}:{}", bind.host(), Self::WORLD_PORT)
    }
    /// What the realm list must advertise, so a client that reached the logon tier over the LAN is
    /// sent to a world address it can also reach (the seeded `game_realm` row says loopback).
    pub fn realm_address(bind: &ClientBind) -> String {
        Self::world_bind(bind)
    }

    // ---- layout discovery ----

    /// Walk up from the current directory to the workspace root, so `lyracore` works from any
    /// subdirectory of a checkout.
    pub fn discover() -> Result<Self> {
        let start = std::env::current_dir()?;
        let mut dir = start.as_path();
        loop {
            if Self::is_workspace_root(dir) {
                return Self::from_root(dir);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => {
                    return Err(Error::ProjectLayout(format!(
                        "not inside a {} checkout (no workspace Cargo.toml above {})",
                        Self::DATABASE,
                        start.display()
                    )))
                }
            }
        }
    }

    fn is_workspace_root(dir: &Path) -> bool {
        std::fs::read_to_string(dir.join("Cargo.toml"))
            .map(|manifest| manifest.contains("[workspace]"))
            .unwrap_or(false)
    }

    pub fn from_root(root: &Path) -> Result<Self> {
        if !Self::is_workspace_root(root) {
            return Err(Error::ProjectLayout(format!(
                "{} is not a workspace root (its Cargo.toml has no [workspace])",
                root.display()
            )));
        }
        let state_dir = root.join(".lyracore");
        Ok(Self {
            root: root.to_path_buf(),
            logs_dir: state_dir.join("logs"),
            state_dir,
        })
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// The coordinator credential this CLI minted from the local node (#297), if it ever had to.
    ///
    /// Inside the git-ignored state directory, next to `state.json` — a credential that belongs to
    /// this checkout's fixture and to nothing else, and that must never become a commit.
    pub fn token_file(&self) -> PathBuf {
        self.state_dir.join("coordinator-token")
    }

    /// Persisted CLI config (`config.rs`) — currently just the 1.12.1 client's `Data/` directory,
    /// remembered so `lyracore import` and `lyracore doctor` stop asking for it every run.
    pub fn config_file(&self) -> PathBuf {
        self.state_dir.join("config.json")
    }

    pub fn log_file(&self, component: Component) -> PathBuf {
        self.logs_dir.join(format!("{}.log", component.as_str()))
    }

    pub fn gateway_bin(&self) -> PathBuf {
        self.root.join(Self::GATEWAY_BIN)
    }

    pub fn module_dir(&self) -> PathBuf {
        self.root.join(Self::MODULE_DIR)
    }

    pub fn module_manifest(&self) -> PathBuf {
        self.module_dir().join("Cargo.toml")
    }

    pub fn module_sources(&self) -> PathBuf {
        self.module_dir().join("src")
    }

    pub fn standalone_unit(&self) -> PathBuf {
        self.root.join(Self::STANDALONE_UNIT)
    }

    pub fn rust_toolchain_file(&self) -> PathBuf {
        self.root.join(Self::RUST_TOOLCHAIN)
    }

    pub fn scripts_dir(&self) -> PathBuf {
        self.root.join(Self::SCRIPTS_DIR)
    }

    pub fn importer_scripts_dir(&self) -> PathBuf {
        self.root.join(Self::IMPORTER_SCRIPTS_DIR)
    }

    pub fn pull_classic_db_script(&self) -> PathBuf {
        self.root.join(Self::PULL_CLASSIC_DB_SCRIPT)
    }

    pub fn import_class_spells_script(&self) -> PathBuf {
        self.root.join(Self::IMPORT_CLASS_SPELLS_SCRIPT)
    }

    pub fn import_manifest_script(&self) -> PathBuf {
        self.root.join(Self::IMPORT_MANIFEST_SCRIPT)
    }

    pub fn importer_bin(&self) -> PathBuf {
        self.root.join(Self::IMPORTER_BIN)
    }

    pub fn wire_harness_pin(&self) -> PathBuf {
        self.root.join(Self::WIRE_HARNESS_PIN)
    }

    /// Where pinned harness releases are cached — inside the git-ignored state directory, so a
    /// harness checkout can never appear in the server repo's `git status`.
    pub fn harness_cache(&self) -> PathBuf {
        self.state_dir.join("wire-harness")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.logs_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_directory_without_a_manifest_is_not_a_project() {
        let tmp = TempDir::new().unwrap();
        assert!(ProjectLayout::from_root(tmp.path()).is_err());
    }

    #[test]
    fn a_non_workspace_manifest_is_rejected() {
        // A bare `[package]` Cargo.toml is any old crate, not this checkout — the predecessor's
        // "does Cargo.toml exist" test accepted every Rust project on the machine.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(ProjectLayout::from_root(tmp.path()).is_err());
    }

    #[test]
    fn state_and_logs_hang_off_a_single_ignored_directory() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let layout = ProjectLayout::from_root(tmp.path()).unwrap();
        assert_eq!(layout.state_file(), tmp.path().join(".lyracore/state.json"));
        // The one credential on disk lives in the same ignored directory (#297).
        assert_eq!(
            layout.token_file(),
            tmp.path().join(".lyracore/coordinator-token")
        );
        assert_eq!(
            layout.log_file(Component::Gateway),
            tmp.path().join(".lyracore/logs/gateway.log")
        );
        assert_eq!(
            layout.config_file(),
            tmp.path().join(".lyracore/config.json")
        );
    }

    // ---- the `--lan` bind contract ----

    #[test]
    fn lan_accepts_the_three_private_ranges_only() {
        for ok in ["192.168.1.50", "10.0.0.7", "172.16.4.4", "172.31.255.254"] {
            assert!(
                ClientBind::parse_lan(ok).is_ok(),
                "{ok} is a private address"
            );
        }
        // A public address, the wildcard, loopback, a link-local, a hostname and an IPv6 literal.
        // `--lan 0.0.0.0` in particular is the one-character typo that would expose the fixture on
        // every interface, so it must be a usage error rather than a "well, it binds".
        for refused in [
            "8.8.8.8",
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "172.32.0.1",
            "my-desktop.local",
            "::1",
            "",
        ] {
            let error = ClientBind::parse_lan(refused).unwrap_err();
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_USAGE,
                "--lan {refused} must be a usage error"
            );
        }
    }

    #[test]
    fn only_the_client_facing_ports_follow_the_lan_address() {
        let lan = ClientBind::parse_lan("192.168.1.50").unwrap();
        assert_eq!(ProjectLayout::logon_bind(&lan), "192.168.1.50:3724");
        assert_eq!(ProjectLayout::world_bind(&lan), "192.168.1.50:8085");
        assert_eq!(ProjectLayout::realm_address(&lan), "192.168.1.50:8085");
        // The database never leaves loopback, whatever `--lan` says.
        assert_eq!(ProjectLayout::stdb_listen(), "127.0.0.1:3000");
        assert_eq!(ProjectLayout::stdb_uri(), "http://127.0.0.1:3000");
        assert_eq!(
            Component::Spacetime.health_host(&lan),
            "127.0.0.1",
            "the database is probed on loopback in every mode"
        );
        assert_eq!(Component::Gateway.health_host(&lan), "192.168.1.50");
    }

    #[test]
    fn the_default_bind_is_loopback_everywhere() {
        let bind = ClientBind::Loopback;
        assert_eq!(ProjectLayout::logon_bind(&bind), "127.0.0.1:3724");
        assert_eq!(ProjectLayout::world_bind(&bind), "127.0.0.1:8085");
    }

    #[test]
    fn a_recorded_host_round_trips_and_junk_falls_back_to_loopback() {
        let lan = ClientBind::parse_lan("10.1.2.3").unwrap();
        assert_eq!(ClientBind::from_recorded(&lan.host()), lan);
        for junk in ["", "127.0.0.1", "8.8.8.8", "not-an-address"] {
            assert_eq!(ClientBind::from_recorded(junk), ClientBind::Loopback);
        }
    }

    // ---- the topology contract (#11) ----

    #[test]
    fn the_default_world_shard_is_first_in_every_database_list() {
        // Not cosmetic: `LYRACORE_DATABASE` is what every lookup the shard map does not answer
        // collapses to, and a publish walks this list in order — so the database whose failure
        // matters most is the one that fails first.
        for topology in [Topology::Sharded, Topology::Single] {
            assert_eq!(topology.databases()[0], ProjectLayout::DATABASE);
            assert_eq!(topology.world_shards()[0], ProjectLayout::DATABASE);
        }
        // ...and the rest is the gateway's own construction order (`ShardMap::databases`: default,
        // rule targets, realm-core), so a per-database report reads in the same order as the
        // gateway log it is checked against.
        assert_eq!(
            Topology::Sharded.databases(),
            vec![
                "lyracore",
                "lyracore-kalimdor",
                "lyracore-instances",
                "lyracore-realm"
            ]
        );
        assert_eq!(Topology::Single.databases(), vec!["lyracore"]);
    }

    #[test]
    fn realm_core_is_last_because_it_is_not_a_world_shard() {
        // It owns accounts and sessions, never characters, so a list that treated it as a world
        // shard would be describing a topology the gateway refuses to build.
        let world = Topology::Sharded.world_shards();
        assert!(!world.contains(&ProjectLayout::REALM_CORE), "{world:?}");
        assert_eq!(
            Topology::Sharded.databases().last(),
            Some(&ProjectLayout::REALM_CORE)
        );
    }

    #[test]
    fn realm_core_is_mandatory_whenever_there_is_more_than_one_world_shard() {
        // A gateway serving >1 world shard with no realm-core is refused per shard, so "sharded
        // without realm-core" is not a topology this CLI may be able to express.
        assert!(Topology::Sharded.world_shards().len() > 1);
        assert_eq!(Topology::Sharded.realm_core(), Some("lyracore-realm"));
        assert_eq!(Topology::Single.world_shards().len(), 1);
        assert_eq!(Topology::Single.realm_core(), None);
    }

    /// Split a `LYRACORE_SHARD_MAP` string the way the gateway's `ShardMap::parse` does: rules
    /// separated by `,`, each `<map|*>:<bucket|*>=<db>`. Two tests need it, and both of them used to
    /// be one `split_once('=')` that silently stopped describing anything once the fixture grew its
    /// second rule (#108) — the whole string parsed as ONE rule targeting a database named
    /// `lyracore-kalimdor, 36:*=lyracore-instances`.
    fn parsed_rules(rules: &str) -> Vec<(&str, &str, &str)> {
        rules
            .split(',')
            .map(|rule| {
                let (selector, db) = rule
                    .trim()
                    .split_once('=')
                    .unwrap_or_else(|| panic!("a rule is `<map>:<bucket>=<db>`: {rule:?}"));
                let (map, bucket) = selector
                    .split_once(':')
                    .unwrap_or_else(|| panic!("a rule selector is `<map>:<bucket>`: {rule:?}"));
                (map, bucket, db)
            })
            .collect()
    }

    #[test]
    fn map_zero_is_never_named_by_a_shard_map_rule() {
        // Eastern Kingdoms stays whole on the default database. A `0:*=` rule would hand a second
        // database every map-0 location there is, which is the one thing the fixture's geography
        // cannot survive — and after the alpha topology reversal there is no second map-0 database
        // for it to name anyway.
        let rules = Topology::Sharded.shard_map().unwrap();
        assert_eq!(
            rules, "1:*=lyracore-kalimdor, 36:*=lyracore-instances",
            "map 1 to the continent shard, map 36 to the instance pool"
        );
        for (map, bucket, _) in parsed_rules(&rules) {
            assert_ne!(map, "0", "{rules}");
            // Every rule is map-level. A per-BUCKET rule splits one map's instances across a pool of
            // several databases; this fixture's pool is one database, so a bucket number here would
            // be routing that only ever resolves to the same answer with more ways to typo it.
            assert_eq!(bucket, "*", "{rules}");
        }
    }

    #[test]
    fn the_dungeon_map_is_routed_off_the_default_database() {
        // The point of the fourth database (#108): instance routing that a fresh clone can actually
        // exercise. Map 36 must name the instance pool — a fixture where it resolves to the default
        // shard is the pre-#108 one with an extra database published and unused.
        let rules = Topology::Sharded.shard_map().unwrap();
        let dungeon = parsed_rules(&rules)
            .into_iter()
            .find(|(map, _, _)| map.parse() == Ok(ProjectLayout::INSTANCE_MAP))
            .expect("no rule routes the dungeon map");
        assert_eq!(dungeon.2, ProjectLayout::INSTANCE_POOL);
        assert_ne!(dungeon.2, ProjectLayout::DATABASE);
    }

    #[test]
    fn the_single_topology_expresses_no_topology_at_all() {
        assert_eq!(Topology::Single.shard_map(), None);
        assert_eq!(Topology::Single.realm_core(), None);
    }

    // ---- the environment a child is handed ----

    #[test]
    fn the_single_topology_actively_unsets_all_four_variables() {
        // The pre-#11 fixture contract, unchanged: a contributor with the production recipe
        // exported must still get one database.
        let cmd = Topology::Single.apply_env(CommandSpec::new("gateway"));
        for var in ProjectLayout::TOPOLOGY_VARS {
            assert_eq!(cmd.env_value(var), None, "{var} must not be set");
            assert!(cmd.removes_env(var), "{var} must be actively unset");
        }
    }

    #[test]
    fn the_sharded_topology_sets_the_two_it_means_and_never_both_sets_and_removes_one() {
        // `CommandSpec::build` applies removals AFTER assignments, so a variable that is both set
        // and removed reaches the child unset — a silent collapse to one database with every
        // string exported exactly as intended.
        let cmd = Topology::Sharded.apply_env(CommandSpec::new("gateway"));
        assert_eq!(
            cmd.env_value("LYRACORE_SHARD_MAP"),
            Some("1:*=lyracore-kalimdor, 36:*=lyracore-instances")
        );
        assert_eq!(cmd.env_value("LYRACORE_REALM_CORE"), Some("lyracore-realm"));
        for var in ProjectLayout::TOPOLOGY_VARS {
            assert!(
                cmd.env_value(var).is_none() || !cmd.removes_env(var),
                "{var} is both set and removed, so the child sees it unset"
            );
        }
        // The two this mode does NOT set are unset rather than inherited. The env var wins over a
        // stale `LYRACORE_SHARD_MAP_FILE` anyway; `LYRACORE_REGION_SHARDS` names databases the
        // retired region topology had and this fixture never publishes, so an inherited one is a
        // realm the gateway would be asked to connect and the CLI would then report as collapsed.
        for var in ["LYRACORE_SHARD_MAP_FILE", "LYRACORE_REGION_SHARDS"] {
            assert!(cmd.removes_env(var), "{var} must be actively unset");
            assert_eq!(cmd.env_value(var), None);
        }
    }

    #[test]
    fn every_database_the_sharded_environment_names_is_one_the_fixture_publishes() {
        // The silent-collapse shape this catches: a typo in one of the exported strings names a
        // database nothing ever published. The gateway logs "shard X unreachable", falls back to
        // the default, and otherwise behaves exactly like a healthy realm.
        let cmd = Topology::Sharded.apply_env(CommandSpec::new("gateway"));
        let published = Topology::Sharded.databases();
        let named = cmd.env_value("LYRACORE_REALM_CORE").unwrap();
        assert!(
            published.contains(&named),
            "LYRACORE_REALM_CORE={named} is unpublished"
        );
        let rules = cmd.env_value("LYRACORE_SHARD_MAP").unwrap();
        // EVERY rule, not the first one: the fixture has had more than one since #108, and the
        // single `split_once('=')` this used to be would have read the whole string as ONE rule
        // pointing at a database called "lyracore-kalimdor, 36:*=lyracore-instances" — an assertion
        // that fails on a correct fixture and can never fail on a broken one.
        for (_, _, target) in parsed_rules(rules) {
            assert!(
                published.contains(&target),
                "{rules}: {target} is unpublished"
            );
        }
    }

    #[test]
    fn a_recorded_topology_round_trips_and_anything_else_is_the_default() {
        for topology in [Topology::Sharded, Topology::Single] {
            assert_eq!(Topology::from_recorded(topology.as_str()), topology);
        }
        // A pre-#11 `state.json` has no field at all. It must read as the new default, so the
        // other databases it never published are reported as unreachable rather than as fine.
        for junk in ["", "SINGLE", "three", "sharded "] {
            assert_eq!(Topology::from_recorded(junk), Topology::Sharded);
        }
    }

    #[test]
    fn components_round_trip_through_their_names() {
        for component in Component::ALL {
            assert_eq!(Component::parse(component.as_str()), Some(component));
        }
        assert_eq!(Component::parse("realm-core"), None);
    }
}
