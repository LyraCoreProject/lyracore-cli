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
/// realm with a live seam, because a seam that only production has is a seam nobody develops
/// against. [`Single`](Topology::Single) is `dev up --single`, and is the pre-#11 fixture unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// Four databases: the default world shard, a map-0 region shard, a map-1 shard, and
    /// realm-core.
    Sharded,
    /// One database. Every topology variable is actively unset for the child, so a contributor with
    /// the production recipe exported still gets the fixture.
    Single,
}

/// One `set_region_assignment` call the sharded fixture writes on realm-core: which database owns
/// a region of the seam menu, at which epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeamAssignment {
    pub map_id: u32,
    pub region_id: u32,
    pub shard: &'static str,
    pub epoch: u64,
}

impl SeamAssignment {
    /// `set_region_assignment(map_id, region_id, shard, epoch)`'s argument list, in the reducer's
    /// own order (`module/src/region.rs`), as the JSON array the node's `/call/` endpoint takes.
    ///
    /// `epoch` is rendered as a JSON NUMBER, which is safe only because the fixture's epochs are
    /// tiny: SpacetimeDB's JSON codec is `f64`-backed, and a `u64` past 2^53 loses precision. The
    /// fixture never writes one — but a caller that starts generating epochs from a timestamp
    /// would, and this is where it would have to change.
    pub fn call_arguments(&self) -> String {
        format!(
            "[{},{},{},{}]",
            self.map_id,
            self.region_id,
            json_string(self.shard),
            self.epoch
        )
    }
}

/// `import_map_regions(packed: String)`'s single argument, as the JSON array the node's `/call/`
/// endpoint takes: the seam menu file's bytes, escaped, and nothing else.
///
/// This CLI never parses, filters or reformats the menu. The rectangles are content data owned by
/// the server checkout (core #327); shipping anything but the file's own bytes would put a second,
/// silently-diverging copy of the realm's geometry in a repository that has no way to check it.
pub fn region_menu_arguments(packed: &str) -> String {
    format!("[{}]", json_string(packed))
}

/// A Rust string as a JSON string literal — quotes, escapes and all.
///
/// `serde_json` is already a dependency (`state.json`), so this is its `to_string` on a `&str` and
/// not a hand-rolled escaper: the one argument that goes through it is a whole file of prose
/// comments, quotes and newlines, and getting that wrong would send the module a truncated seam
/// menu that still parses.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}

impl Topology {
    /// What `dev up` picked, as recorded in `state.json` — so `status`, `logs`, `down` and
    /// `account create` all describe the stack that is actually running.
    ///
    /// Anything unrecognised — including the empty string a pre-#11 `state.json` has — reads back
    /// as the DEFAULT. A stale single-database state file then reports the three unpublished
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
    /// then the databases shard-map rules name, then the region shards, then realm-core).
    ///
    /// ⚠ The default world shard being FIRST is load-bearing, twice. The gateway reads the seam
    /// menu from the first entry of its world-shard list (`Coordinator::region_db_for` →
    /// `world_shards().first()`), so a region shard sorting ahead of `lyracore` would make it read
    /// an empty menu and switch region routing off — silently, with every database published,
    /// claimed and connected. And a publish walks this list in order, so the database whose failure
    /// matters most is the one that fails first.
    pub fn databases(&self) -> Vec<&'static str> {
        let mut dbs = self.world_shards();
        dbs.extend(self.realm_core());
        dbs
    }

    /// The world shards — the databases that hold characters — in the gateway's own order: the
    /// default, then what a shard-map rule names, then the region shards.
    pub fn world_shards(&self) -> Vec<&'static str> {
        match self {
            Topology::Single => vec![ProjectLayout::DATABASE],
            Topology::Sharded => vec![
                ProjectLayout::DATABASE,
                ProjectLayout::KALIMDOR_SHARD,
                ProjectLayout::REGION_SHARD,
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

    /// `LYRACORE_SHARD_MAP` — the (map, instance-bucket) → database rules. Kalimdor only: the
    /// map-0 seam is expressed as regions, which is precisely the decision a shard-map rule cannot
    /// take (a `0:*=` rule would hand the region shard every map-0 location the menu does NOT
    /// cover — all of Eastern Kingdoms, on a database holding none of it).
    pub fn shard_map(&self) -> Option<String> {
        match self {
            Topology::Single => None,
            Topology::Sharded => Some(format!(
                "{}:*={}",
                ProjectLayout::KALIMDOR_MAP,
                ProjectLayout::KALIMDOR_SHARD
            )),
        }
    }

    /// `LYRACORE_REGION_SHARDS` — world shards a REGION assignment may name, connected but never
    /// routed to by the shard map itself. The only way to say "connect to this database, and let
    /// the region overlay decide who goes there".
    pub fn region_shards(&self) -> Option<&'static str> {
        match self {
            Topology::Single => None,
            Topology::Sharded => Some(ProjectLayout::REGION_SHARD),
        }
    }

    /// Which shards the seam menu is imported on: every WORLD shard a region of the menu can be
    /// assigned to, the first of which is also where the gateway reads `game_map_region` from
    /// (`Coordinator::region_db_for`).
    ///
    /// The map-1 shard is deliberately not one. The fixture menu draws map-0 regions only, so a
    /// database that can never own one of them has nothing to resolve, and the import is a
    /// replace-the-whole-menu write — a needless one on a database is a needless way to fail.
    pub fn region_menu_shards(&self) -> Vec<&'static str> {
        match self {
            Topology::Single => Vec::new(),
            Topology::Sharded => vec![ProjectLayout::DATABASE, ProjectLayout::REGION_SHARD],
        }
    }

    /// The seam the fixture activates (core #327): region `0:1` is Northshire Valley and STAYS on
    /// the default database, so a fresh character — who spawns there — never begins with a handoff;
    /// region `0:2` is the rest of Elwynn and moves to the region shard, so walking the road out of
    /// the valley is a real shard crossing.
    ///
    /// Both at epoch 1. The first assignment for a region accepts any epoch, and a re-run writes
    /// the same epoch again — which the module refuses as a stale retry, by design (an equal epoch
    /// is a race, not a flip). `DevManager::assign_regions` reads that refusal as "already wired".
    pub fn seam_assignments(&self) -> Vec<SeamAssignment> {
        match self {
            Topology::Single => Vec::new(),
            Topology::Sharded => vec![
                SeamAssignment {
                    map_id: ProjectLayout::SEAM_MAP,
                    region_id: ProjectLayout::NORTHSHIRE_REGION,
                    shard: ProjectLayout::DATABASE,
                    epoch: 1,
                },
                SeamAssignment {
                    map_id: ProjectLayout::SEAM_MAP,
                    region_id: ProjectLayout::ELWYNN_REGION,
                    shard: ProjectLayout::REGION_SHARD,
                    epoch: 1,
                },
            ],
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
    /// - `Sharded` SETS the three it means and unsets `LYRACORE_SHARD_MAP_FILE`, which the env var
    ///   would win over anyway; leaving it inherited would only make a stale file look load-bearing.
    ///
    /// A variable is never both set and removed: `CommandSpec::build` applies the removals last, so
    /// that combination would silently unset it.
    pub fn apply_env(&self, cmd: CommandSpec) -> CommandSpec {
        let mut cmd = cmd.env_remove("LYRACORE_SHARD_MAP_FILE");
        match self {
            Topology::Single => {
                for var in ProjectLayout::TOPOLOGY_VARS {
                    if var != "LYRACORE_SHARD_MAP_FILE" {
                        cmd = cmd.env_remove(var);
                    }
                }
            }
            Topology::Sharded => {
                cmd = cmd
                    .env("LYRACORE_SHARD_MAP", self.shard_map().expect("sharded"))
                    .env("LYRACORE_REALM_CORE", self.realm_core().expect("sharded"))
                    .env(
                        "LYRACORE_REGION_SHARDS",
                        self.region_shards().expect("sharded"),
                    );
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

    /// The DEFAULT WORLD SHARD: Eastern Kingdoms, the database that holds the seam menu, and the
    /// only database the `--single` fixture has. It is `LYRACORE_DATABASE` in every topology, and
    /// it is first in every list of databases this CLI builds (see [`Topology::databases`]).
    pub const DATABASE: &'static str = "lyracore";
    /// The map-0 REGION shard: the far side of the seam, holding everything in Elwynn outside
    /// Northshire Valley. Wired only through `LYRACORE_REGION_SHARDS` and a region assignment —
    /// never a shard-map rule.
    pub const REGION_SHARD: &'static str = "lyracore-elwynn";
    /// The map-1 shard, reached by an ordinary `LYRACORE_SHARD_MAP` rule.
    pub const KALIMDOR_SHARD: &'static str = "lyracore-kalimdor";
    pub const KALIMDOR_MAP: u32 = 1;
    /// Realm-core: accounts, sessions, the character→shard index, and the region assignments.
    pub const REALM_CORE: &'static str = "lyracore-realm";

    /// The seam the fixture draws, as `content/regions/fixture.regions` defines it: two map-0
    /// regions, one per world shard. The geometry itself is NOT here and must never be — the
    /// rectangles are content data belonging to the server checkout, and this CLI only ever ships
    /// that file's bytes to `import_map_regions`.
    pub const SEAM_MAP: u32 = 0;
    /// Northshire Valley — where a new character spawns, and where the fixture seeds its whole
    /// spatial content. Stays on [`DATABASE`](Self::DATABASE).
    pub const NORTHSHIRE_REGION: u32 = 1;
    /// The rest of Elwynn. Moves to [`REGION_SHARD`](Self::REGION_SHARD).
    pub const ELWYNN_REGION: u32 = 2;

    /// The seam menu the sharded fixture imports, relative to the checkout root. Content data that
    /// belongs to the server repo (core #327), so a checkout older than this CLI simply does not
    /// have it — which is a skip with a message, never a half-wired realm.
    pub const REGION_FIXTURE: &'static str = "content/regions/fixture.regions";

    /// Every variable that decides a child's database topology — see [`Topology::apply_env`],
    /// which is the only thing allowed to act on them.
    ///
    /// ONE list. `cmd/dev.rs` and `cmd/account.rs` each used to carry their own copy, one as a
    /// constant and one as four inline `env_remove` calls, and that is exactly how they drifted:
    /// the gateway's fixture was hermetic and the provisioning child's was not.
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

    /// The world-import ETL, which lives with the importer it drives rather than in the server
    /// repo's private `scripts/`. These four are the shipped, documented tooling `lyracore import`
    /// is a façade over — see the header of each one for what it does and what it needs.
    pub const IMPORTER_SCRIPTS_DIR: &'static str = "importer/scripts";
    pub const PULL_CLASSIC_DB_SCRIPT: &'static str = "importer/scripts/pull-classic-db.sh";
    pub const IMPORT_WORLD_SCRIPT: &'static str = "importer/scripts/import-world.sh";
    pub const IMPORT_CLASS_SPELLS_SCRIPT: &'static str = "importer/scripts/import-class-spells.sh";

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

    pub fn import_world_script(&self) -> PathBuf {
        self.root.join(Self::IMPORT_WORLD_SCRIPT)
    }

    pub fn import_class_spells_script(&self) -> PathBuf {
        self.root.join(Self::IMPORT_CLASS_SPELLS_SCRIPT)
    }

    pub fn wire_harness_pin(&self) -> PathBuf {
        self.root.join(Self::WIRE_HARNESS_PIN)
    }

    /// The checkout's seam menu, if it ships one. `None` is version skew (a core checkout older
    /// than this CLI), not an error.
    pub fn region_fixture(&self) -> Option<PathBuf> {
        let path = self.root.join(Self::REGION_FIXTURE);
        path.is_file().then_some(path)
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
        // Not cosmetic: the gateway reads the seam menu from the FIRST world shard in its own list
        // (`Coordinator::region_db_for`). A region shard sorting ahead of `lyracore` would make the
        // gateway read an empty menu and switch region routing off — silently, with all four
        // databases published, claimed and connected.
        for topology in [Topology::Sharded, Topology::Single] {
            assert_eq!(topology.databases()[0], ProjectLayout::DATABASE);
            assert_eq!(topology.world_shards()[0], ProjectLayout::DATABASE);
        }
        // ...and the rest is the gateway's own construction order (`ShardMap::databases`: default,
        // rule targets, region shards, realm-core), so a per-database report reads in the same
        // order as the gateway log it is checked against.
        assert_eq!(
            Topology::Sharded.databases(),
            vec![
                "lyracore",
                "lyracore-kalimdor",
                "lyracore-elwynn",
                "lyracore-realm"
            ]
        );
        assert_eq!(Topology::Single.databases(), vec!["lyracore"]);
    }

    #[test]
    fn realm_core_is_last_because_it_is_not_a_world_shard() {
        // It owns accounts and sessions, never characters. The gateway drops it from
        // `LYRACORE_REGION_SHARDS` for exactly that reason, so a list that treated it as a world
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

    #[test]
    fn the_region_shard_is_never_named_by_a_shard_map_rule() {
        // `0:*=lyracore-elwynn` would hand the region shard every map-0 location the seam menu does
        // NOT cover — all of Eastern Kingdoms, on a database holding none of it. The region overlay
        // is its only way in, and `LYRACORE_REGION_SHARDS` is the only var that says so.
        let rules = Topology::Sharded.shard_map().unwrap();
        assert_eq!(rules, "1:*=lyracore-kalimdor");
        assert!(!rules.contains(ProjectLayout::REGION_SHARD), "{rules}");
        assert_eq!(
            Topology::Sharded.region_shards(),
            Some(ProjectLayout::REGION_SHARD)
        );
    }

    #[test]
    fn the_single_topology_expresses_no_topology_at_all() {
        assert_eq!(Topology::Single.shard_map(), None);
        assert_eq!(Topology::Single.region_shards(), None);
        assert_eq!(Topology::Single.realm_core(), None);
        assert!(Topology::Single.region_menu_shards().is_empty());
        assert!(Topology::Single.seam_assignments().is_empty());
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
    fn the_sharded_topology_sets_the_three_it_means_and_never_both_sets_and_removes_one() {
        // `CommandSpec::build` applies removals AFTER assignments, so a variable that is both set
        // and removed reaches the child unset — a silent collapse to one database with every
        // string exported exactly as intended.
        let cmd = Topology::Sharded.apply_env(CommandSpec::new("gateway"));
        assert_eq!(
            cmd.env_value("LYRACORE_SHARD_MAP"),
            Some("1:*=lyracore-kalimdor")
        );
        assert_eq!(cmd.env_value("LYRACORE_REALM_CORE"), Some("lyracore-realm"));
        assert_eq!(
            cmd.env_value("LYRACORE_REGION_SHARDS"),
            Some("lyracore-elwynn")
        );
        for var in ProjectLayout::TOPOLOGY_VARS {
            assert!(
                cmd.env_value(var).is_none() || !cmd.removes_env(var),
                "{var} is both set and removed, so the child sees it unset"
            );
        }
        // ...and the file form is unset in BOTH modes: the env var wins over it anyway, so leaving
        // a stale `LYRACORE_SHARD_MAP_FILE` inherited only makes it look load-bearing.
        assert!(cmd.removes_env("LYRACORE_SHARD_MAP_FILE"));
        assert_eq!(cmd.env_value("LYRACORE_SHARD_MAP_FILE"), None);
    }

    #[test]
    fn every_database_the_sharded_environment_names_is_one_the_fixture_publishes() {
        // The silent-collapse shape this catches: a typo in one of the exported strings names a
        // database nothing ever published. The gateway logs "shard X unreachable", falls back to
        // the default, and otherwise behaves exactly like a healthy realm.
        let cmd = Topology::Sharded.apply_env(CommandSpec::new("gateway"));
        let published = Topology::Sharded.databases();
        for var in ["LYRACORE_REALM_CORE", "LYRACORE_REGION_SHARDS"] {
            let named = cmd.env_value(var).unwrap();
            assert!(published.contains(&named), "{var}={named} is unpublished");
        }
        let rule = cmd.env_value("LYRACORE_SHARD_MAP").unwrap();
        let (_, target) = rule
            .split_once('=')
            .expect("a rule is `<map>:<bucket>=<db>`");
        assert!(
            published.contains(&target),
            "{rule} targets an unpublished db"
        );
    }

    #[test]
    fn the_seam_menu_lands_on_every_shard_a_region_can_route_to() {
        // The gateway reads the menu from the first world shard; an assignment may name any of
        // them. Both assignment targets must therefore be shards the menu was imported on.
        let menu = Topology::Sharded.region_menu_shards();
        assert_eq!(menu[0], ProjectLayout::DATABASE, "{menu:?}");
        for assignment in Topology::Sharded.seam_assignments() {
            assert!(
                menu.contains(&assignment.shard),
                "region {} routes to {}, which has no seam menu: {menu:?}",
                assignment.region_id,
                assignment.shard
            );
        }
    }

    #[test]
    fn the_two_seam_assignments_split_map_zero_between_the_two_world_shards() {
        let assignments = Topology::Sharded.seam_assignments();
        assert_eq!(assignments.len(), 2);
        assert!(assignments.iter().all(|a| a.map_id == 0 && a.epoch == 1));
        // Region 1 is Northshire Valley, where a new character spawns — it STAYS on the default
        // database, so a fresh login never begins with a handoff (core #327).
        assert_eq!(assignments[0].region_id, 1);
        assert_eq!(assignments[0].shard, ProjectLayout::DATABASE);
        assert_eq!(assignments[1].region_id, 2);
        assert_eq!(assignments[1].shard, "lyracore-elwynn");
        // Region 0 is "the rest of the map" and is never assignable — the module refuses it.
        assert!(assignments.iter().all(|a| a.region_id != 0));
    }

    #[test]
    fn an_assignment_renders_the_reducers_four_arguments_in_its_own_order() {
        // `set_region_assignment(map_id: u32, region_id: u32, shard: String, epoch: u64)`. Swapping
        // the two u32s writes a real row for a region that does not exist, which routes nobody and
        // reports nothing.
        let assignment = Topology::Sharded.seam_assignments()[1];
        assert_eq!(assignment.call_arguments(), r#"[0,2,"lyracore-elwynn",1]"#);
    }

    #[test]
    fn a_seam_menu_argument_is_json_escaped_rather_than_concatenated() {
        // The menu is a whole file of prose comments, quotes and newlines. Splicing it into a JSON
        // array unescaped would send the module a truncated menu — and a truncated menu still
        // parses, because `#` comments and blank lines are skipped.
        assert_eq!(json_string("a \"quoted\" line\nand another"), {
            let expected: &str = "\"a \\\"quoted\\\" line\\nand another\"";
            expected.to_string()
        });
    }

    #[test]
    fn a_recorded_topology_round_trips_and_anything_else_is_the_default() {
        for topology in [Topology::Sharded, Topology::Single] {
            assert_eq!(Topology::from_recorded(topology.as_str()), topology);
        }
        // A pre-#11 `state.json` has no field at all. It must read as the new default, so the
        // three databases it never published are reported as unreachable rather than as fine.
        for junk in ["", "SINGLE", "four", "sharded "] {
            assert_eq!(Topology::from_recorded(junk), Topology::Sharded);
        }
    }

    #[test]
    fn a_checkout_without_a_seam_menu_reports_no_fixture_rather_than_a_path() {
        // Version skew: a core checkout older than this CLI ships no `content/regions/`. That is a
        // skip with a message, not a half-wired realm and not a crash.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let layout = ProjectLayout::from_root(tmp.path()).unwrap();
        assert_eq!(layout.region_fixture(), None);

        let fixture = tmp.path().join(ProjectLayout::REGION_FIXTURE);
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        std::fs::write(&fixture, "0:1 = 512..524, 336..350\n").unwrap();
        assert_eq!(layout.region_fixture(), Some(fixture));
    }

    #[test]
    fn components_round_trip_through_their_names() {
        for component in Component::ALL {
            assert_eq!(Component::parse(component.as_str()), Some(component));
        }
        assert_eq!(Component::parse("realm-core"), None);
    }
}
