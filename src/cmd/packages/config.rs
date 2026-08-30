//! `lyracore packages config NAME [KEY [VALUE]] [--new]` — read and write one installed Package's
//! Package Config across the Realm.
//!
//! `game_package_config` is per-Shard durable state. Every database of the fixture topology holds
//! its own copy of a Package's `(key, value)` rows, and nothing in the module coordinates them: a
//! Package seeds its own defaults on each Shard, and an Operator who edits Shard by Shard is how a
//! Realm ends up answering two different values for one key with nothing saying so. The fan-out is
//! this verb's job. A set writes EVERY Shard of the recorded topology; a read visits every Shard and
//! reports a disagreement instead of collapsing it onto whichever Shard answered first.
//!
//! Reads go through `spacetime sql`, because the table is `public` and no reducer is needed to see
//! it. The write goes through `set_package_config` over the bearer-token HTTP path, never
//! `spacetime call`, for the reason `character gm` documents: the reducer is Operator-gated, and the
//! `spacetime` CLI's own identity is not necessarily the Operator here.
//!
//! The module owns what a key means and whether it may be created. This verb keeps no key list of
//! its own and never rewrites the module's refusal — it forwards `--new` as `allow_new` and prints
//! back what the reducer said, so the two cannot drift.

use std::collections::{BTreeMap, BTreeSet};

use crate::cmd::dev::{operator_call_failure, reducer_url};
use crate::cmd::import;
use crate::cmd::packages::{self, PackageName};
use crate::http::HttpClient;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};

/// The verb name this module's refusals quote back at the operator.
pub const VERB: &str = "packages config";

/// What `packages config` was asked to do with one Package's keys.
///
/// `allow_new` sits inside `Set` rather than beside the action because it means nothing anywhere
/// else: a list or a get carrying a consent flag would be a shape every use has to refuse again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// Every key the Package holds on any Shard, with its value.
    List,
    /// One key's value.
    Get { key: String },
    /// Write one key to every Shard. `allow_new` is the reducer argument of the same name, the only
    /// way to create a key the Package did not seed.
    Set {
        key: String,
        value: String,
        allow_new: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOptions {
    pub package: String,
    pub action: ConfigAction,
}

/// One key-value write, as `set_package_config` takes it.
struct Write<'a> {
    key: &'a str,
    value: &'a str,
    allow_new: bool,
}

/// What every Shard holds for one Package, in topology order.
///
/// A Shard with no row for a key keeps that absence rather than dropping out of the picture: "this
/// Shard never seeded it" is the same class of disagreement as "this Shard holds something else",
/// and an Operator has to see both before the next write.
struct RealmConfig {
    shards: Vec<(String, BTreeMap<String, String>)>,
}

impl RealmConfig {
    /// Every key any Shard holds, sorted.
    fn keys(&self) -> BTreeSet<&str> {
        self.shards
            .iter()
            .flat_map(|(_, rows)| rows.keys().map(String::as_str))
            .collect()
    }

    /// What each Shard answers for `key`, in topology order. `None` is a Shard with no such row.
    fn values(&self, key: &str) -> Vec<(&str, Option<&str>)> {
        self.shards
            .iter()
            .map(|(shard, rows)| (shard.as_str(), rows.get(key).map(String::as_str)))
            .collect()
    }

    /// The one value every Shard agrees on, or `None` when they do not — including when a Shard is
    /// missing the row entirely.
    fn agreed(&self, key: &str) -> Option<&str> {
        let mut answers = self.values(key).into_iter().map(|(_, value)| value);
        let first = answers.next()??;
        answers.all(|value| value == Some(first)).then_some(first)
    }
}

/// Read or write one installed Package's Package Config.
pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
    options: &ConfigOptions,
) -> Result<()> {
    // Parsed before anything else: the name reaches a rendered `spacetime sql` query, and the
    // build's own name rule is narrow enough that nothing which passes it can carry a quote.
    let package = PackageName::parse(&options.package)?;
    refuse_uninstalled(project, &package)?;
    let shards = packages::recorded_databases(project)?;

    match &options.action {
        ConfigAction::List => {
            let realm = read_realm(project, runner, &shards, &package)?;
            print!("{}", list_report(&realm, &package, &shards));
            Ok(())
        }
        ConfigAction::Get { key } => {
            let realm = read_realm(project, runner, &shards, &package)?;
            print!("{}", get_report(&realm, &package, key)?);
            Ok(())
        }
        ConfigAction::Set {
            key,
            value,
            allow_new,
        } => {
            let write = Write {
                key,
                value,
                allow_new: *allow_new,
            };
            set(project, runner, http, &shards, &package, &write)
        }
    }
}

/// Refuse a Package this checkout does not have, naming what it does have.
///
/// BOTH inventories count. A disabled Package is still installed and its rows are still on every
/// Shard; refusing to read them would leave the Operator holding config nothing can show.
fn refuse_uninstalled(project: &ProjectLayout, package: &PackageName) -> Result<()> {
    let inventory = packages::inventory(project)?;
    if inventory.iter().any(|found| found.name == *package) {
        return Ok(());
    }
    let known = if inventory.is_empty() {
        "no Packages are installed".to_string()
    } else {
        format!(
            "installed: {}",
            inventory
                .iter()
                .map(|found| format!("{} ({})", found.name.as_str(), found.state.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Err(Error::Usage(format!(
        "'{}' is not an installed Package, so `{VERB}` has nothing to read or write for it — {known}.",
        package.as_str()
    )))
}

/// Read one Package's rows from every Shard, before anything is printed.
///
/// A failed query is NOT an empty result. Conflating them would report a dead node as "this Shard
/// holds no config", which is the one answer that makes a disagreement invisible.
fn read_realm(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    shards: &[String],
    package: &PackageName,
) -> Result<RealmConfig> {
    // One exact-match query per Shard: no ORDER BY, no IN and no subquery, none of which
    // `spacetime sql` supports. `SELECT *` rather than named columns, so the read cannot break on a
    // column name the SQL layer treats as a keyword; the header names the columns instead.
    let query = format!(
        "SELECT * FROM game_package_config WHERE package_name = '{}'",
        package.as_str()
    );
    let mut rows = Vec::with_capacity(shards.len());
    for shard in shards {
        let output = runner
            .run_and_wait(&import::sql_command(project, shard, &query))
            .map_err(|e| {
                Error::Process(format!(
                    "could not read Package Config from '{shard}': {e}\n  A failed query is not an \
                     empty result, so nothing is reported for any Shard. Check that the node is up \
                     and '{shard}' is published (`lyracore dev status`), and that its module is \
                     current — `game_package_config` only exists once it has been published there."
                ))
            })?;
        rows.push((shard.clone(), config_rows(shard, &output)?));
    }
    Ok(RealmConfig { shards: rows })
}

/// The `key` → `value` pairs in one `spacetime sql` answer.
///
/// The columns are found BY NAME in the header rather than by position: `SELECT *` returns the
/// table's declared order, and a column added ahead of `value` would otherwise be read as the value.
/// An answer with no rows carries no header either, which is an empty result and not a malformed
/// one.
fn config_rows(shard: &str, output: &str) -> Result<BTreeMap<String, String>> {
    let mut cells = import::table_cells(output);
    if cells.is_empty() {
        return Ok(BTreeMap::new());
    }
    let header = cells.remove(0);
    let column = |name: &str| {
        header.iter().position(|cell| cell == name).ok_or_else(|| {
            Error::State(format!(
                "'{shard}' answered for game_package_config without a '{name}' column (got: {}). \
                 Its module is not the one this checkout builds — publish it there first.",
                header.join(", ")
            ))
        })
    };
    let key = column("key")?;
    let value = column("value")?;
    Ok(cells
        .into_iter()
        .filter(|row| row.len() > key.max(value))
        .map(|row| (row[key].clone(), row[value].clone()))
        .collect())
}

/// Every key the Package holds, and what the Realm says each one is.
fn list_report(realm: &RealmConfig, package: &PackageName, shards: &[String]) -> String {
    let mut report = format!("\n=== Package Config: {} ===\n", package.as_str());
    report.push_str(&format!(
        "{} Shard(s): {}\n\n",
        shards.len(),
        packages::shard_list(shards)
    ));

    let keys = realm.keys();
    if keys.is_empty() {
        report.push_str("  no config keys on any Shard.\n\n");
        report.push_str(&format!(
            "A Package seeds its own defaults when it initialises, so an empty list usually means \
             this one has none. `{VERB} {} KEY VALUE --new` sets a key anyway.\n",
            package.as_str()
        ));
        return report;
    }

    let mut disagreeing = 0;
    for key in &keys {
        match realm.agreed(key) {
            Some(value) => report.push_str(&format!("  {key:<28} {value}\n")),
            None => {
                disagreeing += 1;
                report.push_str(&disagreement(realm, key));
            }
        }
    }
    if disagreeing > 0 {
        report.push_str(&format!(
            "\n{disagreeing} key(s) disagree across Shards. `{VERB} {} KEY VALUE` writes one value \
             to every Shard.\n",
            package.as_str()
        ));
    }
    report
}

/// One key's value, or the Shards that disagree about it.
fn get_report(realm: &RealmConfig, package: &PackageName, key: &str) -> Result<String> {
    let keys = realm.keys();
    if !keys.contains(key) {
        let known = if keys.is_empty() {
            format!(
                "it has no config keys on any Shard — `{VERB} {} {key} VALUE --new` sets one anyway",
                package.as_str()
            )
        } else {
            format!(
                "known keys: {}",
                keys.into_iter().collect::<Vec<_>>().join(", ")
            )
        };
        return Err(Error::Usage(format!(
            "Package '{}' has no config key '{key}' on any Shard — {known}.",
            package.as_str()
        )));
    }
    Ok(match realm.agreed(key) {
        Some(value) => format!("{value}\n"),
        None => disagreement(realm, key),
    })
}

/// What each Shard answers for a key they do not agree on, Shard by Shard.
///
/// Printed INSTEAD of a value, never alongside one: whatever reads this verb's output for a single
/// value must not quietly receive one of several answers.
fn disagreement(realm: &RealmConfig, key: &str) -> String {
    let mut report = format!("  {key:<28} DISAGREES across Shards:\n");
    for (shard, value) in realm.values(key) {
        report.push_str(&format!(
            "  {blank:<28} {shard:<24} {}\n",
            value.unwrap_or("(unset)"),
            blank = ""
        ));
    }
    report
}

/// Write one key to every Shard of the recorded topology.
fn set(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    http: &dyn HttpClient,
    shards: &[String],
    package: &PackageName,
    write: &Write,
) -> Result<()> {
    let credential = crate::token::resolve_existing(runner, &project.token_file())?;
    // Serialized rather than concatenated: the value is arbitrary Operator text, and hand-escaping
    // it would be one more place to forget a quote or a newline.
    let arguments =
        serde_json::to_string(&(package.as_str(), write.key, write.value, write.allow_new))?;

    println!();
    println!(
        "=== setting {}.{} on {} Shard(s) ===",
        package.as_str(),
        write.key,
        shards.len()
    );
    for (index, shard) in shards.iter().enumerate() {
        match http.post_json(
            &reducer_url(shard, "set_package_config"),
            Some(credential.token()),
            &arguments,
        ) {
            Ok(_) => println!("  ✓ {shard}"),
            Err(error) => return Err(write_failure(project, shards, index, error)),
        }
    }
    println!();
    println!(
        "{}.{} = {} on: {}",
        package.as_str(),
        write.key,
        write.value,
        packages::shard_list(shards)
    );
    Ok(())
}

/// A write that stopped part way through the Realm.
///
/// Fail fast, never rollback — the rule `packages replay` already runs on, for the same reason: a
/// Realm that reported success while half-written cannot be recovered from its own report, and one
/// that named the Shard it stopped at can. Re-running the same command after the cause is fixed
/// rewrites the Shards that already took the value, which changes nothing on them.
///
/// The module's refusal is forwarded word for word, key list included. Only the spelling of the
/// consent is translated: the reducer argument is `allow_new`, and on this surface it is `--new`.
fn write_failure(
    project: &ProjectLayout,
    shards: &[String],
    stopped_at: usize,
    error: Error,
) -> Error {
    let refusal = error.to_string();
    let advised = if refusal.contains("allow_new") {
        format!("{refusal}\n  On this command line, allow_new is `--new`.")
    } else {
        operator_call_failure(project, &shards[stopped_at], error).to_string()
    };
    Error::Process(format!(
        "{advised}\n  stopped at: {}\n  written: {}\n  untouched: {}\n\n  Nothing is rolled back. \
         Fix the cause and re-run the SAME command — rewriting a Shard that already took the value \
         changes nothing on it.",
        shards[stopped_at],
        packages::shard_list(&shards[..stopped_at]),
        packages::shard_list(&shards[stopped_at + 1..]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeHttp;
    use crate::proc::fake::FakeStack;
    use crate::project::Topology;
    use crate::state::{ProcessRecord, RuntimeState};
    use tempfile::TempDir;

    /// A checkout with an installed Package, a recorded topology, and a persisted credential — the
    /// three things this verb reads before it does anything.
    struct Checkout {
        tmp: TempDir,
        project: ProjectLayout,
    }

    impl Checkout {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
            let project = ProjectLayout::from_root(tmp.path()).unwrap();
            crate::token::resolve_or_mint(
                &FakeStack::new()
                    .fail_on("login show", "not logged in")
                    .runner(),
                &FakeHttp::new(),
                &project.token_file(),
                "http://127.0.0.1:3000",
            )
            .unwrap();
            Self { tmp, project }
        }

        fn with_package(&self, name: &str) -> &Self {
            self.write_package(&self.tmp.path().join("packages").join(name))
        }

        fn with_disabled_package(&self, name: &str) -> &Self {
            self.write_package(
                &self
                    .tmp
                    .path()
                    .join(".lyracore/packages-disabled")
                    .join(name),
            )
        }

        fn write_package(&self, dir: &std::path::Path) -> &Self {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src/mod.rs"), "").unwrap();
            self
        }

        /// A stack that was brought up in this topology. The gateway record is what makes the
        /// recorded topology count: with nothing recorded, the default sharded fixture is the
        /// answer, which is the behaviour `publish` and `packages replay` already have.
        fn with_topology(&self, topology: Topology) -> &Self {
            RuntimeState {
                gateway: Some(ProcessRecord {
                    pid: 4001,
                    identity: "gateway".to_string(),
                }),
                topology: topology.as_str().to_string(),
                ..Default::default()
            }
            .save(&self.project.state_file())
            .unwrap();
            self
        }
    }

    fn list_options(package: &str) -> ConfigOptions {
        ConfigOptions {
            package: package.to_string(),
            action: ConfigAction::List,
        }
    }

    fn get_options(package: &str, key: &str) -> ConfigOptions {
        ConfigOptions {
            package: package.to_string(),
            action: ConfigAction::Get {
                key: key.to_string(),
            },
        }
    }

    fn set_options(package: &str, key: &str, value: &str, allow_new: bool) -> ConfigOptions {
        ConfigOptions {
            package: package.to_string(),
            action: ConfigAction::Set {
                key: key.to_string(),
                value: value.to_string(),
                allow_new,
            },
        }
    }

    /// A `spacetime sql` answer in the tabular shape the real CLI prints, in the table's own column
    /// order.
    fn sql_rows(rows: &[(&str, &str)]) -> String {
        let mut out = " id | package_name | key | value \n".to_string();
        for (index, (key, value)) in rows.iter().enumerate() {
            out.push_str(&format!(" {} | greeter | {key} | {value} \n", index + 1));
        }
        out
    }

    /// A Shard answering `rows` for the Package Config query.
    fn shard(stack: FakeStack, database: &str, rows: &[(&str, &str)]) -> FakeStack {
        stack.with_stdout(
            &format!("{database} SELECT * FROM game_package_config"),
            &sql_rows(rows),
        )
    }

    fn queries(stack: &FakeStack) -> Vec<String> {
        stack
            .rendered()
            .into_iter()
            .filter(|r| r.contains("spacetime sql"))
            .collect()
    }

    fn realm(shards: &[(&str, &[(&str, &str)])]) -> RealmConfig {
        RealmConfig {
            shards: shards
                .iter()
                .map(|(name, rows)| {
                    (
                        (*name).to_string(),
                        rows.iter()
                            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    fn package(name: &str) -> PackageName {
        PackageName::parse(name).unwrap()
    }

    // ---- the installed check ----

    #[test]
    fn a_package_that_is_not_installed_is_refused_with_the_installed_list() {
        let checkout = Checkout::new();
        checkout.with_package("greeter");
        checkout.with_disabled_package("bellringer");
        let stack = FakeStack::new();

        let error = run(
            &checkout.project,
            &stack.runner(),
            &FakeHttp::new(),
            &list_options("absent"),
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        let message = error.to_string();
        assert!(
            message.contains("'absent' is not an installed Package"),
            "{message}"
        );
        assert!(message.contains("greeter (enabled)"), "{message}");
        assert!(message.contains("bellringer (disabled)"), "{message}");
        assert!(
            stack.rendered().is_empty(),
            "no Shard is read for a Package that is not here: {:?}",
            stack.rendered()
        );
    }

    /// A disabled Package still has rows on every Shard. Refusing to read them would leave the
    /// Operator holding config nothing can show.
    #[test]
    fn a_disabled_package_is_still_installed_enough_to_read() {
        let checkout = Checkout::new();
        checkout
            .with_disabled_package("bellringer")
            .with_topology(Topology::Single);
        let stack = FakeStack::new().with_stdout(
            "lyracore SELECT * FROM game_package_config",
            " id | package_name | key | value \n 1 | bellringer | max_bells | 5 \n",
        );

        run(
            &checkout.project,
            &stack.runner(),
            &FakeHttp::new(),
            &list_options("bellringer"),
        )
        .unwrap();

        assert_eq!(queries(&stack).len(), 1, "{:?}", queries(&stack));
    }

    // ---- reading ----

    #[test]
    fn a_list_reads_every_shard_of_the_recorded_topology() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let mut stack = FakeStack::new();
        for database in Topology::Sharded.databases() {
            stack = shard(stack, database, &[("greeting", "Hello")]);
        }

        run(
            &checkout.project,
            &stack.runner(),
            &FakeHttp::new(),
            &list_options("greeter"),
        )
        .unwrap();

        let asked = queries(&stack);
        assert_eq!(asked.len(), Topology::Sharded.databases().len());
        for (rendered, database) in asked.iter().zip(Topology::Sharded.databases()) {
            assert!(rendered.contains(database), "{rendered}");
            assert!(
                rendered.contains("package_name = 'greeter'"),
                "the query is scoped to the Package: {rendered}"
            );
            assert!(!rendered.contains("ORDER BY"), "{rendered}");
            assert!(!rendered.contains(" IN ("), "{rendered}");
        }
    }

    #[test]
    fn a_list_shows_every_key_the_realm_agrees_on() {
        let realm = realm(&[
            ("lyracore", &[("bells", "5"), ("greeting", "Hello")]),
            (
                "lyracore-kalimdor",
                &[("bells", "5"), ("greeting", "Hello")],
            ),
        ]);

        let report = list_report(&realm, &package("greeter"), &shard_names());

        assert!(
            report.contains("bells                        5\n"),
            "{report}"
        );
        assert!(
            report.contains("greeting                     Hello\n"),
            "{report}"
        );
        assert!(!report.contains("DISAGREES"), "{report}");
    }

    #[test]
    fn a_package_with_no_keys_anywhere_says_so_and_names_how_to_set_one() {
        let realm = realm(&[("lyracore", &[])]);
        let report = list_report(&realm, &package("greeter"), &shard_names());
        assert!(report.contains("no config keys on any Shard"), "{report}");
        assert!(report.contains("--new"), "{report}");
    }

    fn shard_names() -> Vec<String> {
        vec!["lyracore".to_string(), "lyracore-kalimdor".to_string()]
    }

    #[test]
    fn a_get_prints_the_value_every_shard_agrees_on() {
        let realm = realm(&[
            ("lyracore", &[("greeting", "Hello")]),
            ("lyracore-kalimdor", &[("greeting", "Hello")]),
        ]);
        assert_eq!(
            get_report(&realm, &package("greeter"), "greeting").unwrap(),
            "Hello\n"
        );
    }

    #[test]
    fn a_get_reads_every_shard_before_it_answers() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let mut stack = FakeStack::new();
        for database in Topology::Sharded.databases() {
            stack = shard(stack, database, &[("greeting", "Hello")]);
        }

        run(
            &checkout.project,
            &stack.runner(),
            &FakeHttp::new(),
            &get_options("greeter", "greeting"),
        )
        .unwrap();

        assert_eq!(queries(&stack).len(), Topology::Sharded.databases().len());
    }

    #[test]
    fn a_get_of_a_key_no_shard_has_is_refused_with_the_keys_that_exist() {
        let realm = realm(&[("lyracore", &[("bells", "5"), ("greeting", "Hello")])]);

        let error = get_report(&realm, &package("greeter"), "greetng").unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        let message = error.to_string();
        assert!(message.contains("no config key 'greetng'"), "{message}");
        assert!(message.contains("known keys: bells, greeting"), "{message}");
    }

    /// The acceptance criterion the read path exists for: Shards holding different values must not
    /// collapse onto whichever one answered first.
    #[test]
    fn a_get_over_shards_that_disagree_names_every_shards_answer_instead_of_one_value() {
        let realm = realm(&[
            ("lyracore", &[("greeting", "Hello")]),
            ("lyracore-kalimdor", &[("greeting", "Ishnu")]),
            ("lyracore-instances", &[]),
        ]);

        let report = get_report(&realm, &package("greeter"), "greeting").unwrap();

        assert!(report.contains("DISAGREES across Shards"), "{report}");
        for expected in [
            "lyracore ",
            "Hello",
            "lyracore-kalimdor",
            "Ishnu",
            "(unset)",
        ] {
            assert!(
                report.contains(expected),
                "{expected} missing from: {report}"
            );
        }
        assert_ne!(report, "Hello\n");
    }

    #[test]
    fn a_list_counts_the_keys_that_disagree_and_leaves_the_agreeing_ones_plain() {
        let realm = realm(&[
            ("lyracore", &[("bells", "5"), ("greeting", "Hello")]),
            (
                "lyracore-kalimdor",
                &[("bells", "5"), ("greeting", "Ishnu")],
            ),
        ]);

        let report = list_report(&realm, &package("greeter"), &shard_names());

        assert!(
            report.contains("bells                        5\n"),
            "{report}"
        );
        assert!(
            report.contains("1 key(s) disagree across Shards"),
            "{report}"
        );
    }

    #[test]
    fn one_shard_missing_a_row_is_a_disagreement_and_not_agreement() {
        let realm = realm(&[
            ("lyracore", &[("greeting", "Hello")]),
            ("lyracore-kalimdor", &[]),
        ]);
        assert_eq!(realm.agreed("greeting"), None);
    }

    /// A dead node must never read as "this Shard holds no config" — that is the one answer that
    /// makes a disagreement invisible.
    #[test]
    fn an_unreachable_shard_fails_the_read_rather_than_reporting_an_empty_shard() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let stack = shard(FakeStack::new(), "lyracore", &[("greeting", "Hello")])
            .fail_on("lyracore-kalimdor SELECT", "connection refused");

        let error = run(
            &checkout.project,
            &stack.runner(),
            &FakeHttp::new(),
            &list_options("greeter"),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("lyracore-kalimdor"), "{message}");
        assert!(
            message.contains("A failed query is not an empty result"),
            "{message}"
        );
    }

    #[test]
    fn columns_are_read_by_name_so_a_new_column_cannot_be_mistaken_for_the_value() {
        let answer = concat!(
            " id | package_name | source | key | value \n",
            " 1 | greeter | seed | greeting | Hello \n"
        );
        assert_eq!(
            config_rows("lyracore", answer).unwrap().get("greeting"),
            Some(&"Hello".to_string())
        );
    }

    #[test]
    fn an_answer_with_no_rows_is_an_empty_result_and_not_a_failure() {
        assert!(config_rows("lyracore", "").unwrap().is_empty());
        assert!(
            config_rows("lyracore", " id | package_name | key | value \n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_answer_without_the_value_column_names_the_module_as_the_problem() {
        let error = config_rows(
            "lyracore",
            " id | package_name | key \n 1 | greeter | greeting \n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("'value' column"), "{error}");
        assert!(error.to_string().contains("publish"), "{error}");
    }

    // ---- writing ----

    #[test]
    fn a_set_reaches_every_shard_of_the_recorded_topology() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let http = FakeHttp::new();

        run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap();

        let urls: Vec<String> = http.requests().into_iter().map(|r| r.url).collect();
        let expected: Vec<String> = Topology::Sharded
            .databases()
            .into_iter()
            .map(|database| reducer_url(database, "set_package_config"))
            .collect();
        assert_eq!(urls, expected);
    }

    #[test]
    fn the_reducer_arguments_carry_the_package_key_value_and_allow_new_in_order() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Single);

        let http = FakeHttp::new();
        run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap();
        assert_eq!(
            http.requests()[0].body,
            r#"["greeter","greeting","Hello",false]"#
        );

        let http = FakeHttp::new();
        run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", true),
        )
        .unwrap();
        assert_eq!(
            http.requests()[0].body,
            r#"["greeter","greeting","Hello",true]"#
        );
    }

    #[test]
    fn a_value_with_quotes_and_newlines_is_json_escaped_rather_than_concatenated() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Single);
        let http = FakeHttp::new();

        run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "say \"hi\"\nthen wave", false),
        )
        .unwrap();

        assert_eq!(
            http.requests()[0].body,
            r#"["greeter","greeting","say \"hi\"\nthen wave",false]"#
        );
    }

    /// The module owns the rule. The CLI forwards its refusal word for word and only translates the
    /// spelling of the consent.
    #[test]
    fn the_modules_unknown_key_refusal_is_forwarded_with_its_key_list_intact() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let http = FakeHttp::failing(
            "package 'greeter' has no config key 'greetng'; known keys: bells, greeting. Pass \
             allow_new to set it anyway",
        );

        let error = run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greetng", "Hello", false),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("known keys: bells, greeting"), "{message}");
        assert!(message.contains("`--new`"), "{message}");
        assert_eq!(
            http.requests().len(),
            1,
            "a refusal on the first Shard stops before the second is written"
        );
    }

    #[test]
    fn a_failure_between_two_shards_stops_the_write_and_places_every_shard() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Sharded);
        let needle = format!("/database/{}/call/", ProjectLayout::KALIMDOR_SHARD);
        let http = FakeHttp::refusing(&needle, "connection refused");

        let error = run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap_err();

        let message = error.to_string();
        assert_eq!(error.exit_code(), crate::error::EXIT_FAILURE);
        assert!(
            message.contains("stopped at: lyracore-kalimdor"),
            "{message}"
        );
        assert!(message.contains("written: lyracore\n"), "{message}");
        assert!(
            message.contains("untouched: lyracore-instances lyracore-realm"),
            "{message}"
        );
        assert_eq!(http.requests().len(), 2, "the write stops at the failure");
    }

    #[test]
    fn an_unclaimed_operator_is_distinguished_from_an_unreachable_node() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Single);
        let http = FakeHttp::failing("operator not claimed");

        let error = run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap_err();

        assert!(error.to_string().contains("dev up"), "{error}");
    }

    #[test]
    fn the_token_reaches_the_request_as_a_bearer_and_never_as_an_argument() {
        let checkout = Checkout::new();
        checkout
            .with_package("greeter")
            .with_topology(Topology::Single);
        let http = FakeHttp::new();

        run(
            &checkout.project,
            &FakeStack::new().runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap();

        let request = &http.requests()[0];
        assert!(request.bearer.is_some());
        assert!(!request.body.contains(request.bearer.as_deref().unwrap()));
        assert!(!request.url.contains(request.bearer.as_deref().unwrap()));
    }

    #[test]
    fn a_set_with_no_credential_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let project = ProjectLayout::from_root(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join("packages/greeter/src")).unwrap();
        std::fs::write(tmp.path().join("packages/greeter/src/mod.rs"), "").unwrap();
        let stack = FakeStack::new().fail_on("login show", "You are not logged in");
        let http = FakeHttp::new();

        let error = run(
            &project,
            &stack.runner(),
            &http,
            &set_options("greeter", "greeting", "Hello", false),
        )
        .unwrap_err();

        assert!(error.to_string().contains("lyracore dev up"), "{error}");
        assert!(http.requests().is_empty());
    }
}
