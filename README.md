<p align="center"><img src="https://raw.githubusercontent.com/LyraCoreProject/LyraCore/refs/heads/main/lyracore-icon-light.svg" alt="LyraCore" width="120"></p>

# lyracore-cli

The CLI for [LyraCore](https://github.com/LyraCoreProject/LyraCore). Start a local realm, import
world data, manage Packages, prepare client content, and provision Accounts.

It also provides read-only production status and an explicit command to reconcile the standalone
SpacetimeDB system service. It does not install Rust or SpacetimeDB or manage backups. The system
service changes only when you run
[`service reconcile`](#service-reconcile--make-the-host-match-the-tracked-unit).

## Use from a LyraCore checkout

Follow the [LyraCore quickstart](https://github.com/LyraCoreProject/LyraCore/blob/main/docs/quickstart.md)
to install the prerequisites and clone Core. Its `./lyracore` launcher installs and runs the CLI
commit pinned in `.lyracore-cli-rev`. You do not need to clone this CLI repository to use it.

From the Core checkout root:

```bash
./lyracore doctor
./lyracore dev up
./lyracore account create TEST
./lyracore dev status
```

The global `lyracore` launcher supplied by Core's installer also works from a checkout subdirectory.
The commands below use that spelling. Use `./lyracore` from the checkout root if you skipped the
installer.

## Commands

[`docs/commands.md`](docs/commands.md) is the full reference for every command below, the
`import`, `config`, `client` and `packages` families included.

`lyracore help` shows the short getting-started list. `lyracore help --all` shows the full help.
In the reference below, square brackets mark optional arguments and `...` means an argument can
repeat. Commands run against the selected Core checkout.

### Local realm and publishing

```text
lyracore help [--all]
lyracore doctor
lyracore preflight
lyracore publish [DATABASE ...] [--skip-preflight]
lyracore dev up [--single] [--lan <IP>]
lyracore dev status
lyracore dev logs [spacetime|gateway]
lyracore dev smoke
lyracore dev down [--forget]
lyracore account create USER [--password-stdin]
lyracore character gm NAME true|false
lyracore update
```

### World data and client content

```text
lyracore config
lyracore config set client-data PATH
lyracore import [--client-data PATH] [--accept] [--profile-shard PROFILE=SHARD ...]
lyracore import world [--client-data PATH] [--accept] [--profile-shard PROFILE=SHARD ...]
lyracore import vmaps [--client-data PATH] [--profile-shard PROFILE=SHARD ...]
lyracore client sync
lyracore client pack --out DIR [--zip]
```

Repeat `--profile-shard` for each assignment. `import` and `import world` are the same command.

### Packages

```text
lyracore packages list
lyracore packages new NAME
lyracore packages add FOLDER|GIT-URL|NAME [--yes]
lyracore packages update [NAME] [--yes]
lyracore packages enable NAME
lyracore packages disable NAME [--yes]
lyracore packages remove NAME [--yes]
lyracore packages build
lyracore packages check
lyracore packages replay [DATABASE ...] [--check] [--yes] [--force-all] [--client-data PATH]
lyracore packages config NAME [KEY [VALUE]] [--new]
```

### Production inspection and host service

```text
lyracore production status --server URI --gateway-log PATH --realm-core DB DATABASE ...
lyracore service reconcile
```

`production status` requires the complete Shard list, including the Shard named by `--realm-core`.
`service reconcile` requires root and restarts SpacetimeDB. Read its section before running it.

## Import world data

```bash
lyracore config set client-data /path/to/WoW-1.12.1/Data
lyracore config
lyracore import
```

`config set client-data` checks and saves the path in `.lyracore/config.json`. `import` uses an
explicit `--client-data PATH` first, then the saved path, then asks for one.

With the realm running, `import` fetches the pinned cMaNGOS `classic-db` world data and combines
it with your own 1.12.1 client's archives. It imports the supported Alliance early-game areas and
Deadmines, including world content, DBCs, terrain, and navigation. It replaces the seeded content
in the affected Import Families. You do not need a running MaNGOS installation.

The command asks for consent on every run. For a scripted run:

```bash
lyracore import world --client-data /path/to/WoW-1.12.1/Data --accept
```

The default plan follows the recorded fixture topology. A sharded realm uses `alliance-eastern`,
`alliance-kalimdor`, and `instances`; a single-Shard realm uses `alliance-single`. To name the
destinations explicitly, supply all three sharded World Import Profile assignments:

```bash
lyracore import world \
  --profile-shard alliance-eastern=lyracore \
  --profile-shard alliance-kalimdor=lyracore-kalimdor \
  --profile-shard instances=lyracore-instances
```

Each assignment must name a distinct Shard. `starting-eastern` and `starting-kalimdor` can replace
their `alliance-*` counterparts to include the Horde starting areas. These assignments select
Shard names on the local SpacetimeDB node; they do not select a remote host.

The import checks each destination against its World Import Profile. World profiles require
skinning content. The `instances` profile allows no skinnable creatures, as in Deadmines, but
requires loot for every referenced skinning table.

`lyracore import vmaps` imports exact model and WMO collision geometry into populated World Shards.
It accepts the same client path and profile assignments, skips the Instance Pool, and does not
enable exact collision checks. It reads your own archives without fetching game data, so it has
no `--accept` option.

## Manage Packages

| Command | What it does |
| --- | --- |
| `packages list` | Show enabled and disabled Packages, their Package Sources, Content Identities, local changes, and registrations. |
| `packages new NAME` | Create a Package from Core's `packages/example/` Reference Package and run preflight. |
| `packages add FOLDER\|GIT-URL\|NAME` | Print a Trust Review, ask for confirmation, copy the Package into `packages/`, record its Package Source, and run preflight. |
| `packages update [NAME]` | Update one Git-backed Package or all of them, including Official Package Sources. Refuse local changes, review the candidate, and restore the previous copy if preflight fails. |
| `packages enable NAME` | Move a disabled Package back into `packages/`. |
| `packages disable NAME` | Run Package Teardown when a dev node is recorded, then move the folder into `.lyracore/packages-disabled/`. |
| `packages remove NAME` | Delete a disabled Package after confirmation. Refuse changes to its recorded Content Identity. |
| `packages build` | Refresh typings, install pinned Bun dependencies, typecheck and run Datascripts, compile Runtime Scripts, and check the generated artifacts together. |
| `packages check` | Check Package Deltas and Script Artifacts against their inputs, refreshing typings when needed. Preflight also runs this check. |
| `packages replay` | Apply enabled Packages' spell Package Deltas and reconcile Runtime Scripts on each target Shard. |
| `packages config NAME [KEY [VALUE]]` | List Package Config, read one key, or write one key on every recorded fixture Shard. |

A bare name such as `playerbots` selects a Package from the
[Official Package Collection](https://github.com/LyraCoreProject/packages). A Git Package Source
must contain one Package at its root. Use `./my-package` to select a local folder explicitly.

```bash
lyracore packages add playerbots
lyracore packages list
lyracore packages replay --check
```

Adding, updating, enabling, or disabling a Package does not publish the Module or sync a client.
Follow the next steps the command prints. Package Teardown deletes the Package's durable rows
before disabling it on a recorded dev node, so the command asks first. Without a recorded node it
moves the folder and reports any tables that still need teardown before publishing.

`packages replay` uses the recorded fixture Shards unless you name destinations. `--check` prints
the plan without changing Shards. Replays skip Import Families whose recorded inputs already
match, so a retry resumes unfinished work; `--force-all` reapplies matching families too.
`--client-data PATH` supplies the client archives needed for the spell family. An empty enabled
Package set also reconciles state, removing Runtime Scripts and spell changes from disabled
Packages.

`packages config NAME KEY VALUE` changes a key the Package already seeded. Add `--new` to create
a key. Reads report disagreements between Shards. Use `lyracore config` for the client path;
Package Config holds the values a Package reads at runtime.

Use `--yes` to answer the confirmation for `add`, `update`, `disable`, `remove`, or `replay` in
advance. It does not bypass their checks. Bun is needed for authoring with `packages build`;
applying a prebuilt Package does not require it.

`packages build` records a Build Identity for each artifact it generates. Datascripts need a Base
Snapshot from your client data; if it is missing, the command prints how to create it.
`packages check` reports a missing Base Snapshot as unverifiable. Source-free prebuilt Script
Artifacts still pass the Rust artifact checks without local authoring inputs.

## Prepare client content

```bash
lyracore client sync
lyracore client pack --out ./client-artifact --zip
```

`client sync` requires the saved client-data path. It builds `Data/patch-3.MPQ`, installs enabled
Packages' addons into `Interface/AddOns/`, and clears the client's `WDB/` cache. Restart the
client for MPQ changes or use `/reload` for addons. Addons left by a disabled or removed Package
are reported when detected and remain on disk.

`client pack` builds a separate Client Artifact from authored content in `client-patch/` and
enabled Packages. It leaves the configured client alone and includes no base game assets.
`--out` is relative to the Core checkout when it is not absolute. `--zip` also creates `DIR.zip`
and requires the `zip` command. A nonempty output directory must be an earlier Client Artifact
created by this command before it can be replaced.

## Grant Character GM access

```bash
lyracore character gm Tester true
lyracore character gm Tester false
```

The command searches the fixture's World Shards for the named Character and uses the coordinator
credential to set GM level 3 for `true` or level 0 for `false`.

## Update Core

```bash
lyracore update
```

This fetches `origin`, refuses tracked local changes, and resets the Core checkout to `origin/main`.
It does not restart the realm. Review the reported change, then restart a local fixture with
`lyracore dev down` followed by `lyracore dev up` to rebuild and republish. If the CLI pin changed,
the Core launcher installs it on the next invocation.

Runtime state lives in the target checkout's git-ignored `.lyracore/`: `state.json` records the
processes the CLI started, `logs/` holds their output, and `config.json` holds the client path.
When the CLI mints a local coordinator identity, `coordinator-token` stores its credential with
mode `0600`.

## `preflight` — the offline deploy gate

```bash
lyracore preflight
```

Preflight checks the build and schema without contacting a SpacetimeDB node. It runs six checks:

| # | Check | The break it catches |
| --- | --- | --- |
| 0 | `rustc` and the `spacetime` CLI **exactly** match the versions the checkout pins (`rust-toolchain.toml`, `module/Cargo.toml`) | tools drifting out from under the pin — a CLI ahead of it publishes a schema the repo never tested against |
| 1 | the module compiles with `--features=debug_reducers` | code that only a publish compiles, so the default test config never sees it |
| 2 | real, offline wasm schema extraction (`spacetime generate` into a scratch directory) | a `#[default(0)]` on a `u64`, which SpacetimeDB rejects at migration time and nothing in-tree validates |
| 3 | every `#[client_visibility_filter]` names real tables and columns | a filter stored as raw text at publish, rejecting a gateway **subscription** at login time |
| 4 | a script with a configurable `DB` target threads it into every tool it drives | an ETL writing to one database and asserting against another |
| 5 | Package Delta and Script Artifact inputs match their Build Identities | stale Package content reaching a Shard |

Every check runs even after one fails, so a run hands back every problem rather than one per
attempt. Check 0 is an EXACT match, unlike `doctor`'s minimum-version floor: newer is not fine when
a publish is the thing being gated. Where `spacetimedb-standalone` is unavailable,
`PREFLIGHT_SKIP_SCHEMA=1` skips check 2, records a blocking failure, and continues the independent
checks. The bypass can collect the remaining evidence but cannot approve a publish because nothing
validated your `#[default]` encodings.
The SpacetimeDB CLI itself is mandatory: if it is absent, preflight still runs the independent
checks but exits nonzero and refuses to call the gate complete.

## `production status` — explicit, read-only topology verification

```bash
lyracore production status --server http://127.0.0.1:3000 --gateway-log /tmp/gw.log \
  --realm-core lyracore-realm \
  lyracore lyracore-world-1 lyracore-instances lyracore-realm
```

Production status requires the SpacetimeDB server nickname, host, or URL and never substitutes
`local` or the contributor fixture's database names. It distinguishes databases missing from
`spacetime list -s <server>` from published schemas that cannot be reached,
then reads only the latest gateway-start segment and reports the configured set, one coordinator
connection per database, coordinator credential warnings, realm-core activation, listeners,
startup errors, realm-address mismatch, and missing writer-occupancy metrics. Warnings leave the
exit code at zero but the summary says `WARNINGS`, never `HEALTHY`. It is strictly observational:
no publish, reducer, SQL write, signal, or restart is available through it.

## `publish` — the one correct deploy

```bash
lyracore publish                                   # every database in the active fixture topology
lyracore publish lyracore lyracore-world-1 realm-core
```

Runs `preflight`, then `spacetime publish -s local -p <checkout>/module
--build-options=--features=debug_reducers --yes <DATABASE>` for each database in turn, stopping at
the first failure. With no names, it publishes the active fixture's recorded topology; with no
active fixture, it uses the default sharded topology that the next `dev up` would provision.
Explicit database **NAMES** are published exactly as given:

* `--features=debug_reducers` is baked in — a plain build omits the debug module, so publish reports
  a FALSE "Removed table" breaking change and aborts;
* `--yes` is baked in — SpacetimeDB prompts for ANY schema change, even an additive END-appended
  `#[default]` column, and a non-interactive stdin turns that prompt into an EOF abort;
* `-c` / `--delete-data` — the destructive wipe — is **refused**, as is any other flag-shaped
  argument, with exit 2 and before a single `spacetime` process starts. Nothing is forwarded.

`--skip-preflight` is the only recognised flag, and it says on stdout that nothing validated the
schema. Publishing several databases in one command is what makes "every shard" checkable instead of
remembered: a schema change needs all of them, and the gateway reports a shard left behind only as
"realm-core unreachable — LOGONS WILL BE REFUSED".

## What `dev up` does

`dev up` brings up a **sharded** realm by default — four databases, one per production tier, so the
topology a contributor develops against is the one production has rather than a single-database
simplification:

| database | role | wired by |
| --- | --- | --- |
| `lyracore` | default world shard: map 0, Eastern Kingdoms | `LYRACORE_DATABASE` |
| `lyracore-kalimdor` | map 1 | `LYRACORE_SHARD_MAP="1:*=lyracore-kalimdor"` |
| `lyracore-instances` | instance pool: every dungeon run (map 36, Deadmines) | `LYRACORE_SHARD_MAP="36:*=lyracore-instances"` |
| `lyracore-realm` | realm-core: accounts, sessions, the character→shard index | `LYRACORE_REALM_CORE` |

Realm-core is **mandatory**, not optional: a gateway serving more than one world shard with no
realm-core refuses to serve them. And `lyracore` is first in every list this CLI builds, because it
is what every lookup no shard-map rule answers collapses to, and because a publish walks the list in
order — so the database whose failure matters most is the one that fails first.

The instance pool joined in #108, for the reason the region shard left: it is a production tier a
fresh clone could not exercise at all, so instance routing was the one split nobody developed
against. Both shard-map rules are the same shape — an instance map routes exactly like a continent
one, and the bucket half of a rule exists to spread ONE map's instances over a pool of several
databases, which a one-database pool does not need.

Three names here — `lyracore`, `lyracore-instances`, `lyracore-realm` — are also production's. What
keeps a fixture off a production node is the **node**, not the name: every publish this CLI renders
is `-s local`, against the SpacetimeDB on loopback:3000 that `dev up` starts.

There is no map-0 region shard here any more. LyraCore's alpha topology reversal (2026-08-08)
retired location/region sharding, and the gateway stopped reading `LYRACORE_REGION_SHARDS` and
`game_map_region` with it — so a database for it was one `dev up` published, claimed, and then
reported as a collapsed realm on every single run, because the gateway never connected it.
`LYRACORE_REGION_SHARDS` is still *actively unset* for the child in both modes, so an old recipe
exported in your shell cannot put it back.

**Dungeon populations spawn on the world shard until you say otherwise.** Routing map 36 off
`lyracore` does not by itself move the spawning: `game_config.hosts_instances` is a per-database
flag that defaults to on everywhere, so the gateway logs one WARNING per start saying every
Deadmines entry spawns its ~207 creatures on the world writer and evicts them again after the
transfer. The run works. To model the production Phase A split, turn the flag off on the world
shard once — it survives a republish:

```
spacetime sql -s local lyracore "UPDATE game_config SET hosts_instances = false WHERE id = 0"
```

Leave it alone under `dev up --single`, where the one database has to host its own dungeons.

The steps:

1. Starts SpacetimeDB on `127.0.0.1:3000`, **or reuses one already listening there.** A node the CLI
   did not start is never recorded and never stopped by `dev down`.
2. Builds the gateway.
3. Runs `preflight`, then publishes each database in turn, through the same internal command
   `lyracore publish` uses — which is what guarantees `--features=debug_reducers`, `--yes`,
   `-s local`, and the unreachability of a `-c` wipe. No path here renders a `spacetime publish`
   any other way, clears a database, or re-selects the SpacetimeDB server. A failure part way
   through names the databases that *did* land, because a half-published realm presents as an
   unrelated mid-session hang rather than a loud "no such table".
4. Resolves the coordinator credential (see below), minting one from the local node if this host has
   no SpacetimeDB login.
5. Calls `claim_operator` **as that identity**, on every database (idempotent for the same identity,
   so repeating `dev up` is not an error). A shard claimed by nobody, or by a different identity,
   refuses the gateway's own writes — and nothing fails until the first write that shard has to
   serve.
6. Starts the gateway with the same credential, bound to loopback.
7. **Reads the realised topology back out of the gateway's own log** and fails if it came up short.
   See below.

### The silent collapse, and what is done about it

A gateway's response to bad topology configuration is not an error. It is **collapse to one
database**: a malformed shard-map rule is logged and dropped, an absent — or default-equal —
`LYRACORE_REALM_CORE` reads as "unconfigured", an empty `LYRACORE_SHARD_MAP` still counts as set, and
a database that never published is "unreachable, falling back to the default". The result starts,
binds, answers its health probe, passes every PID-and-port check, and serves one database while the
others sit published, claimed and unused.

So `dev up` does not stop at exporting the right strings:

* it reads `coordinator connected to shard <db>` back out of `.lyracore/logs/gateway.log` — the
  gateway awaits every one of those connections *before* it binds its listeners, so a gateway
  answering its port has already written all of them — and **fails, naming the missing databases**,
  if the realm came up short. The gateway is left running and recorded, so `dev down` still stops
  it;
* a gateway build that does not log that line at all is reported as *unverified* rather than as
  collapsed;
* `dev status` reports each database separately: published or unreachable, connected or never
  reached.

### `--single` — one database, on purpose

`dev up --single` is the pre-sharding fixture, unchanged: one database, no realm-core, and
`LYRACORE_SHARD_MAP`, `LYRACORE_SHARD_MAP_FILE`, `LYRACORE_REALM_CORE` and `LYRACORE_REGION_SHARDS`
all *actively unset* for the child gateway — so a contributor who has the production recipe exported
in their shell still gets the fixture, not a multi-database gateway pointed at databases this CLI
never published. An unconfigured shard map collapses every lookup to `LYRACORE_DATABASE`, making the
result equivalent to a single-database build.

The two options compose: `dev up --single --lan 192.168.1.50` is a one-database realm on the LAN.

A running gateway cannot be re-sharded any more than it can be rebound — its shard set is read from
the environment once, at startup. Switching modes is refused with the `dev down` to run first, rather
than reporting "already up" for a realm with the wrong number of databases in it.

### The coordinator credential

Steps 4–6 are not optional plumbing. `game_account` and `game_session` are **private** module tables
and `provision_account` is operator-gated, so the gateway's coordinator connection has to
authenticate as the identity that claimed the operator. Without it the gateway starts, warns, and
dies ~15 seconds later on `coordinator subscriptions not applied within 15s`, which reads like a
broken node rather than a missing credential; `account create` would fail as "operator only" for the
same reason.

Where the credential comes from, in order:

1. **`.lyracore/coordinator-token`** — one this CLI minted on an earlier run. It wins, because it is
   the identity that already claimed the operator: `claim_operator` is idempotent for the same
   identity and refuses a different one, so preferring anything else once this file exists would
   lock the checkout out of its own database.
2. **`spacetime login show --token`** — if you already use SpacetimeDB, your identity is reused and
   nothing is minted or stored.
3. **`POST /v1/identity` on the local node** — a **server-issued** identity, minted from the node
   `dev up` just started and persisted at mode `0600`.

Step 3 is what keeps the quickstart anonymous. `spacetime login` offers only the spacetimedb.com
browser flow, so requiring it would put a third-party account signup in front of `git clone &&
./lyracore dev up`; a server-issued token is exactly as privileged here, because the module trusts
whoever claimed the operator and this CLI claims it with the token it just minted. That claim is
made over the node's HTTP API rather than by shelling out to `spacetime call`, which would run as
the CLI's identity instead — a claim by one identity and a gateway running as another is precisely
the lock-out this avoids.

`account create` uses the same ladder **without** step 3: a freshly minted identity has claimed
nothing, so it would be refused after the password had already been read. It says to run `dev up`.

The credential reaches a child as an **environment variable, never an argument** (`ps` shows
nothing), and as an `Authorization` header, never a URL. `CommandSpec` renders program and arguments
only — so it cannot reach a log line, an error message, or `state.json`. The one place it touches
disk is `.lyracore/coordinator-token`, created `0600`, inside a git-ignored directory.

Re-running `dev up` on a healthy stack does nothing; on a partially-up stack it starts only the
missing part.

### `--lan <IP>` — let another machine on your network connect

`dev up --lan 192.168.1.50` binds the two CLIENT-FACING listeners (logon 3724, world 8085) to that
address and advertises it in the realm list, so a 1.12.1 client elsewhere on the LAN can set its
realmlist to it and play.

**SpacetimeDB is not part of that.** It stays on `127.0.0.1:3000` in every mode: the database's
admin surface is not something a `dev` command should put on a network.

The address must be a private one — `10.0.0.0/8`, `172.16.0.0/12`, or `192.168.0.0/16`. A public
address, or `0.0.0.0`, is a usage error rather than a wildcard bind, because "expose an alpha game
server to the internet" should not be one mistyped character away from "let my flatmate log in".

A running gateway cannot be rebound: switching modes is refused with the `dev down` to run first,
rather than reporting "already up" for a realm that is not listening where you asked.

### `dev smoke`

Runs the pinned wire harness's generic login smoke — logon, world handshake, character enumerate,
enter world — against the running fixture.

The harness is a separate, server-agnostic repository consumed as the RELEASE pinned in the
checkout's `.wire-harness-rev` (`<tag> <full sha>`), and this CLI owns the consume path:

* the **tag** is what is cloned — a release, never a branch, and there is deliberately no way to say
  `main`;
* the **sha** is what the checkout is then verified against, because a tag is a mutable ref and
  "pinned to a tag someone moved" is not pinned. A mismatch is reported as a supply-chain event, not
  a stale cache;
* the clone lands in the git-ignored `.lyracore/wire-harness/<sha>/`. The
  [wire-harness repository](https://github.com/LyraCoreProject/wire-harness) is public and fetched
  over HTTPS, so no GitHub account or SSH key is needed;
* `LYRACORE_WIRE_HARNESS_DIR=/path/to/wire-harness` overrides all of that with a local working tree.
  It is validated, and announced on stderr every time — a stale local checkout silently substituted
  for the pin is a measurement nobody can reproduce.

The seam is resolved **inside that pinned checkout**, not from an `adapters/` directory in the
server repo. The wire client is built from the harness's own manifest.

It signs in as the fixture account, so provision that first:

```bash
printf 'test123' | lyracore account create TEST --password-stdin
```

### Not the production topology

The sharded fixture is not the production recipe in the server repo's `docs/danger-zones.md` §3 —
different databases, loopback only, and every topology variable decided by this CLI rather than
inherited from your shell. Nothing here reads one out of the environment in either mode.

A gateway already serving the world port is **refused, never adopted** — its build and topology are
unknown, and the health probe would otherwise pass against someone else's listener.

## Stopping things safely

A bare PID is not an identity: PIDs get reused, and signalling a recycled one kills a stranger's
process. Each recorded PID is stored with its process start time and command name, read via POSIX
`ps` — no `/proc`, no `grep -P`, no GNU-only flags, so Linux and macOS behave the same.

`dev status` checks the things that can be wrong independently: the recorded PID is still the
process the CLI started, its endpoint answers (on the LAN address, in LAN mode), and — **per
database** — that it is published on the node and that the gateway actually connected to it. A stack
whose PIDs and ports are both perfect is still broken if a database was never published, and still
half-broken if one was published and never reached; in a sharded realm the database that *is* fine
is invariably the default one, which is why the report is per-database rather than one line.

Which databases it reports is read from `state.json`, so it describes the realm that is actually
running rather than the one today's default would build.

`dev down` compares that identity before signalling anything. If the PID now belongs to something
else it **refuses and kills nothing**, directing you to `dev down --forget`, which drops the record
without signalling.

## `service reconcile` — make the host match the tracked unit

```bash
sudo ./lyracore service reconcile
```

For a **production host** only. It makes the host's `spacetimedb-standalone` supervisor match the
unit tracked in the checkout at `deploy/systemd/spacetimedb-standalone.service`. Deployment
reconciliation is one job, so the verb owns the git steps too. In order:

1. **Root, up front.** `id -u` runs before the fetch. The plan resets the checkout and then writes
   to `/etc/systemd/system`, and a plan that stops halfway for a password is worse than one that
   never started.
2. **The same checkout update `update` does.** Fetch `origin`, refuse over tracked local edits, and
   move to `origin/main` with `git reset --hard`. Tracked local edits refuse *everything*, the
   service change included.
3. **Host prerequisites.** `busctl` with JSON output, the unit's `User=` account, its `ExecStart`
   binary, its `--data-dir`, and the directory holding its `StandardError=append:` log. Each missing
   one is a refusal before the host changes. None of them is created for you.
4. **A conflicting service.** Every active unit whose `ExecStart` or `WorkingDirectory` claims the
   same data directory or listen address. A hand-rolled legacy `spacetimedb.service` is named and
   refused, never migrated and never stopped on your behalf, so two nodes cannot race for one port
   and one data directory.
5. **The install.** `install -o root -g root -m 0644` into `/etc/systemd/system/`, then
   `systemctl daemon-reload`, `enable`, `restart`.
6. **The verification.** `systemctl show` must report `ActiveState=active`, the tracked `LimitNOFILE`,
   and stderr mode `append`. The CLI compares the running `MainPID` file descriptor 2 with the tracked
   log by device and inode. A different destination or unreadable file fails reconciliation. The
   typed `ExecStart` D-Bus property must match the tracked command, including each argument boundary.
   If the command differs, the failure also names the applicable systemd drop-ins. A node with the
   inherited 1024-descriptor ceiling fails reconciliation.

Every expected value is read out of the tracked unit rather than duplicated in this CLI, so it
cannot certify a host against a contract the checkout no longer ships. The node's persistent data
directory is only ever checked for existence: never created, moved or deleted. Two runs converge on
the same end state, and it runs even when the checkout is already at `origin/main`, because
deployment drift is independent of git drift. It restarts the node every time, so every run costs a
short outage.

Steps 3 and 4 check the host before changing it, so a failure there leaves the checkout on
`origin/main` and the host as it was. The reset in step 2 comes first on purpose: the unit to
install, and the contract to check the host against, are read out of the updated checkout.

No gateway, module publish or schema migration is touched. Those stay operator decisions. Plain
`update` is the contributor command and reconciles nothing.

## Passwords

`account create` reads the password from a hidden terminal prompt, or from one bounded stdin line
with `--password-stdin`. It is handed to `lyracore-gateway provision USER --password-stdin` over the child's
stdin and never becomes a command-line argument, so `ps` shows only the username. It is held in a
zeroized buffer and is absent from rendered commands, logs, error messages, and `state.json`.

```bash
printf 'hunter2' | lyracore account create TEST --password-stdin
```

On a sharded realm the account is written **twice** — once on the world shard, whose account id owns
the characters, and once on realm-core, which is where the logon server answers the SRP6 challenge
from. `account create` reads the running realm's topology out of `state.json` and hands the
provisioning child the same `LYRACORE_REALM_CORE` the gateway is using. Without it the account
exists, the command reports success, and the login is refused forever.

## Project layout coupling

`src/project.rs` is the single adapter holding the target project's internal database, package,
path, and bind names. Renaming those internals is a one-file change here, and no public command
surface moves with it.

Publishing and preflight live in this CLI's `cmd/publish.rs` and `cmd/preflight.rs`. World import
drives Core's importer binary and its `importer/scripts/` tools for the cMaNGOS fetch, class spells,
and profile checks. `dev smoke` fetches the test code selected by Core's `.wire-harness-rev`.

## Host operations scripts

`deploy/` holds the production host scripts that are not CLI commands: the SpacetimeDB history
prune, the disk guard that renews the Bot Capacity Lease, the diagnostic capture wrapper, and their
systemd units. They moved here from the LyraCore repository. The server's `docs/danger-zones.md`
and `docs/operations/disk-policy.md` give the install steps; run them from a checkout of this
repository. The Standalone Supervisor unit stays in the LyraCore checkout, because
`service reconcile` reads it from there.

```bash
bash deploy/spacetimedb-prune-test.sh
python3 -m unittest discover -s deploy -p 'test_*.py'
```

## Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | Success — including "already up", "already down", and a `doctor` with only warnings |
| `1` | Operational failure: missing prerequisite, failed subprocess, or a refused foreign PID |
| `2` | Invalid invocation, or not inside a checkout |

`doctor` exits nonzero only for launch-blocking failures. A busy port is a warning, not a failure —
it is usually your own running stack.

## Development

```bash
cargo test
cargo +1.85 check   # the supported minimum toolchain
```

Tests run entirely against fake command and process adapters plus temporary directories; nothing in
the suite starts a real server or touches a real process.

## License

MIT OR Apache-2.0
