<p align="center"><img src="https://raw.githubusercontent.com/LyraCoreProject/LyraCore/refs/heads/main/lyracore-icon-light.svg" alt="LyraCore" width="120"></p>

# lyracore-cli

Source-first developer CLI for LyraCore. It drives the local developer fixture: start SpacetimeDB,
publish the module, claim the operator identity, run the gateway, and provision accounts.

It deliberately does **not** manage production realms, backups, system services, or the installation
of Rust and SpacetimeDB.

## Commands

```text
lyracore doctor
lyracore preflight
lyracore publish [DATABASE ...] [--skip-preflight]
lyracore dev up [--single] [--lan <IP>]
lyracore dev status
lyracore dev logs [spacetime|gateway]
lyracore dev smoke
lyracore dev down [--forget]
lyracore account create USER [--password-stdin]
```

Runtime state lives in the git-ignored `.lyracore/` of the target checkout — `state.json` for the
processes the CLI started, `logs/{spacetime,gateway}.log`, and `coordinator-token` (mode `0600`) if
this host had no SpacetimeDB login and the CLI minted a local identity.

## `preflight` — the offline deploy gate

```bash
lyracore preflight
```

The break class `cargo test` and `cargo check` cannot see. It touches **no node**: no publish, no
call, no sql, no database, so it is safe to run against a live stack. Five checks:

| # | Check | The break it catches |
| --- | --- | --- |
| 0 | `rustc` and the `spacetime` CLI **exactly** match the versions the checkout pins (`rust-toolchain.toml`, `module/Cargo.toml`) | tools drifting out from under the pin — a CLI ahead of it publishes a schema the repo never tested against |
| 1 | the module compiles with `--features=debug_reducers` | code that only a publish compiles, so the default test config never sees it |
| 2 | real, offline wasm schema extraction (`spacetime generate` into a scratch directory) | a `#[default(0)]` on a `u64`, which SpacetimeDB rejects at migration time and nothing in-tree validates |
| 3 | every `#[client_visibility_filter]` names real tables and columns | a filter stored as raw text at publish, rejecting a gateway **subscription** at login time |
| 4 | a script with a configurable `DB` target threads it into every tool it drives | an ETL writing to one database and asserting against another |

Every check runs even after one fails, so a run hands back every problem rather than one per
attempt. Check 0 is an EXACT match, unlike `doctor`'s minimum-version floor: newer is not fine when
a publish is the thing being gated. Where `spacetimedb-standalone` is unavailable,
`PREFLIGHT_SKIP_SCHEMA=1` skips check 2 — and then nothing validates your `#[default]` encodings.

## `publish` — the one correct deploy

```bash
lyracore publish                                   # the fixture database
lyracore publish lyracore lyracore-world-1 realm-core
```

Runs `preflight`, then `spacetime publish -s local -p <checkout>/module
--build-options=--features=debug_reducers --yes <DATABASE>` for each database in turn, stopping at
the first failure. It takes database **NAMES**:

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
* the clone lands in the git-ignored `.lyracore/wire-harness/<sha>/`, so it can never appear in the
  server repo's `git status`. The repository is private, so the clone uses your existing git
  credentials over ssh;
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

The CLI drives a checkout through `Cargo.toml`, `rust-toolchain.toml`, `module/`, `scripts/*.sh` and
`.wire-harness-rev` — it does **not** shell out to any script in the target repository. The
guarantees that used to belong to `scripts/publish-module.sh` and `scripts/preflight.sh` are
properties of `cmd/publish.rs` and `cmd/preflight.rs` here, so a checkout that ships without a
`scripts/` or `adapters/` directory is still fully drivable.

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
