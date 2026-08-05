# lyracore-cli

Source-first developer CLI for LyraCore. It drives the local, single-database developer fixture:
start SpacetimeDB, publish the module, claim the operator identity, run the gateway, and provision
accounts.

It deliberately does **not** manage production realms, sharding, backups, system services, or the
installation of Rust and SpacetimeDB.

## Commands

```text
lyracore doctor
lyracore dev up [--lan <IP>]
lyracore dev status
lyracore dev logs [spacetime|gateway]
lyracore dev smoke
lyracore dev down [--forget]
lyracore account create USER [--password-stdin]
```

Runtime state lives in the git-ignored `.lyracore/` of the target checkout — `state.json` for the
processes the CLI started, `logs/{spacetime,gateway}.log`, and `coordinator-token` (mode `0600`) if
this host had no SpacetimeDB login and the CLI minted a local identity.

## What `dev up` does

1. Starts SpacetimeDB on `127.0.0.1:3000`, **or reuses one already listening there.** A node the CLI
   did not start is never recorded and never stopped by `dev down`.
2. Builds the gateway.
3. Publishes **only** the seeded fixture database, always through the target checkout's
   `scripts/publish-module.sh` — which is what guarantees `--features=debug_reducers`, `--yes`,
   `-s local`, and the refusal to forward a `-c` wipe. No path here invokes `spacetime publish`
   directly, clears a database, or re-selects the SpacetimeDB server.
4. Resolves the coordinator credential (see below), minting one from the local node if this host has
   no SpacetimeDB login.
5. Calls `claim_operator` **as that identity** (idempotent for the same identity, so repeating
   `dev up` is not an error).
6. Starts the gateway with the same credential, bound to loopback.

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
enter world — against the running fixture, by handing off to the checkout's own
`adapters/lyracore/run-suite.sh`. The harness is a separate, server-agnostic repository that the
checkout pins; this CLI resolves nothing about it and overrides nothing, so
`LYRACORE_WIRE_HARNESS_DIR` reaches it through the inherited environment exactly as documented
there.

It signs in as the fixture account, so provision that first:

```bash
printf 'test123' | lyracore account create TEST --password-stdin
```

### One database, on purpose

This fixture is a **single-database** stack. The multi-database production topology is not
reproduced here, and `LYRACORE_SHARD_MAP`, `LYRACORE_SHARD_MAP_FILE`, `LYRACORE_REALM_CORE`, and `LYRACORE_REGION_SHARDS`
are *actively unset* for the child gateway — so a contributor who has the production recipe exported
in their shell still gets the fixture, not a multi-database gateway pointed at databases this CLI
never published. An unconfigured shard map collapses every lookup to `LYRACORE_DATABASE`, making the result
equivalent to a single-database build.

A gateway already serving the world port is **refused, never adopted** — its build and topology are
unknown, and the health probe would otherwise pass against someone else's listener.

## Stopping things safely

A bare PID is not an identity: PIDs get reused, and signalling a recycled one kills a stranger's
process. Each recorded PID is stored with its process start time and command name, read via POSIX
`ps` — no `/proc`, no `grep -P`, no GNU-only flags, so Linux and macOS behave the same.

`dev status` checks all three things that can be wrong independently: the recorded PID is still the
process the CLI started, its endpoint answers (on the LAN address, in LAN mode), and the fixture
database is actually published on the node — a stack whose PIDs and ports are both perfect is still
broken if nothing was ever published to it.

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

## Project layout coupling

`src/project.rs` is the single adapter holding the target project's internal database, package,
script, and bind names. Renaming those internals is a one-file change here, and no public command
surface moves with it.

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
