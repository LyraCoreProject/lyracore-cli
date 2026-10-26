# LyraCore development CLI

This is the command reference for `lyracore`. It moved here from the LyraCore repository's
`docs/development-cli.md`. "This checkout" means the LyraCore checkout the CLI runs in.

> Looking for the walkthrough rather than the contract? **[`docs/quickstart.md`](https://github.com/LyraCoreProject/LyraCore/blob/main/docs/quickstart.md)**
> is the clone → running realm → connected client path, with prerequisites and troubleshooting.
> This page is the CLI's command and safety contract.

`lyracore` runs a local developer realm, imports content, manages Packages and inspects an
explicit production topology. It does not install Rust or SpacetimeDB or manage backups.
Host service administration lives in the separate [`lyracore-ops` tool](operations.md).

## Running it

From the repository root:

```bash
./lyracore doctor      # are the prerequisites present?
./lyracore dev up      # start (or reuse) the local stack
```

If you installed with [`install.sh`](https://github.com/LyraCoreProject/LyraCore/blob/main/install.sh), the same commands are `lyracore doctor` /
`lyracore dev up` from anywhere inside the checkout: `$HOME/.local/bin/lyracore` resolves the
checkout you are standing in and runs *its* pinned CLI. `./lyracore` is the form used throughout this
document; the two are the same program.

`./lyracore` installs the exact CLI commit pinned in **`.lyracore-cli-rev`** into the git-ignored
`.lyracore/cli/<rev>/`, then runs it. The first run for a new revision builds it; later runs exec
the cached binary. So the CLI version is a property of the checkout, not of whatever is on your
PATH.

The install fetches the CLI over anonymous **HTTPS** — no SSH key, no credential helper, nothing to
configure. (It used an `ssh://` remote with `CARGO_NET_GIT_FETCH_WITH_CLI=true` while that repository
was private, which meant a fresh clone's very first command failed for anyone but a maintainer.)

**To bump the CLI:** put the new commit SHA in `.lyracore-cli-rev` and commit it.

## Commands

<!-- command-reference:start -->
```text
lyracore help [--all | COMMAND ...]
lyracore doctor
lyracore bindings [--check]
lyracore preflight
lyracore publish [DATABASE ...] [--skip-preflight] [--server URL]
lyracore dev up [--single] [--lan IP]
lyracore dev watch
lyracore dev status
lyracore dev logs [spacetime|gateway] [--all]
lyracore dev check
lyracore dev smoke ACCOUNT CHARACTER [--password-stdin]
lyracore dev down [--forget]
lyracore account create USER [--password-stdin]
lyracore character gm NAME true|false
lyracore sql QUERY
lyracore import [--accept] [--client-data PATH] [--profile-shard PROFILE=SHARD ...]
lyracore import world [--accept] [--client-data PATH] [--profile-shard PROFILE=SHARD ...]
lyracore import check [--profile-shard PROFILE=SHARD ...]
lyracore import vmaps [--client-data PATH] [--profile-shard PROFILE=SHARD ...]
lyracore config
lyracore config set client-data PATH
lyracore client sync
lyracore client pack --out DIR [--zip]
lyracore packages add FOLDER|GIT-URL|NAME [--yes]
lyracore packages update [NAME] [--yes]
lyracore packages enable NAME
lyracore packages disable NAME [--yes]
lyracore packages remove NAME [--yes]
lyracore packages apply [DATABASE ...] [--check] [--yes] [--force-all] [--client-data PATH]
lyracore packages config NAME [KEY [VALUE]] [--new] [--database NAME ...]
lyracore packages build
lyracore packages check
lyracore packages list
lyracore packages new NAME [--from RUNG]
lyracore production status --server URI --gateway-log PATH --realm-core DB DATABASE ...
lyracore update [--reset-mirror]
```
<!-- command-reference:end -->

Use `lyracore help --all` for the complete descriptions, `lyracore packages --help` for a
command family, or `lyracore help packages apply` for one operation. Full and focused help
come from the same catalogue; tests keep the command list above in sync with it.

**The CLI shells out to nothing in `scripts/`.** It drives this checkout through its *files* —
`Cargo.toml`, `rust-toolchain.toml`, `module/`, `scripts/*.sh` (read, not run), `.wire-harness-rev`.
The shell scripts remain the authority for CI and for anyone driving the repo directly; the CLI's
job is to be usable in a checkout that has neither `scripts/` nor `adapters/`.

`import` is the one deliberate exception, and it is a different thing: `importer/scripts/` is not
private repo plumbing but **shipped tooling**, part of the importer the same way `module/src` is
part of the module. Those scripts carry post-ETL assertions tuned against real dumps over months;
`import` is a façade that adds ordering, a consent gate and per-stage diagnosis on top of them, on
the same path `publish` took before it was absorbed into Rust. A checkout without
`importer/scripts/` gets a named prerequisite error, not a silent no-op.

## `import` — the real world, on your machine

```bash
./lyracore import                                        # prompts for consent and for the client path
./lyracore import --client-data /games/WoW-1.12.1/Data   # prompts for consent only
./lyracore import --accept --client-data /games/…/Data   # scripted: consent answered in advance
```

**The consent notice is printed in full before any network access, client archive read, or database
write.** The CLI may first check that a supplied path and its required filenames exist; it does not
open those archives. The notice names cmangos' `classic-db` and its GPL-3.0 licence,
states that the content describes Blizzard's copyrighted game world and that this project never
distributes it or anything built from it, and states that the DBC half comes from the user's own
1.12.1 client. Only a typed `yes` or `--accept` proceeds; anything else exits 2 having run nothing.
There is no terminal-less default: `import` with no TTY and no `--accept` refuses.

The command has three setup stages: fetch and checksum the pinned dump, resolve the client `Data/`
directory, and build `lyracore-importer`. Each content destination then has three fail-fast stages:
run its importer modes, re-arm its schedules and gather pools, and run profile-aware Verification.
That is six displayed stages under `--single` and twelve on the sharded fixture.

Server-issued Operator credentials reach importer subprocesses through a temporary SpacetimeDB
CLI config with owner-only permissions. The CLI removes that config when the import ends. The
global SpacetimeDB login stays intact.

The sharded destination plan is fixed:

| Destination | Profile | Spatial result |
| --- | --- | --- |
| `lyracore` | `alliance-eastern` | Human corridor, Dun Morogh and Loch Modan on map 0, plus Deeprun Tram map 369 |
| `lyracore-kalimdor` | `alliance-kalimdor` | Teldrassil and Darkshore on map 1 |
| `lyracore-instances` | `instances` | whole map 36 with required WMO geometry, no open-world terrain or navigation pass |

The single topology uses `alliance-single`, the union of those bounded continent slices and maps 36 and 369.
Every importer child names its destination and loopback SpacetimeDB endpoint. The world-content
mode loads the full Spell.dbc catalogue before the world AI, because the Module refuses EventAI
relay definitions that name a missing spell. That catalogue already supplies caster spells, so
Verification derives every referenced caster spell from `game_creature_cast` and
`game_creature_spell` instead of maintaining an independent allowlist.

If a mode fails, later modes and later destinations do not start. The error names the destination,
profile and mode. Fix the cause and rerun the full command. Import families use clear-and-reload, so
the rerun repairs a partial family; the complete multi-destination operation is not atomic.

`import world` automatically imports canonical Vmap geometry for profiles that own Deeprun Tram
or Deadmines before recording content completion. Run `./lyracore import vmaps` to refresh
World Shard collision data afterward.
It follows the World Shard profiles and skips the Instance Pool. Importing vmaps does not enable
exact rays. Enabling them is a separate Operator decision after `docs/verification/vmap-rollout.md` Verification.

The importer also has one Instance Vmap Slice named `deadmines-entry-exit`. A direct
`lyracore-importer --vmap <client Data/ dir> --world-profile instances` dry run resolves Map 36
through `Map.dbc` and reads only the two ADT tiles crossed by the entry, exit radius, and one-cell
collar. It reports the Map.dbc, WDT, ADT, WMO root, and WMO group identities, placement calibration,
supported floor samples, per-sample height changes, headroom, short collision probes, and the direct
collision ray. Applying this slice is refused unless the entry and every sample needed to reach the
exit trigger sphere are supported, the entry and supported endpoint match their authored route
heights, each height change is walkable, and those static probes are clear. Samples continue to the
trigger center for diagnostics; the center need not have a floor when the supported route has already
entered the trigger volume.
This establishes static geometry suitability, not actual Character movement. The private fixture
must exercise the ordinary Core route before the attended client check. Selected WMO groups that
reference active doodads are also refused until their nested transforms are supported. `import world` applies this slice to the Instance Pool; the separate `import vmaps` refresh
command still visits World Shards only. Map 36 terrain and Navigation
Coverage remain unavailable until archive-derived evidence supports a representation that preserves
its floors.

The automated repository checks cover destination plans, profile fences, canned SQL failures, and
synthetic importer rows. They do not read a real pinned dump or client archives. A lawful real-data
profile dry run, terrain and navigation extraction, vmap generation, and stock 1.12.1 client
playthrough remain Operator Verification; a successful plan or unit test is not evidence that those
checks passed.

One Verification runs from `cargo test` when you point it at your own client: with
`LYRACORE_CLIENT_DATA=<client Data/ dir>`,
`cargo test -p lyracore-importer alliance_eastern_terrain -- --ignored` runs the `alliance-eastern`
terrain dry run and asserts every Bounded Map Slice self-check is within tolerance, Loch Modan
included. Run it before an `--apply` after changing a profile anchor. It is an ignored test, so a
plain `cargo test` does not run it.

A `--client-data` path and its required filenames are checked **before** the consent notice is
answered, so a typo costs nothing; archive reads still start only after consent. The flagless run is
prompted for at stage 2, in order. Every stage runs from the checkout
root regardless of the directory you invoked `lyracore` from. Every stage's target database is
passed explicitly, never left to a script's default — that default is `lyracore`, and a silent one
is how a shard once had its spells written to a different database entirely.

## `config` — remembering your client-data path

```bash
./lyracore config                          # show what's remembered (or "(unset)")
./lyracore config set client-data PATH     # remember one
```

Stored in the git-ignored `.lyracore/config.json`, repo-local to this checkout — it is not your
`spacetime` login, and it is not shared across clones. `config set client-data` runs the same
validation `import` does (the install-root-vs-`Data/` check, the TBC-or-later rejection) before
writing anything, so a bad path is refused with the same diagnosis either command would give you.

This is also `import`'s fallback chain, not just a separate command: stage 2 tries `--client-data`
first, then the value in `config.json` if one is set and still valid, and only then falls back to
the interactive prompt — which, once you type a path that validates, is **saved to `config.json`
for you**, so a plain `./lyracore import` never asks twice.

## `packages` — install, scaffold, and see what is installed

```bash
./lyracore packages add ~/src/my-package       # asks before it copies anything
./lyracore packages add ~/src/my-package --yes # answer the confirmation in advance
./lyracore packages add https://host/greeter.git   # clone a repository whose root is one Package
./lyracore packages add greeter                # bare name: resolve from the Official Package Collection
./lyracore packages list
./lyracore packages new my-package             # start from example-script
./lyracore packages new my-package --from example-rust
```

A Package is a drop-in folder under `packages/<name>/`. `module/build.rs` discovers it and compiles
its `src/` into the module wasm; `--pack-client` picks up its `client/` half. Installing one adds
code that runs inside the module with full access to every table in the database, so `add` is built
around saying so.

**Everything that can refuse the install happens before anything is copied.** The name must be one
the build accepts (`[a-zA-Z][a-zA-Z0-9_-]*` — the build maps `my-package` onto the module
`pkg_my_package` and panics on anything else). A Package must carry `src/`, `client/`, `data/`,
`scripts/` or `datascripts/`. When `src/` exists, `src/mod.rs` is required. The name must
collide with neither the enabled nor the disabled inventory, compared on the Rust identifier rather
than the folder name — `my-package` and `my_package` fold onto the same module.

Then it prints a **Trust Review** and asks. The review is a deterministic, read-only scan of the
candidate folder using a port of the build's own marker scan, so a commented-out or quoted marker
registers nothing here either. It reports tables, reducers, hooks, tick passes, character-owned
sweeps, addons, client overrides and Runtime Script sources. Runtime Scripts are named because they
run on the Realm once the Package is built. It also names Package-local Datascripts, which
`packages build` runs as trusted code on the author's machine. The review states plainly that
everything else in the Package's Rust is trusted code and that it is an inventory, not a security
guarantee.

On confirmation the folder is **copied, never symlinked**: a linked Package would compile from a
folder outside the checkout, so `preflight`, `publish` and `client sync` would each read whatever
that folder said at the time. A symlink anywhere inside the candidate is refused for the same
reason. A `.git` directory is skipped.

The install then writes a **Provenance Stamp** — `packages/<name>/.lyracore-package.toml`, holding
the Package Source, the Content Identity of what was copied, and the install time.
Server content runs `preflight`; a client-only Package skips the module build and is validated
when `client sync` or `client pack` packs its content. A Git Package Source records one key more: the exact commit that was installed.

**An argument that looks like a URL is a Git Package Source.** `https://`, `http://`, `ssh://`,
`git://` and the scp-style `git@host:path` are cloned. Everything else is a path on this machine, as
it always was, other than a bare word that resolves as neither (see below). The repository's root is
the Package, so the Package takes the repository's name without the `.git` suffix, and a repository
whose name the build would refuse is refused here. The clone lands in scratch space under
`.lyracore/`, and what gets installed is a copy of its tree without the `.git`. An installed Package
is a fixed tree, never a working copy. The clone needs credentials or it fails; it never sits
waiting on a hidden prompt.

**A bare word that is not a path on this machine is an Official Package Source.** `packages add
greeter` resolves `greeter` against the one Official Package Collection this CLI knows,
`LyraCoreProject/packages`, which holds several first-party Packages side by side, one top-level
directory each. The version in the checkout's `docs/package-api.md` selects a tag such as `api-v1`.
The CLI fetches that tag and sends the named directory through the same Trust Review and consent
question as any other install. A missing tag refuses the install; the rest of the clone is
discarded. An unknown name is refused before anything is copied; a name that only differs
from one already in the collection by hyphen/underscore folding is named in the refusal instead of
installed in its place. The Provenance Stamp records the collection's URL and the exact commit the
directory was resolved at. `packages update` can advance the installed Package to the current
commit at the compatible tag, after the normal Trust Review and confirmation.

The command prints the next steps for the content it installed:

```bash
./lyracore packages apply # prepare and activate installed Packages
./lyracore client sync    # if the Package ships client content
```

A failed `preflight` leaves the copy in place, says the module on the node is unchanged, and names
the exact `rm -rf` that undoes the install — so you can fix the Package where it sits and re-run
`preflight` instead of starting over.

`packages list` reports, per Package: enabled or disabled, its Package Source, its recorded Content
Identity, whether the tree on disk still matches it (`clean` or `LOCALLY DRIFTED`), and what it
registers. A Package with no stamp — dropped into `packages/` by hand, or installed before this
command existed — renders as unrecorded rather than failing the listing.

`packages new NAME [--from RUNG]` fetches a Reference Package from the Official Package Collection.
The checkout's Package API version selects its tag, as with `packages add`. The default is
`example-script`, which needs no Rust. Other rungs are `example-client`, `example-data`,
`example-rust` and `example-all`. A missing tag or rung leaves the Package Inventory unchanged.

The command copies the source into `packages/NAME/`, renames Package names, Rust identifiers and
client filenames and prints its Trust Review. Server-content scaffolds run `preflight`;
client-only scaffolds are validated when packed. The Provenance Stamp keeps the
`scaffold` kind and records the chosen rung and exact collection revision. `packages update`
does not replace scaffolded code.

Rungs with Runtime Scripts or Datascripts need a lowercase Package name of at most 64 characters.
Each `<name>.<script file stem>` must also fit the 64-character Runtime Script name limit.

Generated artifacts and `script-ids.json` are omitted. The Runtime Script Toolchain assigns IDs
for the new Package when it builds the renamed sources. Before using a Datascript copy beside
another, choose distinct Package Spell IDs. Run `packages apply` to build and activate the copy.
A Datascript needs your own client data. Use `client sync` for client content. Scaffolding requires
network access to the collection.

## `packages enable`, `disable`, `remove` — taking a Package out of the build

```bash
./lyracore packages disable my-package        # tear down, then out of the build, still on disk
./lyracore packages disable my-package --yes  # answer the teardown question in advance
./lyracore packages enable my-package         # back into the build
./lyracore packages remove my-package         # delete a disabled Package, asks first
./lyracore packages remove my-package --yes   # answer the deletion question in advance
```

**Enabled is a location, not a recorded flag.** `packages/` is what the build discovers.
`.lyracore/packages-disabled/` is git-ignored local state the build cannot see. `enable` and
`disable` rename one folder between the two. Nothing can disagree with the filesystem about which
Packages the next build compiles.

Both directories are on the same filesystem, so the rename is atomic and each verb undoes the other's
move. The Provenance Stamp lives inside the folder, so it travels with the move and is never
rewritten: a re-enabled Package still reports its Package Source and still reads as `clean` rather
than drifted. `enable` never asks. `disable` asks before it runs Package Teardown, because teardown
deletes the Package's rows.

Name collisions fail before anything moves, on the Rust identifier rather than the folder name. A
disabled `foo_bar` cannot be enabled next to an enabled `foo-bar`, because both fold onto
`pkg_foo_bar`.

**`disable` runs Package Teardown before it moves anything.** Disabling takes the Package's tables
out of the schema, so the next publish removes them. SpacetimeDB refuses to remove a table that
still holds rows, and `lyracore publish` never passes the destructive wipe flag, so that publish
stops. Teardown prevents this. When the recorded dev stack is up, `disable` calls
`teardown_package` on every Shard of the recorded topology, then once more on each Shard. On each
Shard, teardown does these steps:

- It empties the Package's tables and deletes its Package Config.
- It stops the Package's hooks and tick passes until a build without the Package runs.
- It makes the Package's Characters Dormant Characters. They go offline and lose their live
  entities, and their Accounts and Characters stay.

`disable` asks before the teardown, and `--yes` answers in advance. If a Shard refuses, nothing
moves. The usual Refusal is a Package Character that is crossing
between Shards. Re-run `disable` when the crossing settles. When the stack is down, `disable` moves
the folder without a teardown and reports the Package's tables. The next publish then stops if
those tables hold rows; enable the Package, start the stack, and disable it again.

The off path for a Package on a running Realm:

```bash
./lyracore packages disable playerbots   # asks, tears down every Shard, then moves the folder
./lyracore packages apply                # publish without its Rust and remove its artifacts
./lyracore client sync                   # only when the Package ships client content
```

`packages enable` followed by `packages apply` brings the Package back fresh: empty tables and default
Package Config. Its Dormant Characters stay as they are, and the Package creates new ones.

**`packages remove NAME` deletes, so it has gates.** It requires the Package to be disabled already,
and points at `packages disable` when it is not: the build has to stop compiling a Package before
the folder goes. It refuses a folder whose Content Identity no longer matches its Provenance Stamp,
and a folder with no readable stamp at all. Both are the same rule. This command may only delete
content that is recorded somewhere else, and local edits to an installed copy are recorded nowhere.
Save them outside the checkout first, or delete the folder by hand.

**None of the three publishes or synchronizes a client.** Each prints the steps it did not run.

## `packages update`

```bash
./lyracore packages update my-package        # advance one Package, asks first
./lyracore packages update                   # advance every Git or Official Package Source
./lyracore packages update --yes             # answer the questions in advance
```

Git Package Sources and Official Package Sources can be updated. With a name, other source kinds
are refused: a local folder has no remote revision to fetch, a scaffold contains code the author
owns, and an unknown source kind is not cloned. With no name, `update` selects both supported
source kinds from the enabled and disabled inventories.

A Git Package Source uses the recorded repository's current commit. An Official Package Source
uses the tag matching the checkout's Package API version and resolves the same Package name there.
A missing tag or Package refuses the update before changing the installed copy.

If the resolved commit matches the Provenance Stamp, there is nothing to do. A different commit
gets the same Trust Review and confirmation as an install, with both commits named.

**A folder that has drifted from its Provenance Stamp is refused, and nothing is discarded.** An
update replaces the whole folder, so it may only run when every byte in that folder is recorded
somewhere else. That is `packages remove`'s rule, for the same reason: local edits to an installed
copy are recorded nowhere, and neither command can get them back.

**The previous revision is kept until the new one is proven.** The old folder moves out of the
inventory and the new revision installs in its place. Enabled server-content changes run
`preflight`, including changes that remove server content. Disabled Packages remain disabled and
skip module validation, as do client-only changes. All candidates still undergo shape, source-drift
and Trust Review checks. The old folder is deleted only after the applicable checks pass. If anything fails, the previous revision goes back byte for byte,
the candidate is discarded, and the error names both commits. `update` publishes nothing and
synchronizes no client; it prints the steps it did not run.

Apply Package Deltas with `packages apply`, below.

## `packages config` — a Package's key-values, on every Shard

```bash
./lyracore packages config greeter                       # every key and its value
./lyracore packages config greeter greeting              # one value
./lyracore packages config greeter greeting "Hi there"   # write it to every Shard
./lyracore packages config greeter volume 3 --new        # create a key the Package never seeded
```

A Package Config row is one durable value a Package reads and the Operator edits. A Package seeds
its own defaults when it initialises, so the list shows real keys with live values.

**The rows are per-Shard state.** Every database of the fixture topology holds its own copy, and the
Module coordinates none of them. A write therefore goes to every Shard of the recorded topology, the
same set `packages apply` uses when no database is named. A read visits every Shard too: when they
do not agree on a key, the command names each Shard's answer instead of printing one of them. A
Shard with no row for the key reads as `(unset)`, which is the same kind of disagreement.

**The Module owns which keys exist.** Writing a key the Package never seeded needs `--new`, which is
the `allow_new` argument of `set_package_config`. Without it the Module refuses the write and names
the keys the Package does have; the command prints that refusal back unchanged. A Package name that
is not installed is refused before any Shard is read, with the installed list.

Reads go through `spacetime sql`, because `game_package_config` is a public table. The write calls
the Operator-gated `set_package_config` reducer over the same bearer-token path `dev up` uses to
claim the Operator, so the local realm has to be up and claimed.

**A failed write is not rolled back.** The command stops at the Shard that refused and names what
was written, what stopped it, and what was never touched. Re-running the same command after the
cause is fixed rewrites the Shards that already took the value, which changes nothing on them.

## `packages build` — Datascript typings, the typecheck gate, and artifact emission

```bash
./lyracore packages build
```

A **Datascript** is author-time TypeScript that describes game data. It is written against the
Module's own schema, so the names and types in it cannot drift from the Module. `packages build`
enforces that and, once a Package has a Datascript, turns it into a validated Package Delta. The
same command compiles a Package's **Runtime Scripts** into its Script Artifact.
The enabled source inventory determines the work: no sources means no Bun or schema generation;
existing artifacts are still validated and artifacts whose last Runtime Script source was removed
are retired. Runtime-script-only builds need Bun and the locked dependencies, but no schema
typings or Datascript typecheck. With Datascripts, the steps are:

0. `bun --version` must match the exact `engines.bun` pin in `datascripts/package.json`.
   A mismatch is a hard failure: the next steps run `bun install` and the locked `tsc`
   against the Bun on `PATH`.
1. `spacetime generate --lang typescript` extracts the schema **through the module wasm** and writes
   it to `datascripts/generated/`. Offline: it builds the module and reads it, and touches no
   database.
2. `bun install --frozen-lockfile` installs exactly what `datascripts/bun.lock` records. Frozen, so
   a build never silently resolves a newer dependency than the next author will get.
3. `tsc --noEmit` typechecks Core's Datascript project and each enabled Package's local Datascripts.
   The CLI extends Core's compiler configuration in a temporary file. No JavaScript is emitted.

Steps 1 and 3 are Datascript-only. Step 7 also validates source-free artifacts without a Bun
installation. Artifact production and identity recording continue as follows:

4. The Base Snapshot must already exist at `datascripts/generated/base-snapshot.json`, or the build
   fails fast with the exact `lyracore-importer --spell-snapshot` command to build one, once, rather
   than letting every Datascript fail with the same "cannot read" error in turn. Skipped when no
   Package has a Datascript: a Runtime Script reads no base data.
5. Each enabled Package runs `datascripts/src/<name>/*.ts`, then
   `packages/<name>/datascripts/*.ts`. Each directory runs in file-name order, with one `bun run`
   subprocess per file. The first failure stops the build.
6. Every enabled Package with an immediate `.ts` or `.lua` file in `scripts/` compiles its Runtime
   Scripts into one Script Artifact, one `bun run` subprocess per Package, in folder-name order.
   The builder is handed the
   Module's Event Binding catalogue, read from `lyracore-delta-check --print-events`, so a mistyped
   Event Binding fails with the file that holds it. Fail-fast, the same way step 5 is.
7. `lyracore-delta-check` traces every enabled Package's generated artifacts together, in one
   invocation, Package Deltas and Script Artifacts alike. This is the same authoritative Rust-side
   check `packages apply` runs before it writes to a Shard, so a Claim Conflict or a Runtime Script
   collision between two Packages is caught by the one implementation that also decides whether it
   may apply, not by a second, looser one.
8. A **Build Identity** sidecar is written next to each source-built artifact that just validated:
   the hashes `packages check` and preflight later recompute to tell whether it is still current.
   A source-free prebuilt Script Artifact has no local author inputs or sidecar; the authoritative
   checker still parses and traces it. Each kind records its own inputs — a Script Artifact's are
   its `scripts/` sources, optional Package-root `script-ids.json`, Runtime Script Toolchain and
   Bun pin.

A Package Delta this command emits is never committed: it is regenerated author-side on every
build and installed from source, the same way `datascripts/generated/` itself is git-ignored. A
Script Artifact is the one exception, since it is package-authored Lua with no client-derived
data, so a Package may commit it under `data/.generated/` to ship its Runtime Scripts. Commit
`script-ids.json` with the sources and Script Artifact so later builds keep the same IDs.

Generating **first** is what gives the gate teeth. Rename a column in the Module, run
`packages build`, and a Datascript still using the old name fails with the file and line to fix —
at author time, rather than at apply time.

Typechecking **before emission** carries the same reasoning one step further: a Datascript that
fails to typecheck should not run at all, so step 3 gates step 5 the way step 1 gates step 3.

The typings cover **core and installed Package tables alike**, and by construction rather than by a
second mechanism: `module/build.rs` compiles every enabled Package into the same module wasm, so a
table a Package registers is in the schema `spacetime generate` reads. Install a Package that
declares a table, re-run `packages build`, and its row type is in `datascripts/generated/types.ts`.

### The Datascript project

```text
datascripts/
  package.json     the checkout’s Bun, SpacetimeDB, and TypeScript pins
  bun.lock         committed — a fresh checkout resolves exactly these dependencies
  tsconfig.json    strict, noEmit
  src/             the Datascripts. `src/reference.ts` is the maintained reference Datascript
  generated/       written by step 1. NOT committed
  runtime-scripts/ the Runtime Script Toolchain — a Bun workspace, so one `bun install` covers both
```

`generated/` is git-ignored on purpose. It is a ~400-file, 2 MB projection of the module wasm that
`packages build` reproduces on every run. Committing it would put a large mechanical diff in every
schema change and create a second source of truth that can disagree with the Module. The Module is
the schema's authority.

`src/reference.ts` is the standing schema check. It names real columns, so it is the file that fails
when the schema moves under it. Keep it referencing real columns.

A Package can ship `packages/<name>/datascripts/welcome.ts`. It imports the authoring library as
`../../../datascripts/lib/index.ts`. The older `datascripts/src/<name>/` location still works.
The Build Identity covers both source directories, so editing either requires a rebuild.

### Bun is author-side only

`packages build` needs Bun only when enabled Package sources need compilation. `packages apply` runs that build when installed sources need new
artifacts. **An Operator applying a prebuilt Package needs no Bun and no Node.** Nothing in
`dev up`, `preflight`, `publish` or `client sync` invokes a JavaScript toolchain. That is why
`doctor` reports a missing or different Bun version as a warning and never as a launch blocker.

The exact version comes from `engines.bun` in the checkout's `datascripts/package.json`.
`doctor` and `packages build` print an install command for that version. Build Identities record
the same pin, so changing it makes existing source-built artifacts stale until rebuilt.
`packages check` reads the pin from disk and does not need Bun installed.

Datascripts are **trusted author-time code**, run from this checkout by the person who wrote them.
They are not sandboxed and are not described as sandboxed. `packages build` above is what turns one
into a Package Delta; `packages apply`, below, is what applies it to a Shard.

### Runtime Scripts — `packages/<name>/scripts/`

A **Runtime Script** is the opposite kind of code: Lua the Module runs *on the realm*, inside the
Runtime Script Host, with a Fuel Budget and no access to anything the Host did not hand it. Write
one in TypeScript or in Lua:

```text
packages/fire_nova/
  script-ids.json          stable IDs assigned by the Runtime Script Toolchain
  scripts/
    ember_echo.ts    compiled by the Runtime Script Toolchain
    bonus.lua        compiled for the author who writes Lua
  data/.generated/
    fire_nova.script.json   the Script Artifact. Committed in the Official Package Collection
```

Runtime Script sources live in the Package's `scripts/` directory. Author-time Datascripts may
live beside them in `datascripts/`.

Each source declares an ordinary named function and registers it for one typed Event Binding:

```ts
function welcome(event: PlayerLoginEvent): void {
  send_chat(event.player, "Welcome to the realm.");
}

events.player.onLogin(welcome);
```

Lua uses the same authoring shape:

```lua
local function welcome(event)
  send_chat(event.player, "Welcome to the realm.")
end

events.player.onLogin(welcome)
```

Each file has one Event Binding. Registration selects the function the Invocation calls with its
typed event. The toolchain captures registration in a local wrapper. It does not add Event Bindings
at runtime. The generated Lua returns the function's Script Answer.

The Runtime Script Toolchain assigns stable numeric IDs in the Package Script Range, 100,000 to
999,999. It writes them to the Package-root `script-ids.json`. Keep that file and commit it with
sources and the Script Artifact. Removed entries stay in the ledger so a later source does not
reuse their IDs. The Runtime Script name is `<package>.<file stem>`, so renaming the handler keeps
its name and ID.

The builder still accepts legacy `@event` and `@id` Script Directives for migration. It preserves
IDs from those directives or an existing Script Artifact with the same Runtime Script name. New
sources use typed registration and let the toolchain manage IDs. Optional `@priority` and
`@enabled` directives default to 0 and true. Use `//` comments in TypeScript and `--` in Lua.

`datascripts/runtime-scripts/runtime-script.d.ts` declares the typed events, Entity Handles and
Host Operations. `runtime-script.lua` provides Lua editor declarations from the same catalogue.
The Module owns the event catalogue, and the build reads it from
`lyracore-delta-check --print-events`.

The emitter rewrites one call shape. piccolo 0.3.3 passes an inline table constructor's element
count as an extra argument when the constructor is the last argument of a call, so `f({7, 8, 9})`
arrives as `f(table, 3)`. Transpiler output meets that shape constantly. Every emitted file
therefore opens with `local function ____tbl(t) return t end` and a trailing constructor is passed
through it. `module/src/runtime_script.rs` pins both the fault and the fix. A hand-written `.lua`
script must avoid that call shape.

Syntax errors and failures raised through `error`, `assert`, or a Host Operation name a line of the
**generated** Lua, the bytes the Shard holds, never a line of the TypeScript. There is no source
map, and a number pointing into a file no Shard holds would be worse than no number. Piccolo 0.3.3
does not expose a frame after a native VM fault unwinds or when the Host stops an unfinished fuel
step. Those diagnostics name the Runtime Script, Event Binding, failure kind, and interpreter
message, but no line. Adding generated-code instrumentation would change emitted Lua and Fuel
Budget accounting, so this Host does not guess.

Build one Package's scripts by hand the way `packages build` does:

```bash
bun run datascripts/runtime-scripts/build-scripts.ts fire_nova
```

The Official Package Collection commits sources, `script-ids.json` and Script Artifacts together.
An Operator can install prebuilt Lua without Bun. In a Core checkout, Script Artifacts are
git-ignored output and Build Identities detect changes to their author inputs. Keep
`script-ids.json` with the Package sources.

## `packages apply`

Native checkouts default to the single `lyracore` Shard. Package reads, Module publication,
schedule repair and artifact application use `.lyracore/native-cli.toml`. The local Native Host
must be recorded and running before the command starts. This command targets the local node on
port 3000; it does not inherit `publish --server` from another invocation.

Prepare and activate the installed Packages on a running Realm:

```bash
lyracore packages apply [DATABASE ...] [--check] [--yes] [--force-all] [--client-data PATH]
```

With no Shard names, native checkouts use `lyracore`; legacy checkouts use the recorded development
topology. Named Shards are used exactly as given. It never infers a production Shard list. Run it
from the checkout whose Module and Packages you intend to deploy, with an Operator identity on
each target.

The command checks Build Identities and builds missing or stale artifacts from installed sources.
A Datascript needs the pinned Bun version and your own client data. If its Base Snapshot is missing,
`apply` extracts one locally. Use `--client-data PATH` or `lyracore config set client-data PATH` to
select the client's Data directory. A current Script Artifact needs neither Bun nor client data.
Source-free Package Deltas must already have a current Build Identity; install updated source or
artifacts if they are stale. Generated Package Deltas remain local.

Before writing, the command validates artifacts, checks conflicts, reads every target's provenance,
and asks once for the planned Realm changes. `--yes` answers that confirmation in advance.
`--check` prepares local artifacts and validates the plan without publishing or calling a reducer.
Local generated files can change during a check.

If the Package Inventory contains Rust, or a target records pending Package Teardown, `apply`
publishes this checkout's Module. It runs preflight and repairs schedules on each published Shard
before continuing. Legacy checkouts also run the Loot Roll upgrade check. Rust Packages cause a
publish on every run, including disabled Packages whose code may still be deployed after an offline disable;
artifact provenance does not track compiled Rust. Disable a Rust Package through
`packages disable` before removing it so its tables and Characters receive Package Teardown.

Each Shard completes its publish and schedule repair, when needed, before applying artifacts.
Only then does the command continue to the next Shard:

- The spell Import Family reimports `Spell.dbc`, then applies the enabled Package Deltas.
- The script Import Family reconciles `game_script` to the enabled Script Artifacts in one
  transaction. A script-only run needs no base import or Module publish.

A family whose Package Import records already match the enabled artifacts is skipped. For spells,
the records must also match the Shard's current base import. `--force-all` applies both families even
when they match. An empty enabled set removes previously applied Package spells and Runtime Scripts;
the confirmation names these removals.

A failure stops the run and reports completed and remaining work. Retry the printed command after
fixing the cause. Completed artifact families are skipped. A Module publish can disconnect clients;
a failed schedule repair names the published Shard and the repair command.

Other claim families use the importer's world-dump modes. This command owns spell and script
artifacts only. Client content is installed separately with `lyracore client sync`.

The old name, `packages replay`, is refused with a migration hint. It is not an alias because
`apply` can build sources and publish Rust Packages.

## `packages check` — is every generated artifact still current?

```bash
./lyracore packages check
```

`packages build` writes a Build Identity next to each source-built artifact. `packages check`
recomputes every recorded input from the checkout on disk right now and refuses, naming the
specific input, the moment one no longer matches. `preflight` folds the same report into its own
gate on `publish`'s behalf, so a stale artifact never reaches a Shard.

A Package Delta always has a sidecar. A source-built Script Artifact has one for its `scripts/`
sources, optional Package-root `script-ids.json`, Runtime Script Toolchain, and Bun pin.
A source-free prebuilt Script Artifact has no local author inputs or sidecar. `packages check`
still sends every Script Artifact through the
authoritative parser and tracer, so a malformed or conflicting prebuilt artifact never passes.

`datascripts/generated/` is regenerated fresh, every run, with the same `spacetime generate`
invocation `packages build`'s typegen step uses, so a Module schema change makes a committed
artifact stale even on a clean checkout that never ran `packages build` itself. The typings are an
input of a Package Delta alone, so a checkout shipping only Runtime Scripts skips that step and
builds no module wasm. Nothing else is regenerated: this command never runs Bun and never re-emits
anything, so it needs neither Bun nor a Base Snapshot to do its job.

A missing Base Snapshot is reported and does not fail the check: the snapshot is the Operator's own
client-derived data, and a CI machine holding none cannot regenerate one to compare against. A Base
Snapshot that is present and no longer matches its recorded hash is a real mismatch and fails like
any other input. A missing sidecar is stale for a Package Delta or a Script Artifact with sources.
For a source-free Script Artifact it is the prebuilt contract, not a skipped parser check.

A checkout with no Packages at all, or none carrying a generated artifact, is a clean no-op.

`packages check` runs against a Package Delta sitting uncommitted in your own checkout: only a
Script Artifact is ever committed, so that is where the drift responsibility lives too — an
author regenerates and re-checks locally, and the Official Package Collection's CI refuses a
Package Delta it finds committed rather than checking it.

## UI Transforms — a Package's edit inside a stock UI file

A Package's `client/mpq/` tree replaces a stock file whole. When two Packages need the same file,
that does not work: one of them has to own it. A UI Transform is the other way in. It is an
anchored edit, declared in `packages/<name>/client/ui-transforms.json`, and several Packages may
edit one file as long as their anchors do not overlap. `client-patch/ui-transforms.json` works the
same way for a checkout-wide edit.

```json
[
  { "path": "Interface/FrameXML/LootFrame.lua",
    "after": "function LootFrame_OnLoad()",
    "insert": "\tPkgLoot_OnLoad();\n" },
  { "path": "Interface/FrameXML/FrameXML.toc",
    "before": "LootFrame.xml",
    "insert": "PkgLoot.lua\nPkgLoot.xml\n" },
  { "path": "Interface/GlueXML/GlueXML.toc",
    "replace": "AccountLogin.xml",
    "insert": "AccountLogin.xml\nPkgGlue.lua\n" }
]
```

Each entry names one `path`, one anchor, and the `insert` text. `path` accepts either slash
direction, must sit under `Interface/FrameXML/` or `Interface/GlueXML/`, and must end in `.lua`,
`.xml` or `.toc`. The anchor is exactly one of `before`, `after` or `replace`. Its text must occur
exactly once in the Baseline: zero occurrences refuses as "anchor not found", several as an
ambiguous anchor, and both name the Package, the file and the anchor text.

`client sync` resolves every anchor against the untouched Baseline before it applies anything, then
applies the edits in the order their anchors appear in the file. The composed result therefore does
not depend on which Package the walk reached first. Two edits whose anchor ranges intersect have no
correct merge and refuse, naming both Packages. A path one Package overrides whole from its `mpq/`
tree while another edits it by transform refuses the same way: a file is replaced or patched, never
both.

The Baseline comes out of your own client's UI archives, read in load order: `interface.MPQ`,
`<locale>/locale-<locale>.MPQ`, `patch.MPQ`, `patch-2.MPQ`, `<locale>/patch-<locale>.MPQ`,
`<locale>/patch-2-<locale>.MPQ`. Archives your client does not have are skipped, and the not-found
message lists every one searched. `patch-3.MPQ` is never read: it is the packer's own previous
output, so composing against it would apply each edit again on every run. The composed file carries
a header comment naming the Baseline hash and the transform hash, so the same client and the same
declarations rebuild byte-identical output.

That output is your client's own bytes with the edits in them, which makes it baseline-derived. It
reaches your client through `client sync` alone. `client pack` refuses a checkout that declares any
UI Transform at all, and names the Package and the file it declared.

## `client sync` — push client content to your own client

```bash
./lyracore config set client-data /games/WoW-1.12.1/Data   # once, if `import` hasn't already
./lyracore client sync
```

A thin wrapper around `lyracore-importer --pack-client <client Data/ dir> --apply` (core repo,
`importer/src/pack_client.rs`): it builds `patch-3.MPQ` from `client-patch/` plus every enabled
Package's `client/` directory, installs the addons into `Interface/AddOns/`, and clears the `WDB/`
cache — so a change to a Package's client-side UI reaches your own client in one command. Refuses
before touching anything if `config set client-data` was never run, naming that command as the fix.

Collision and licensing-firewall failures — two Packages shipping the same archive path or the same
addon name, or a raw `.dbc`/`.MPQ` committed where only our own assets belong — are caught before
any file is written to your client, and name both sources.

There is no managed-content ledger and nothing here deletes an addon. When a Package that used to
ship an addon is disabled or removed, `client sync` warns (best-effort) that the addon it installed
earlier is still sitting in your `Interface/AddOns/` and names the Package — removing it is your
call, by hand. An addon `client sync` never installed (yours, or a third party's) is never flagged.

This is also where a declared UI Transform is composed and packed, against the Baseline in your own
UI archives.

Packaging any of this for someone other than the Operator running the command is `client pack`'s
job, below. It packs strictly less, because a baseline-derived file never leaves this machine.

## `client pack` — build the artifact a player installs

```bash
./lyracore client pack --out ./client-pack
./lyracore client pack --out ./client-pack --zip
```

`client sync` fills your own client. `client pack` builds the Client Artifact instead: a directory
tree a player copies over a stock 1.12.1 install.

```text
<DIR>/
  lyracore-client-pack.json     the manifest, written last
  Data/patch-3.MPQ              only when a source ships at least one mpq/ file
  Interface/AddOns/<Name>/
```

Under it sits `lyracore-importer --pack-out <DIR>` (core repo, `importer/src/pack_client.rs`),
which collects the same sources `client sync` collects and opens no client at all.

The licensing firewall holds by provenance. Only package-authored bytes, the ones an author
committed under `client-patch/` or `packages/<name>/client/`, may enter the artifact. A DBC overlay
and a UI Transform output are both computed from the Operator's own client, so `--pack-out` refuses
each by name and writes nothing. That is why an artifact's `Data/patch-3.MPQ` carries no DBC while
the one `client sync` installs does.

`--out` resolves against the checkout root when it is relative. It is refused inside the configured
client-data path and inside `packages/`. A directory that already holds files and no
`lyracore-client-pack.json` is refused as well, because this command did not create it. A directory
that does hold that manifest is a prior artifact, and it is cleared before the repack.

The manifest is written last, so its presence means the artifact is complete. It records `format`
(`1`), `packed_at` (UTC, RFC 3339), `core_revision` (`git rev-parse HEAD`, or `unknown` when that
fails), one `packages` entry per enabled Package (`name`, `source_kind`, `source`, `revision` from
its Provenance Stamp, and a `content_identity` computed fresh at pack time), and `contents`, every
packed file's relative path, sorted.

`--zip` also writes `<DIR>.zip`, by running the system `zip` binary with `<DIR>` as its working
directory. Neither repo carries a zip library, so a machine without the binary gets that named as
the reason, and keeps the directory that was already built. A `<DIR>.zip` this command did not
write is refused rather than overwritten.

## `bindings`

Run `./lyracore bindings` before a Gateway Cargo build or editor check on a fresh Core checkout.
It generates private Rust bindings from the local Module with `debug_reducers`, using the pinned
Rust and SpacetimeDB tools. Schema generation and CLI publishing take Rust flags from the
checkout's Cargo configuration and ignore ambient Rust-flag overrides. It does not contact a Realm. `dev up` prepares them before its Gateway
build, and the publish upgrade check reads the prepared schema.

Files live under `.lyracore/generated/gateway/`, outside the source tree. Repeated preparation
reuses output while source and generated-file hashes match. Module and workspace sources, installed
Package sources, the Runtime Script event catalogue, Cargo configuration and lockfile, and tool
pins determine freshness. A failed generation leaves the previous complete output in place.

`./lyracore bindings --check` checks existing output without running Cargo or SpacetimeDB and
prints JSON with `directory` and `watches`. Core's Gateway build script uses that contract through
the installed pinned CLI. Missing or stale bindings stop the build with preparation instructions.
Deleting `.lyracore/generated/gateway/` clears the generated cache, including old directories kept
for compilers that were already running.

## `preflight` — the offline deploy gate

```bash
./lyracore preflight
```

The same five checks the maintainers' internal preflight script runs, in the same order, with the
same fail-every-check-then-report behaviour, and it touches **no node** — no publish, no call, no
sql, so it is safe against a live stack:

0. `rustc` and the `spacetime` CLI **exactly** match the versions this checkout pins
   (`rust-toolchain.toml`, `module/Cargo.toml`). Exact, not a floor — `doctor` asks for a minimum,
   a deploy gate cannot, because a CLI *ahead* of the pin publishes a schema this repo never tested
   against. A missing or drifted CLI is a hard failure but does **not** skip checks 2–3.
1. the module builds with `--features=debug_reducers` — the feature a publish bakes in and the
   default test config never compiles.
2. real offline wasm schema extraction (`spacetime generate` into a scratch directory that deletes
   itself). `PREFLIGHT_SKIP_SCHEMA=1` still skips it where `spacetimedb-standalone` is unavailable.
3. every `#[client_visibility_filter]` names real tables and columns. Ported to Rust from an
   internal Python script, so **`python3` is no longer a prerequisite**; the port was verified
   differentially against the Python over the real module (same verdict and same message on every
   mutation tried).
4. a script with a configurable `DB` target threads it into every tool it drives.

## `publish` — the one correct deploy

```bash
./lyracore publish                                    # every database of the active fixture topology
./lyracore publish lyracore lyracore-world-1 realm-core   # several shards, in order
```

In legacy checkouts, omitting names publishes every database in the recorded fixture topology.
An active single fixture has one database. An active sharded fixture uses all of its databases.
Without a recorded fixture, the command uses the default sharded topology. Explicit names select
exactly those databases, in the order given.

Legacy checkouts render `spacetime publish -s local -p <checkout>/module
--build-options=--features=debug_reducers --yes <DATABASE>`. The command runs `preflight` first,
then publishes databases in order and stops at the first failure.

Database names cannot start with a flag. The CLI refuses destructive wipe flags and unknown
options before starting a process. `--skip-preflight` skips offline checks. Legacy checkouts still
prepare bindings and check Loot Roll upgrade readiness.

Native checkouts publish one Shard named `lyracore` by default, using
`.lyracore/native-cli.toml` for the publisher identity. They do not run the legacy multi-Shard
upgrade checks. To target a separate native node on the same machine, use:

```bash
./lyracore publish --server http://127.0.0.1:13000
```

`--server` requires a native checkout and its private publisher configuration. The URL must use
HTTP, a numeric loopback address and a nonzero port. It cannot include credentials or a path.
Without this option, native publishing targets `http://127.0.0.1:3000`. All native publishes
require the private publisher configuration before any command runs. Publishing to port 3000
requires a recorded Native Host with a matching live process identity and an answering endpoint.
A stale `native_runtime` flag is insufficient. Stop recorded legacy processes with `dev down`,
then use `dev up` to create the native Realm. An explicit target on another port can use an isolated
native node while the legacy runtime remains recorded.

[`docs/danger-zones.md`](https://github.com/LyraCoreProject/LyraCore/blob/main/docs/danger-zones.md)
remains authoritative for deployment and data migrations.

## `production status` — read-only evidence for a named topology

```bash
./lyracore production status \
  --server local \
  --gateway-log /tmp/gw.log \
  --realm-core lyracore-realm \
  lyracore lyracore-world-1 lyracore-instances lyracore-realm
```

This command does not infer production from the contributor fixture. The server, log path,
realm-core, and complete database set are required. The server value is forwarded unchanged to
SpacetimeDB inventory and schema probes. The command checks that every named database is reachable,
isolates the latest gateway-start segment, compares configured and expected topology, requires a
distinct coordinator connection per database, and verifies realm-core plus logon/world listener markers.
Address and missing-occupancy signals are warnings; unreachable databases, missing connections,
startup errors, or missing listeners fail the command. It performs no publish, reducer call, SQL
write, or service action. It is the canonical log parser for the production runbook; verify the
operating system's actual sockets separately.

Runtime files live in the git-ignored `.lyracore/` — `state.json` for the processes the CLI started,
`logs/{spacetime,gateway}.log`, and `coordinator-token` (mode `0600`) if this host had no
`spacetime login` and the CLI minted a local identity for it.

## Native Host startup

A Core checkout with `native-host/prepare-host.py` selects the native runtime. `dev up` builds
that pinned host, starts its HTTP endpoint on `127.0.0.1:3000`, and opens the logon and world
listeners on ports 3724 and 8085. `--lan` changes only the two game listener addresses. The host
owns all three listeners. No Gateway process starts.

Startup runs preflight, publishes one Shard named `lyracore`, claims the Operator, installs its
GUID range and provisions the Realm address. It checks for an actual WoW world authentication
challenge before reporting success. Publishing and Operator calls use the same retained identity.
The CLI reuses the retained native credential first, then a SpacetimeDB login if available, or
mints a local identity. It writes the credential
and publisher configuration to `.lyracore/native-owner-token` and `.lyracore/native-cli.toml`,
both with mode `0600`. Native publishes target the explicit loopback URI.

Native startup requires OpenSSL 3 or newer on PATH. Before starting the Host, the CLI creates a
persistent P-256 JWT key pair in `.lyracore/native-jwt`. The directory has mode `0700`, and both
key files have mode `0600`. Restarts reuse the private key and compare its derived public key to
the retained one. A missing public key is derived from the private key. A public key without its
private key, corrupt key material or a mismatched pair stops startup without replacing existing
keys. The Host receives this directory through `--jwt-key-dir`; a certificate authority already
set in `native-data/config.toml` retains precedence.

`doctor` checks the same OpenSSL requirement for native checkouts. A missing command, LibreSSL,
or an unsupported version is a blocking failure with a link to the installation instructions.
The check reads the version only; it does not create or change signing keys.

Native state lives in `.lyracore/native-data`; startup creates a new local Realm there. It does
not merge data from the old Shards. Stop a recorded legacy runtime with `dev down` first. An
unrelated process on any required port causes startup to stop before publishing. The CLI records
the Native Host before publishing, so `dev down` can still stop it after a later step fails.

`publish` without names, content imports and Character operations select the single Shard for
native checkouts. `account create` computes the SRP salt and verifier locally and calls the Module
over authenticated loopback HTTP. It does not build or invoke a Gateway binary. The password stays
out of process arguments and HTTP requests.

`dev status` reports the host and its listeners. `dev logs` shows the host log, and `dev down`
stops the recorded host. Repeating `dev up` reuses a healthy native runtime. Changing its listener
addresses requires `dev down` first. `--single` remains accepted and has no effect on native startup.
The Native Host runs in a separate process group. Ctrl+C during startup leaves it running; use
`dev down` to stop it.

## Development reload

```bash
./lyracore dev watch
```

`dev watch` starts or reuses a loopback Native Host with `LYRACORE_NATIVE_DEV=1`. It requires
`native-host/dev-reload.patch` in Core and the checkout's pinned `spacetime` CLI. An existing normal
Host causes a refusal with `dev down` advice. Watch never restarts that Host. Normal `dev up`
keeps its existing startup path and preserves Development reload when it reuses a running Host.
`.lyracore/state.json` records this mode as `native_hot_reload`; older state defaults to false.

The first watch build uses the development artifact, including on a fresh local Realm. It does
not run the release preflight or build the Host for each edit. It runs this build from the Core
root with `CARGO_TARGET_DIR=.lyracore/dev-reload-target`:

```text
cargo rustc --locked --lib --crate-type=cdylib -p lyracore-module --target wasm32-unknown-unknown --profile dev-reload --features=debug_reducers
```

Core owns the incremental `dev-reload` profile. Build scripts and procedural macros use
optimization level 3. The watcher emits only the WASM library; Rust tests retain the normal
library build through their own commands. Watch publishes the resulting
`wasm32-unknown-unknown/dev-reload/lyracore_module.wasm` from that target directory using the
retained native publisher identity. Its token must match the private `native-cli.toml` written
at startup. It sends one authenticated request with the WASM bytes:

```text
PUT http://127.0.0.1:3000/v1/database/lyracore?host_type=Wasm&clear=false&policy=Compatible
```

This skips the CLI's separate pre-publish request. The Host checks the definition while applying
the update. Inside the transaction it checks that the installed program hash has not changed.
It never clears existing state. The HTTP
client accepts literal loopback addresses only, disables proxies and redirects, and gives publish
up to 180 seconds. A compatibility or authorization refusal reports the Host's reason. A 5xx,
lost response or invalid confirmation reports an unknown outcome. Watch does not retry that
request automatically.

The Native Host requires exact equality of the complete normalized Module definition during
update planning and transactional update. This includes more than table layouts. Schema changes,
including a change to the `debug_reducers` feature set, require a separate explicit migration
workflow. The first publish checks any Module already present in `native-data`. Watch forwards no
extra publish flags and never selects a remote target or deletes content.

Watch polls Rust source and Cargo manifests in the Module and its resolved local dependencies.
It also watches enabled Package Rust source, `build_support` inputs, the Runtime Script
`events.json`, Cargo configuration and Rust toolchain files. It ignores build output, hidden
inventories and dependency caches. Package symlinks work; canonical paths prevent directory cycles
and aliases into build output from entering the watched inputs.

Watch debounces edits for 400 ms and runs one build and publish at a time. A source change observed
during compilation discards that candidate and builds the latest source after it settles. A change
during publish schedules another build. Every successful build publishes its artifact. The Host
handles identical WASM as a no-op, so a manual publish cannot make watch skip a later build's artifact.
Build, publish and total elapsed times appear for each attempt.

A compile failure sends no publish and leaves the current Module running. Watch waits for another
edit and tries again. A failed publish also leaves watch running. The command's output gives the
actual reason, including definite compatibility refusals. If the HTTP connection is lost, the
outcome can be unknown; inspect `dev logs` before assuming which Module is active. The Host's
compatibility check and transaction checks remain authoritative. Watch does not run repair or
provisioning reducers for each reload.

For a local Module definition change, stop watch with Ctrl+C, then run:

```bash
./lyracore dev down
./lyracore dev up
./lyracore dev down
./lyracore dev watch
```

Normal `dev up` attempts the standard SpacetimeDB migration without Development reload. If that
publish is refused, perform the required manual migration before returning to watch. Package changes
that publish a new Module definition require the same procedure. Package changes that keep the
definition can use the normal apply workflow.

One watcher can hold `.lyracore/dev-watch.lock` per checkout. Ctrl+C stops the watcher and its
foreground build or publish. The Native Host runs in a separate process group and stays running.
Use `dev down` to stop it.

## Startup for older Core checkouts

1. Starts SpacetimeDB on `127.0.0.1:3000`, **or reuses one already listening there.** A node the CLI
   did not start is never recorded and never stopped by `dev down`.
2. Builds the gateway.
3. Runs `preflight`, then publishes **every database in the recorded topology** — all four of the
   default sharded fixture, or just `lyracore` under `--single` — through the same internal command
   `lyracore publish` uses — which is what guarantees `--features=debug_reducers`, `--yes`,
   `-s local`, and the unreachability of a `-c` wipe. No path here renders a `spacetime publish` any
   other way, clears a database, or re-selects the SpacetimeDB server. A checkout the gate rejects
   is not published and no gateway is started against it.
4. Resolves the **coordinator credential** (below), minting one from the local node if this host has
   no SpacetimeDB login.
5. Calls `claim_operator` as that identity (idempotent for the same identity, so repeating `dev up`
   is not an error).
6. Starts the gateway, with the same credential, bound to loopback.

Re-running `dev up` on a healthy stack does nothing. On a partially-up stack it starts only the
missing part.

### The coordinator credential — no spacetimedb.com account required

The gateway's coordinator connection reads the **private** `game_account`/`game_session` tables and
calls the operator-gated `provision_account`, so it must authenticate as the identity that claimed
the operator. Without that it starts, warns, and dies ~15s later on `coordinator subscriptions not
applied within 15s`, which reads like a node fault rather than a credential one.

`dev up` takes the first credential that exists (#297):

1. **`.lyracore/coordinator-token`** — one this CLI minted for this checkout earlier. It wins,
   because it is the identity that already claimed the operator; `claim_operator` is TOFU and
   refuses a different one, so preferring anything else here would lock the checkout out of its own
   database.
2. **`spacetime login show --token`** — an existing SpacetimeDB login is reused, and nothing is
   minted or copied into the checkout.
3. **`POST /v1/identity` on the local node** — a *server-issued* identity, persisted at mode `0600`
   for rung 1.

Rung 3 is what keeps `git clone && ./lyracore dev up` anonymous: `spacetime login` offers only the
spacetimedb.com browser flow, and this fixture never needs it. The claim then follows the
credential — a login token *is* the `spacetime` CLI's identity, so that path still shells out to
`spacetime call`; a server-issued one is sent to the node's HTTP API with a bearer header, because
shelling out would claim the operator for the CLI's identity while the gateway ran as the minted
one.

`account create` walks the same ladder **without** rung 3: a freshly minted identity has claimed
nothing, so provisioning would be refused *after* the password had been read. It refuses first and
names `dev up`.

The credential reaches a child as an environment variable, never argv, and the node as an
`Authorization` header, never a URL — so it is absent from rendered commands, logs, errors and
`state.json`. Its only on-disk copy is the `0600` file above.

A fresh Module creates configuration and runtime schedules. `dev up` claims the Operator on
each Shard, repairs schedules, provisions the Realm advertisement on the default World Shard,
and starts the Gateway. It creates no Accounts, Characters, or playable content.

Provision your Account, import your world, and check content before play:

```bash
./lyracore account create ALICE
./lyracore import
./lyracore dev check
```

`dev check` asks each World Shard and Instance Pool to check its completed import and actual
content dependencies. It reports missing content with the Module's diagnostic. Realm-core
needs no spatial content. The command changes no content and is separate from offline
`preflight` and the process status in `dev status`.

Use `./lyracore import check --profile-shard PROFILE=SHARD ...` to check the same explicit
assignments you supplied to `import world`. The import clears prior completion before each
destination's first mode. It completes that destination only after all modes, schedule repair,
profile floors, and dependency checks pass. A failed import stays unavailable for play.

### `--lan <IP>` — let another machine on your network in

```bash
./lyracore dev up --lan 192.168.1.50     # one of YOUR machine's private addresses
```

This binds the two **client-facing** listeners — logon 3724 and world 8085 — to that address, and
makes the realm list advertise it (`LYRACORE_REALM_ADDRESS`, read by `gateway/src/config.rs`). Both
The CLI provisions `game_realm` with the address of the client listener.

**SpacetimeDB does not move.** It stays on `127.0.0.1:3000` in every mode. The address must be
private (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`); a public address or `0.0.0.0` is a usage
error, not a wildcard bind. A running gateway cannot be rebound — switching modes is refused, with
the `dev down` to run first, rather than reported as "already up".

LAN mode supports local development. It uses the Accounts and passwords you provision.

### `dev smoke`

```bash
./lyracore dev check
./lyracore dev smoke ALICE Ginger
```

Smoke checks the local World's content before it builds or runs the pinned Headless Client.
It reads the Account password from a hidden terminal prompt. Use `--password-stdin` to supply
one line through stdin. The password stays out of command arguments, logs and runtime state.

The CLI clones the release tag from `.wire-harness-rev` into `.lyracore/wire-harness/<sha>/`
and verifies its commit. It builds `vanilla-wire` from that checkout, then runs login,
Character enumeration and world entry with your chosen Account and Character. If the named
Character is missing, the client creates a Human Warrior using imported starting content.
It performs no fixture restoration or automatic Account provisioning.

`LYRACORE_WIRE_HARNESS_DIR=/path/to/wire-harness` selects a local checkout. The CLI reports the
override each time. Smoke requires running local processes and uses the recorded client
address, including LAN mode.

### Sharded out of the box, on purpose

**This reverses a documented decision** (#327). This section used to read "One database, on purpose",
and the fixture used to *unset* the topology variables so a contributor could not end up with a
gateway pointed at databases the CLI never published. That was the right call for "get a client
connected" and the wrong call for "show what this project is": a visitor who followed the quickstart
never met the sharded topology at all, and sharding is the thing that makes LyraCore different.

> **The fixture shrank on 2026-08-08 (#471):** the region tier — and with it the `lyracore-elwynn`
> region shard and the Northshire Valley | rest-of-Elwynn seam the quickstart used to walk — was
> removed from the codebase. The fixture keeps the broad splits: the continental shard map and
> realm-core.

> **The instance pool joined on 2026-08-11 (#108):** for the reason the region shard left. It is a
> production tier a fresh clone could not exercise at all, so instance routing was the one split
> nobody developed against.

`dev up` brings up a **four-database** fixture, one per production tier in
[`architecture.md`](https://github.com/LyraCoreProject/LyraCore/blob/main/docs/architecture.md) §3.1, all of it published and health-checked by the CLI
itself:

| Database | Role |
| --- | --- |
| `lyracore` | default World Shard for imported Eastern Kingdoms maps and the Realm advertisement |
| `lyracore-kalimdor` | world shard for map 1, reached via a `LYRACORE_SHARD_MAP` rule |
| `lyracore-instances` | the instance pool — every dungeon run (map 36, the Deadmines), reached by a second rule of the same shape |
| `lyracore-realm` | realm-core — accounts and sessions, the character→shard index, load samples |

So a fresh clone has two live splits: the **Eastern Kingdoms | Kalimdor** continental divide,
crossed by the escrowed cross-database transfer rather than by walking, and the **open world |
instance** one, crossed at a dungeon portal.

Both shard-map rules are the same shape (`<map>:*=<db>`), because an instance map routes exactly
like a continent one. The bucket half of a rule exists to spread ONE map's instances over a pool of
several databases; a one-database pool does not need it, and map 0 is named by neither rule —
Eastern Kingdoms stays whole on the default database.

Three of those four names are production's own, and deliberately: what keeps a fixture off a
production node is the node it is published to (`-s local`, loopback:3000), never the name.

All World Shards and the Instance Pool require `./lyracore import` with public world data and
your own client MPQs before play. The import assigns `alliance-eastern` to `lyracore`, `alliance-kalimdor` to
`lyracore-kalimdor`, and the instance-only `instances` profile to `lyracore-instances`. The single
topology receives their union in `lyracore`. Each destination is verified independently after its
clear-and-reload stages, so rerun the command after fixing a failure.

⚠ **Dungeon populations still spawn on the world shard until you say otherwise.** Routing map 36 off
`lyracore` does not move the spawning with it: `game_config.hosts_instances` is a per-database flag
that defaults to on everywhere, so the gateway logs one WARNING per start saying every entry spawns
~207 creatures on the world writer and evicts them again after the transfer. The run works. To model
the production split, turn the flag off on the world shard once — it survives a republish:

```bash
spacetime sql -s local lyracore "UPDATE game_config SET hosts_instances = false WHERE id = 0"
```

Leave it alone under `dev up --single`, where the one database has to host its own dungeons.

The CLI still *owns* the topology rather than inheriting it: the variables above are set to the
fixture's own values for the child gateway, so a contributor with the production recipe exported in
their shell gets the fixture, not a four-database gateway pointed at production database names. And
`dev status` / `doctor` report **every** fixture database, not just the default — a partial publish
presents as an unrelated mid-session hang (`docs/danger-zones.md` §3), which is a brutal first
experience to debug.

> ⚠ **A schema change now means republishing every fixture database, not one.** `./lyracore publish`
> covers the set; the same rule the production realm has always had (`docs/danger-zones.md` §1.2)
> now applies locally, because the local realm is now genuinely sharded. Republishing only
> `lyracore` after a migration leaves the other three on the old schema.

**`dev up --single` is the escape hatch.** It publishes and runs `lyracore` alone, with
`LYRACORE_SHARD_MAP`, `LYRACORE_SHARD_MAP_FILE` and `LYRACORE_REALM_CORE`
unset — per `gateway/src/config.rs`, an unconfigured shard map collapses every lookup to
`LYRACORE_DATABASE`, so the result is byte-identical to a single-database build. Reach for it when
you are debugging something that is not about sharding, when RAM is tight (four databases cost more
than one), or to establish whether a bug is a sharding bug at all.

## `character gm` — grant or revoke GM commands

```bash
./lyracore character gm NAME true    # level 3 — unlocks the .commands dot-command kit
./lyracore character gm NAME false   # level 0 — revokes it
```

Calls the operator-gated `set_gm_level` reducer, authenticated with the same coordinator credential
`dev up` resolved (§"The coordinator credential" above) — never `spacetime call`, because that
shells out as the `spacetime` CLI's own identity, which is not necessarily the one that claimed the
operator. If the checkout has no coordinator credential yet, it refuses and names `dev up`, the same
remedy `account create` gives for the same problem.

Characters live on **world shards**, never realm-core, so it walks the shards in topology order
(just `lyracore` under `--single`) and calls each in turn, stopping at the first one that has a
player by that name. A miss on every shard is reported once, aggregated, rather than once per shard.

## `sql` — read and edit the local Realm's database

```bash
./lyracore sql "SELECT * FROM game_creature_template WHERE entry = 6"
```

Runs one SQL statement on the local Realm's database (`lyracore`) and prints SpacetimeDB's output.
It runs as the publisher identity that `dev up` keeps in `.lyracore/native-cli.toml`. SpacetimeDB
lets only that identity read private tables or run `INSERT`, `UPDATE` and `DELETE`, so a plain
`spacetime sql` call is refused for those.

Changes go straight to the live local database. Nothing asks for confirmation and nothing undoes a
change. Take a copy of the data first if you may want it back.

If the checkout has no publisher configuration, the command refuses and names `dev up`. If
SpacetimeDB does not answer, the error points at `dev status`. The exit code is `1` when
SpacetimeDB reports a failure, for example a malformed query.

## What `dev status` verifies

Three things that fail independently:

- **process identity** — the recorded PID is still the process this CLI started (start time +
  command name, not just "a PID exists");
- **the endpoint** — it answers where it was bound, which in `--lan` mode is the LAN address, not
  loopback;
- **the databases** — **every** fixture database is actually published on the node
  (`spacetime describe`: schema only, no rows, no writes) — all four in the default topology, just
  `lyracore` under `--single`. A stack whose PIDs and ports are both perfect is still broken if
  nothing was ever published to the node it is pointed at, and that is the one state the other two
  checks cannot see. Checking only the default would miss the *partial* publish, which is worse:
  it presents as an unrelated mid-session hang rather than a loud failure
  (`docs/danger-zones.md` §3).

`doctor` covers the prerequisites for getting there: the project layout, `rustc` against the
version this checkout declares, Cargo, SpacetimeDB 2.7.1, the WASM target, the three ports, and
OpenSSL 3 or newer for a native checkout. It asks for a **minimum** version; `preflight` asks for
an **exact** match against the pins, because one is "can you build this at all" and the other is
"is it safe to deploy this".

`doctor` also reports the **client data** path from `config.json` (§"`config`" above): unset, or set
to something that no longer validates, is a `⚠` naming the problem and the `config set client-data`
fix — never a failure, because a client is only needed for `lyracore import`, not for `dev up`, and
`doctor` gates the latter.

## Stopping things safely

A bare PID is not an identity — PIDs get reused, and signalling a recycled one kills a stranger's
process. Each recorded PID is stored with the process start time and command name (read via POSIX
`ps`; no `/proc`, no GNU-only flags, so Linux and macOS behave the same).

`dev down` compares that identity before signalling anything. If the PID now belongs to something
else it **refuses and kills nothing**, telling you to run `dev down --forget`, which drops the
record without signalling.

## Passwords

`account create` reads the password from a hidden terminal prompt, or from one bounded stdin line
with `--password-stdin`. It is handed to `gateway provision USER --password-stdin` over the child's
stdin and never becomes a command-line argument, so `ps` shows only the username. It is held in a
zeroized buffer, and is absent from rendered commands, logs, error messages, and `state.json`.

```bash
printf 'hunter2' | ./lyracore account create ALICE --password-stdin
```

## Host administration

`lyracore service reconcile` has moved to `lyracore-ops service reconcile`.
See [Host operations](operations.md) for installation and the reconciliation contract.
It does not fetch or reset the Core checkout.

## `update` — update Core and print restart instructions

```bash
./lyracore update
./lyracore update --reset-mirror  # only when replacing rewritten mirror history
```

An ordinary update refuses tracked edits, fetches `origin`, and checks for commits in `HEAD`
that are absent from `origin/main`. Any such commits stop the update, including a clean local
branch ahead of the remote. Reconcile development branches with Git yourself. Otherwise the
checkout moves to the fetched revision using `git reset --keep`.

Some mirror releases replace history rather than advancing it. `--reset-mirror` explicitly
allows that replacement and first retains the old revision on a local
`lyracore-backup/<full-commit-sha>` branch. The printed branch can be inspected or switched to
for recovery. This option still refuses tracked edits and incoming paths that overlap untracked
or ignored files/directories. Unrelated untracked files are preserved. A failed update leaves
any recovery branch in place. If the recovery branch already exists, the update refuses to
overwrite it; rename that branch with Git before retrying.

A checkout already at `origin/main` reports that it is current. When the CLI pin changes, the
new revision installs on the next `lyracore` invocation.

Nothing restarts automatically. To rebuild and republish after an update, run:

```bash
./lyracore dev down && ./lyracore dev up
```

## `dev logs` — bounded output

`lyracore dev logs [spacetime|gateway]` shows the last 100 lines of each selected log,
capped at 64 KiB per file. `--all` streams the complete files without loading them into memory.
Neither form follows future output.

## Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | Success — including "already up", "already down", and a `doctor` with only warnings |
| `1` | Operational failure: missing prerequisite, failed subprocess, or a refused foreign PID |
| `2` | Invalid invocation, or not inside a checkout |

`doctor` exits nonzero only for launch-blocking failures. A busy port is a warning, not a failure —
it is usually your own running stack.

A separate `import vmaps` clears content readiness on each changed Shard. Complete a full world import with the same profile assignments, then run `import check` before playing.
