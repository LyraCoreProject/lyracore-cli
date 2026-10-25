<p align="center"><img src="https://raw.githubusercontent.com/LyraCoreProject/LyraCore/refs/heads/main/lyracore-icon-light.svg" alt="LyraCore" width="120"></p>

# lyracore-cli

The CLI for [LyraCore](https://github.com/LyraCoreProject/LyraCore). Start a local realm, import
world data, manage Packages, prepare client content, and provision Accounts.

## Run from a Core checkout

Follow the [LyraCore quickstart](https://github.com/LyraCoreProject/LyraCore/blob/main/docs/quickstart.md)
to install the prerequisites and clone Core. Its `./lyracore` launcher builds and runs the CLI
commit pinned in `.lyracore-cli-rev`. You do not need to clone this CLI repository to use it.

```bash
./lyracore doctor
./lyracore dev up
./lyracore account create ALICE
./lyracore import
./lyracore dev check
./lyracore dev status
```

Core's installer also supplies a global `lyracore` launcher that works from checkout subdirectories.
Core checkouts with `native-host/prepare-host.py` run one Native Host and one Shard. Older Core
checkouts retain the four-Shard runtime, with `dev up --single` selecting one. Import your world
content before play. Stop the processes owned by the CLI with `lyracore dev down`.

## Find a command

```bash
lyracore help                 # initial setup
lyracore help --all           # complete command surface
lyracore packages --help      # one command family
lyracore help packages apply  # one operation
```

The [command reference](docs/commands.md) holds the detailed behavior and safety contracts.
Full and focused help share one catalogue; a test keeps its documented command list synchronized.

Before a direct Gateway Cargo build or editor check, run `lyracore bindings` to prepare generated
bindings. `dev up` prepares them automatically. See the [bindings contract](docs/commands.md#bindings).

`packages apply` prepares and activates installed source. Authors can separately use
`packages build` to produce artifacts and `packages check` to verify existing ones.
`client sync` installs client content; `client pack --out DIR` builds a distributable artifact.

`lyracore update` updates Core only when it can preserve local commits. Replacing rewritten mirror
history requires `--reset-mirror`, which saves a recovery branch first. Neither form restarts
anything. See the [update contract](docs/commands.md#update--update-core-and-print-restart-instructions).

## Host administration

`lyracore production status` inspects an explicitly named production topology without changing it.
Root-only systemd reconciliation lives in the separate `lyracore-ops` binary and uses the selected
checkout without updating source. See [installation and host operations](docs/operations.md).
The developer CLI does not install Rust or SpacetimeDB or manage backups.

## Development

FreeBSD builds require 15.1 or newer. Package moves use its atomic `renameat2` operation to
refuse existing destinations without overwriting them.

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo +1.85 check --locked  # supported minimum toolchain
bash deploy/spacetimedb-prune-test.sh
python3 -m unittest discover -s deploy -p 'test_*.py'
```

Unit tests use fake process adapters and temporary directories. Checkout-update integration tests
also invoke the built CLI and Git against disposable local repositories, without network access.
No test starts a real realm or administers a host service.

[Core contract ownership](docs/contract-ownership.md) records the interfaces needed to remove the
remaining artifact-format and import-verification duplication without losing checks.

## License

Licensed under either [Apache License 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.
