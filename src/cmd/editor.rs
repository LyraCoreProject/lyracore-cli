//! `lyracore editor`: build the pinned Meld world editor once, then run it against the local
//! Native Host. The `MELD_*` variables are Meld's launch contract; see docs/commands.md#editor.
use crate::cmd::client;
use crate::config::Config;
use crate::proc::{CommandSpec, ProcessInspector, ProcessRunner};
use crate::project::ProjectLayout;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

const MELD_REPOSITORY: &str = "https://github.com/Mathih13/Meld.git";

#[cfg(target_os = "macos")]
const MELD_BINARY: &str = "build/bin/Meld.app/Contents/MacOS/Meld";
#[cfg(not(target_os = "macos"))]
const MELD_BINARY: &str = "build/bin/Meld";

#[cfg(target_os = "macos")]
const WAILS_BUILD_TAGS: &[&str] = &[];
#[cfg(not(target_os = "macos"))]
const WAILS_BUILD_TAGS: &[&str] = &["-tags", "webkit2_41"];

pub fn run(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    inspector: &dyn ProcessInspector,
    home: Option<&Path>,
) -> Result<()> {
    let rev = read_pin(project)?;
    if !project.has_native_host() {
        return Err(Error::Usage(
            "lyracore editor needs a Native Host checkout (native-host/prepare-host.py)".into(),
        ));
    }
    crate::cmd::publish::require_native_host(project, inspector, &ProjectLayout::stdb_uri())
        .map_err(|error| {
            Error::Usage(format!(
                "lyracore editor needs the local Native Host running; run `lyracore dev up` \
                 first ({error})"
            ))
        })?;
    crate::token::native_owner(project)?;
    let wow_path = wow_client_folder(project)?;

    let install = project.editor_install(&rev);
    if !install.join(MELD_BINARY).is_file() {
        build(runner, &rev, &install, home)?;
    }

    let token_file = project.token_file();
    let token_file = std::fs::canonicalize(&token_file).unwrap_or(token_file);
    let mut launch = CommandSpec::new(install.join(MELD_BINARY).to_string_lossy())
        .cwd(install.clone())
        .env("MELD_LYRACORE_URL", ProjectLayout::stdb_uri())
        .env("MELD_LYRACORE_DATABASE", ProjectLayout::DATABASE)
        .env("MELD_LYRACORE_TOKEN_FILE", token_file.to_string_lossy());
    if let Some(wow_path) = wow_path {
        launch = launch.env("MELD_WOW_PATH", wow_path.to_string_lossy());
    }
    println!(
        "starting Meld on {} (close it or press Ctrl+C to stop)",
        ProjectLayout::stdb_uri()
    );
    runner.run_streaming(&launch)
}

fn read_pin(project: &ProjectLayout) -> Result<String> {
    let pin = project.editor_pin();
    let refuse = || {
        Error::Usage(format!(
            "{} must hold one 40-character lowercase hex Meld commit SHA",
            pin.display()
        ))
    };
    let text = std::fs::read_to_string(&pin).map_err(|_| refuse())?;
    let rev = text.lines().next().unwrap_or("");
    if rev.len() != 40 || !rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(refuse());
    }
    Ok(rev.to_string())
}

/// `client-data` names the client's `Data/` folder; Meld wants the folder that contains it.
fn wow_client_folder(project: &ProjectLayout) -> Result<Option<PathBuf>> {
    let Some(raw) = Config::load(&project.config_file())?.client_data else {
        return Ok(None);
    };
    let data = client::configured_client_data(project, &raw)?;
    data.parent()
        .map(|root| Some(root.to_path_buf()))
        .ok_or_else(|| {
            Error::State(format!(
                "client-data {} has no parent folder",
                data.display()
            ))
        })
}

/// Builds in a sibling `.partial` folder and renames it into place, so an interrupted build is
/// never mistaken for an installed one.
fn build(runner: &dyn ProcessRunner, rev: &str, install: &Path, home: Option<&Path>) -> Result<()> {
    let wails = require_prerequisites(runner, home)?;
    println!("preparing Meld (first run, this takes a few minutes)...");
    let staging = install.with_extension("partial");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let built = fetch_and_build(runner, rev, &staging, &wails).and_then(|()| {
        if staging.join(MELD_BINARY).is_file() {
            Ok(())
        } else {
            Err(Error::Process(format!(
                "`wails build` reported success but left no {MELD_BINARY}"
            )))
        }
    });
    if let Err(error) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if install.exists() {
        std::fs::remove_dir_all(install)?;
    }
    std::fs::rename(&staging, install)?;
    Ok(())
}

fn fetch_and_build(
    runner: &dyn ProcessRunner,
    rev: &str,
    staging: &Path,
    wails: &str,
) -> Result<()> {
    let git = |command: CommandSpec| -> Result<()> {
        runner
            .run_and_wait(&command.cwd(staging).env("GIT_TERMINAL_PROMPT", "0"))
            .map(drop)
            .map_err(|error| {
                Error::Process(format!(
                    "could not fetch Meld {rev} from {MELD_REPOSITORY}: {error}"
                ))
            })
    };
    git(CommandSpec::new("git").arg("init").arg("--quiet"))?;
    git(CommandSpec::new("git")
        .arg("fetch")
        .arg("--quiet")
        .arg("--depth")
        .arg("1")
        .arg("--")
        .arg(MELD_REPOSITORY)
        .arg(rev))?;
    git(CommandSpec::new("git")
        .arg("checkout")
        .arg("--detach")
        .arg("FETCH_HEAD"))?;

    let mut build = CommandSpec::new(wails).cwd(staging).arg("build");
    for arg in WAILS_BUILD_TAGS {
        build = build.arg(*arg);
    }
    runner
        .run_streaming(&build)
        .map_err(|error| Error::Process(format!("building Meld failed: {error}")))
}

/// Returns the `wails` program to run: `wails` on PATH, else `$HOME/go/bin/wails`.
fn require_prerequisites(runner: &dyn ProcessRunner, home: Option<&Path>) -> Result<String> {
    let answers = |program: &str, arg: &str| {
        runner
            .run_capturing_stderr(&CommandSpec::new(program).arg(arg))
            .is_ok()
    };
    for (program, arg, hint) in [
        ("git", "--version", "install Git with your package manager"),
        ("go", "version", "install Go from https://go.dev/dl/"),
        (
            "bun",
            "--version",
            "install Bun with `curl -fsSL https://bun.sh/install | bash`",
        ),
    ] {
        if !answers(program, arg) {
            return Err(Error::PrerequisiteMissing(format!(
                "`{program}` is needed to build Meld; {hint}"
            )));
        }
    }
    let mut candidates = vec!["wails".to_string()];
    candidates.extend(home.map(|home| home.join("go/bin/wails").to_string_lossy().into_owned()));
    candidates
        .into_iter()
        .find(|wails| answers(wails, "version"))
        .ok_or_else(|| {
            Error::PrerequisiteMissing(
                "`wails` is needed to build Meld; install it with \
                 `go install github.com/wailsapp/wails/v2/cmd/wails@latest`"
                    .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{Call, FakeStack};
    use crate::state::{ProcessRecord, RuntimeState};
    use tempfile::TempDir;

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";
    const TOKEN: &str = "eyJmYWtl.owner-token-must-stay-in-its-file.SIG";

    struct Fixture {
        tmp: TempDir,
        project: ProjectLayout,
        stack: FakeStack,
    }

    impl Fixture {
        /// A Native Host checkout with a running Host, its owner token and a valid pin.
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
            std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
            std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
            let project = ProjectLayout::from_root(tmp.path()).unwrap();
            std::fs::write(project.editor_pin(), format!("{REV}\n")).unwrap();
            std::fs::create_dir_all(&project.state_dir).unwrap();
            std::fs::write(project.token_file(), TOKEN).unwrap();
            std::fs::write(
                project.native_cli_config(),
                format!("spacetimedb_token = \"{TOKEN}\""),
            )
            .unwrap();
            RuntimeState {
                native_runtime: true,
                spacetime: Some(ProcessRecord {
                    pid: 42,
                    identity: "native host".into(),
                }),
                ..Default::default()
            }
            .save(&project.state_file())
            .unwrap();
            let meld = tmp.path().join("meld-source");
            std::fs::create_dir_all(meld.join(MELD_BINARY).parent().unwrap()).unwrap();
            std::fs::write(meld.join(MELD_BINARY), "").unwrap();
            let stack = FakeStack::new()
                .with_process(42, "native host")
                .with_port(ProjectLayout::STDB_PORT)
                .with_git_clone(&meld);
            Self {
                tmp,
                project,
                stack,
            }
        }

        fn run(&self) -> Result<()> {
            run(
                &self.project,
                &self.stack.runner(),
                &self.stack.inspector(),
                None,
            )
        }

        fn install(&self) -> PathBuf {
            self.project.editor_install(REV)
        }

        fn installed(&self) -> &Self {
            std::fs::create_dir_all(self.install().join(MELD_BINARY).parent().unwrap()).unwrap();
            std::fs::write(self.install().join(MELD_BINARY), "").unwrap();
            self
        }

        fn launch(&self) -> CommandSpec {
            self.stack
                .calls()
                .into_iter()
                .find_map(|call| match call {
                    Call::Stream(spec) if spec.program().ends_with("Meld") => Some(spec),
                    _ => None,
                })
                .expect("Meld was launched")
        }
    }

    fn tags() -> &'static str {
        if cfg!(target_os = "macos") {
            ""
        } else {
            " -tags webkit2_41"
        }
    }

    #[test]
    fn a_missing_or_malformed_pin_is_refused_before_any_process_runs() {
        for pin in [
            None,
            Some(""),
            Some("0123456789abcdef"),
            Some("0123456789ABCDEF0123456789ABCDEF01234567"),
            Some("main"),
        ] {
            let fixture = Fixture::new();
            match pin {
                Some(text) => std::fs::write(fixture.project.editor_pin(), text).unwrap(),
                None => std::fs::remove_file(fixture.project.editor_pin()).unwrap(),
            }

            let error = fixture.run().unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains(".lyracore-editor-rev must hold one"),
                "{pin:?}: {error}"
            );
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
            assert!(fixture.stack.calls().is_empty(), "{pin:?}");
        }
    }

    #[test]
    fn a_stopped_native_host_points_at_dev_up_and_fetches_nothing() {
        let fixture = Fixture::new();
        let stack = FakeStack::new();

        let error = run(&fixture.project, &stack.runner(), &stack.inspector(), None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("run `lyracore dev up` first"), "{error}");
        assert!(stack.calls().is_empty());
        assert!(!fixture.project.state_dir.join("editor").exists());
    }

    #[test]
    fn a_missing_owner_token_points_at_dev_up_and_fetches_nothing() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.project.token_file()).unwrap();

        let error = fixture.run().unwrap_err().to_string();

        assert!(error.contains("run `lyracore dev up` first"), "{error}");
        assert!(fixture.stack.calls().is_empty());
    }

    #[test]
    fn a_missing_prerequisite_is_named_before_anything_is_fetched() {
        for (needle, named) in [
            ("git --version", "`git`"),
            ("go version", "`go`"),
            ("bun --version", "`bun`"),
            ("wails version", "`wails`"),
        ] {
            let mut fixture = Fixture::new();
            fixture.stack = fixture.stack.clone().fail_on(needle, "not found");

            let error = fixture.run().unwrap_err().to_string();

            assert!(error.contains(named), "{needle}: {error}");
            assert!(
                !fixture
                    .stack
                    .rendered()
                    .iter()
                    .any(|command| command.contains("fetch")),
                "{needle}"
            );
        }
    }

    #[test]
    fn wails_under_the_go_bin_folder_is_used_when_it_is_not_on_path() {
        let mut fixture = Fixture::new();
        let home = fixture.tmp.path().join("home");
        fixture.stack =
            fixture
                .stack
                .clone()
                .fail_until("wails version", "not found", "go/bin/wails");

        run(
            &fixture.project,
            &fixture.stack.runner(),
            &fixture.stack.inspector(),
            Some(&home),
        )
        .unwrap();

        let wails = home.join("go/bin/wails").to_string_lossy().into_owned();
        assert!(
            fixture
                .stack
                .rendered()
                .contains(&format!("{wails} build{}", tags())),
            "{:?}",
            fixture.stack.rendered()
        );
    }

    #[test]
    fn the_first_run_fetches_the_pinned_commit_builds_it_and_launches_it() {
        let fixture = Fixture::new();

        fixture.run().unwrap();

        let staging = fixture.install().with_extension("partial");
        let rendered = fixture.stack.rendered();
        let work: Vec<&String> = rendered
            .iter()
            .filter(|command| !command.contains("version"))
            .collect();
        assert_eq!(
            work,
            vec![
                "git init --quiet",
                &format!(
                    "git fetch --quiet --depth 1 -- https://github.com/Mathih13/Meld.git {REV}"
                ),
                "git checkout --detach FETCH_HEAD",
                &format!("wails build{}", tags()),
                &fixture.install().join(MELD_BINARY).to_string_lossy(),
            ]
        );
        let fetch = fixture
            .stack
            .calls()
            .into_iter()
            .find_map(|call| match call {
                Call::Wait(spec) if spec.render().contains("fetch") => Some(spec),
                _ => None,
            })
            .unwrap();
        assert_eq!(fetch.cwd_value(), Some(staging.as_path()));
        assert!(fixture.install().join(MELD_BINARY).is_file());
        assert!(!staging.exists());
    }

    #[test]
    fn an_installed_build_is_launched_without_fetching_or_building() {
        let fixture = Fixture::new();
        fixture.installed();

        fixture.run().unwrap();

        assert_eq!(
            fixture.stack.rendered(),
            vec![fixture
                .install()
                .join(MELD_BINARY)
                .to_string_lossy()
                .into_owned()]
        );
    }

    #[test]
    fn a_failed_build_leaves_nothing_that_looks_installed() {
        let mut fixture = Fixture::new();
        fixture.stack = fixture
            .stack
            .clone()
            .fail_on("wails build", "compile error");

        let error = fixture.run().unwrap_err().to_string();

        assert!(error.contains("building Meld failed"), "{error}");
        assert!(!fixture.install().exists());
        assert!(!fixture.install().with_extension("partial").exists());
    }

    #[test]
    fn a_build_without_the_meld_binary_is_not_installed() {
        let mut fixture = Fixture::new();
        let empty = fixture.tmp.path().join("empty-source");
        std::fs::create_dir_all(&empty).unwrap();
        fixture.stack = fixture.stack.clone().with_git_clone(&empty);

        let error = fixture.run().unwrap_err().to_string();

        assert!(error.contains("left no"), "{error}");
        assert!(!fixture.install().exists());
    }

    #[test]
    fn meld_gets_the_realm_address_and_the_token_file_but_never_the_token() {
        let fixture = Fixture::new();
        fixture.installed();
        let data = fixture.tmp.path().join("wow/Data");
        std::fs::create_dir_all(&data).unwrap();
        for archive in ["dbc.MPQ", "terrain.MPQ", "model.MPQ", "wmo.MPQ"] {
            std::fs::write(data.join(archive), "").unwrap();
        }
        Config {
            client_data: Some(data.to_string_lossy().into_owned()),
        }
        .save(&fixture.project.config_file())
        .unwrap();

        fixture.run().unwrap();

        let launch = fixture.launch();
        let token_file = std::fs::canonicalize(fixture.project.token_file()).unwrap();
        let wow = std::fs::canonicalize(fixture.tmp.path().join("wow")).unwrap();
        assert_eq!(
            launch.env_value("MELD_LYRACORE_URL"),
            Some("http://127.0.0.1:3000")
        );
        assert_eq!(launch.env_value("MELD_LYRACORE_DATABASE"), Some("lyracore"));
        assert_eq!(
            launch.env_value("MELD_LYRACORE_TOKEN_FILE"),
            Some(token_file.to_string_lossy().as_ref())
        );
        assert_eq!(
            launch.env_value("MELD_WOW_PATH"),
            Some(wow.to_string_lossy().as_ref())
        );
        assert!(launch.args().is_empty());
        assert!(!format!("{launch:?}").contains(TOKEN));
    }

    #[test]
    fn meld_gets_no_wow_path_when_client_data_is_unset() {
        let fixture = Fixture::new();
        fixture.installed();

        fixture.run().unwrap();

        assert_eq!(fixture.launch().env_value("MELD_WOW_PATH"), None);
    }
}
