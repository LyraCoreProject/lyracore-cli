use super::native::NativeMode;
use super::*;
use crate::cmd::doctor::parse_version;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

const POLL: Duration = Duration::from_millis(200);
const DEBOUNCE: Duration = Duration::from_millis(400);

type Snapshot = BTreeMap<PathBuf, blake3::Hash>;

struct PublishedModule {
    host: ProcessRecord,
    hash: String,
}

struct Reload {
    roots: Vec<PathBuf>,
    published: Option<PublishedModule>,
}

impl DevManager {
    pub fn watch(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        http: &dyn HttpClient,
    ) -> Result<()> {
        require_reload_checkout(&self.project, runner)?;
        self.project.ensure_dirs()?;
        let _lock = watcher_lock(&self.project)?;
        let mut initialize = self.up_native(
            runner,
            inspector,
            http,
            ClientBind::Loopback,
            NativeMode::HotReload,
        )?;
        let mut reload = Reload::new(&self.project);
        let mut observed = snapshot(&reload.roots)?;
        let mut pending = true;
        let mut changed_at = Instant::now() - DEBOUNCE;
        println!("Watching Module source. Ctrl+C stops watch and leaves the Native Host running.");
        loop {
            let current = snapshot(&reload.roots)?;
            if current != observed {
                observed = current;
                changed_at = Instant::now();
                pending = true;
            }
            if pending && changed_at.elapsed() >= DEBOUNCE {
                match reload.update(&self.project, runner, inspector) {
                    Ok(false) => {
                        changed_at = Instant::now();
                    }
                    Ok(true) => {
                        if let Some(credential) = initialize.as_ref() {
                            self.initialize_native(runner, inspector, http, credential)?;
                            initialize = None;
                        }
                        pending = false;
                    }
                    Err(error) => {
                        eprintln!("{error}\nWaiting for another source change.");
                        pending = false;
                    }
                }
            }
            sleep(POLL);
        }
    }
}

fn require_reload_checkout(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
    if !project.has_native_host() || !project.root.join("native-host/dev-reload.patch").is_file() {
        return Err(Error::PrerequisiteMissing(
            "dev watch requires a Native Host checkout with native-host/dev-reload.patch".into(),
        ));
    }
    let manifest = fs::read_to_string(project.module_manifest())?;
    let expected = preflight::pinned_spacetimedb(&manifest).ok_or_else(|| {
        Error::PrerequisiteMissing("cannot read the pinned spacetime version".into())
    })?;
    let found = runner.run_capturing_stderr(&CommandSpec::new("spacetime").arg("--version"))?;
    if parse_version(&found) != Some(expected) {
        return Err(Error::PrerequisiteMissing(format!(
            "dev watch requires spacetime {expected}; found {}",
            found.trim()
        )));
    }
    Ok(())
}

fn watcher_lock(project: &ProjectLayout) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(project.state_dir.join("dev-watch.lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |error| Error::Process(format!("another dev watch owns this checkout: {error}")),
    )?;
    Ok(file)
}

fn target_dir(project: &ProjectLayout) -> PathBuf {
    project.state_dir.join("dev-reload-target")
}

fn wasm_path(project: &ProjectLayout) -> PathBuf {
    target_dir(project).join("wasm32-unknown-unknown/dev-reload/lyracore_module.wasm")
}

fn build_command(project: &ProjectLayout) -> CommandSpec {
    preflight::module_command(project, "cargo")
        .arg("build")
        .arg("--locked")
        .arg("-p")
        .arg(ProjectLayout::MODULE_PACKAGE)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--profile")
        .arg("dev-reload")
        .arg(ProjectLayout::DEPLOY_FEATURES)
        .env("CARGO_TARGET_DIR", target_dir(project).to_string_lossy())
}

fn publish_command(
    project: &ProjectLayout,
    inspector: &dyn ProcessInspector,
) -> Result<CommandSpec> {
    publish::require_native_host(project, inspector, &ProjectLayout::stdb_uri())?;
    let state = RuntimeState::load(&project.state_file())?;
    if !state.native_hot_reload {
        return Err(Error::Usage(
            "dev watch requires a development Native Host; run `lyracore dev down` first".into(),
        ));
    }
    Ok(CommandSpec::new("spacetime")
        .arg("--config-path")
        .arg(project.native_cli_config().to_string_lossy())
        .arg("publish")
        .arg("--bin-path")
        .arg(wasm_path(project).to_string_lossy())
        .arg("--delete-data=never")
        .arg("--yes=migrate,skip-login")
        .arg("--no-config")
        .arg("-s")
        .arg(ProjectLayout::stdb_uri())
        .arg(ProjectLayout::DATABASE)
        .cwd(&project.root))
}

impl Reload {
    fn new(project: &ProjectLayout) -> Self {
        Self {
            roots: source_roots(project),
            published: None,
        }
    }

    /// False means sources changed during compilation; retry after they settle.
    fn update(
        &mut self,
        project: &ProjectLayout,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
    ) -> Result<bool> {
        let total = Instant::now();
        self.refresh_roots(project, runner)?;
        let before = snapshot(&self.roots)?;
        println!("Building development Module...");
        let build = Instant::now();
        let (built, stale) = monitored_build(&self.roots, before, || {
            runner.run_streaming(&build_command(project))
        })?;
        let build_time = build.elapsed();
        if stale {
            println!(
                "Sources changed during build; discarding candidate. Build {:.2}s, publish 0.00s, total {:.2}s.",
                build_time.as_secs_f64(), total.elapsed().as_secs_f64()
            );
            return Ok(false);
        }
        built.map_err(|error| Error::Process(format!(
            "{error}\nBuild failed; no Module was published. Build {:.2}s, publish 0.00s, total {:.2}s.",
            build_time.as_secs_f64(), total.elapsed().as_secs_f64()
        )))?;
        let hash = blake3::hash(&fs::read(wasm_path(project))?)
            .to_hex()
            .to_string();
        let command = publish_command(project, inspector)?;
        let host = RuntimeState::load(&project.state_file())?
            .spacetime
            .ok_or_else(|| Error::State("Native Host record disappeared".into()))?;
        if self
            .published
            .as_ref()
            .is_some_and(|published| published.host == host && published.hash == hash)
        {
            println!(
                "WASM unchanged; skipped publish. Build {:.2}s, publish 0.00s, total {:.2}s.",
                build_time.as_secs_f64(),
                total.elapsed().as_secs_f64()
            );
            return Ok(true);
        }
        self.published = None;
        println!("Publishing development Module to the local Native Host...");
        let publish = Instant::now();
        runner.run_streaming(&command).map_err(|error| Error::Process(format!(
            "{error}\nPublish failed; the HTTP outcome may be unknown. Inspect the Native Host log before assuming the previous Module is active. Build {:.2}s, publish {:.2}s, total {:.2}s.",
            build_time.as_secs_f64(), publish.elapsed().as_secs_f64(), total.elapsed().as_secs_f64()
        )))?;
        let published = PublishedModule { host, hash };
        self.published = Some(published);
        println!(
            "Module updated. Build {:.2}s, publish {:.2}s, total {:.2}s.",
            build_time.as_secs_f64(),
            publish.elapsed().as_secs_f64(),
            total.elapsed().as_secs_f64()
        );
        Ok(true)
    }

    fn refresh_roots(&mut self, project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<()> {
        let output = runner.run_and_wait(
            &CommandSpec::new("cargo")
                .arg("metadata")
                .arg("--locked")
                .arg("--format-version=1")
                .arg("--filter-platform=wasm32-unknown-unknown")
                .cwd(&project.root),
        )?;
        let metadata: serde_json::Value = serde_json::from_str(&output)?;
        let packages = metadata["packages"]
            .as_array()
            .ok_or_else(|| Error::Process("cargo metadata returned no packages".into()))?;
        let mut roots: BTreeSet<_> = source_roots(project).into_iter().collect();
        for package in packages
            .iter()
            .filter(|package| package["source"].is_null())
        {
            if let Some(manifest) = package["manifest_path"].as_str() {
                if let Some(directory) = Path::new(manifest).parent() {
                    roots.insert(directory.to_path_buf());
                }
            }
        }
        self.roots = roots.into_iter().collect();
        Ok(())
    }
}

fn monitored_build(
    roots: &[PathBuf],
    before: Snapshot,
    build: impl FnOnce() -> Result<()>,
) -> Result<(Result<()>, bool)> {
    std::thread::scope(|scope| {
        let (stop, receive) = mpsc::channel();
        let monitor = scope.spawn(move || -> Result<bool> {
            let mut stale = false;
            loop {
                let finished = receive.recv_timeout(POLL) != Err(mpsc::RecvTimeoutError::Timeout);
                stale |= snapshot(roots)? != before;
                if finished {
                    return Ok(stale);
                }
            }
        });
        let built = build();
        let _ = stop.send(());
        let stale = monitor
            .join()
            .map_err(|_| Error::Process("source monitor stopped unexpectedly".into()))??;
        Ok((built, stale))
    })
}

fn source_roots(project: &ProjectLayout) -> Vec<PathBuf> {
    [
        "module",
        "crates",
        "packages",
        "build_support",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "rust-toolchain",
        ".cargo/config",
        ".cargo/config.toml",
        "datascripts/runtime-scripts/events.json",
    ]
    .iter()
    .map(|path| project.root.join(path))
    .collect()
}

fn snapshot(roots: &[PathBuf]) -> Result<Snapshot> {
    fn ignored(path: &Path) -> bool {
        path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            name.starts_with('.')
                || name == "target"
                || name.starts_with("target-")
                || name == "node_modules"
        })
    }
    fn visit(path: &Path, visited: &mut BTreeSet<PathBuf>, files: &mut Snapshot) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            files.insert(
                path.to_path_buf(),
                blake3::hash(fs::read_link(path)?.as_os_str().as_encoded_bytes()),
            );
        }
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        if canonical.components().any(|part| {
            let name = part.as_os_str().to_string_lossy();
            name == ".git"
                || name == ".lyracore"
                || name == "target"
                || name.starts_with("target-")
                || name == "node_modules"
        }) || !visited.insert(canonical)
        {
            return Ok(());
        }
        if path.is_dir() {
            let mut entries = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort();
            for entry in entries {
                if !ignored(Path::new(entry.file_name().unwrap_or_default())) {
                    visit(&entry, visited, files)?;
                }
            }
        } else if path.is_file() {
            let name = path.file_name().unwrap_or_default();
            if path.extension().is_some_and(|ext| ext == "rs")
                || [
                    "Cargo.toml",
                    "Cargo.lock",
                    "rust-toolchain.toml",
                    "rust-toolchain",
                    "events.json",
                ]
                .iter()
                .any(|wanted| name == *wanted)
                || ["config", "config.toml"]
                    .iter()
                    .any(|wanted| name == *wanted)
                || path
                    .components()
                    .any(|part| part.as_os_str() == "build_support")
            {
                match fs::read(path) {
                    Ok(bytes) => {
                        files.insert(path.to_path_buf(), blake3::hash(&bytes));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    let mut visited = BTreeSet::new();
    for root in roots {
        visit(root, &mut visited, &mut files)?;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeHttp;
    use crate::proc::fake::{Call, FakeStack, FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
    use std::cell::Cell;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        dev: DevManager,
        stack: FakeStack,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::write(
                root.join("rust-toolchain.toml"),
                format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
            )
            .unwrap();
            fs::create_dir_all(root.join("module/src")).unwrap();
            fs::write(root.join("module/src/lib.rs"), "fn before() {}\n").unwrap();
            fs::write(
                root.join("module/Cargo.toml"),
                format!("spacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"),
            )
            .unwrap();
            fs::create_dir_all(root.join("native-host")).unwrap();
            fs::write(root.join("native-host/prepare-host.py"), "").unwrap();
            fs::write(root.join("native-host/dev-reload.patch"), "patch").unwrap();
            let project = ProjectLayout::from_root(root).unwrap();
            let stack = FakeStack::new().with_stdout("cargo metadata", r#"{"packages":[]}"#);
            Self {
                _tmp: tmp,
                dev: DevManager::new(project).unwrap(),
                stack,
            }
        }

        fn start(&mut self) {
            self.dev
                .up_native(
                    &self.stack.runner(),
                    &self.stack.inspector(),
                    &FakeHttp::new(),
                    ClientBind::Loopback,
                    NativeMode::HotReload,
                )
                .unwrap();
        }

        fn project(&self) -> &ProjectLayout {
            &self.dev.project
        }

        fn publish_count(&self) -> usize {
            self.stack
                .rendered()
                .iter()
                .filter(|command| command.contains(" publish "))
                .count()
        }

        fn compiler(
            &self,
            build: impl Fn() -> Result<()>,
        ) -> Compiler<'_, impl Fn() -> Result<()>> {
            Compiler {
                stack: &self.stack,
                build,
            }
        }

        fn artifact(&self, bytes: &[u8]) -> Result<()> {
            let path = wasm_path(self.project());
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, bytes)?;
            Ok(())
        }
    }

    struct Compiler<'a, F> {
        stack: &'a FakeStack,
        build: F,
    }

    impl<F: Fn() -> Result<()>> ProcessRunner for Compiler<'_, F> {
        fn run_and_wait(&self, command: &CommandSpec) -> Result<String> {
            self.stack.runner().run_and_wait(command)
        }
        fn run_capturing_stderr(&self, command: &CommandSpec) -> Result<String> {
            self.stack.runner().run_capturing_stderr(command)
        }
        fn run_with_secret_stdin(&self, command: &CommandSpec, secret: &[u8]) -> Result<String> {
            self.stack.runner().run_with_secret_stdin(command, secret)
        }
        fn run_streaming(&self, command: &CommandSpec) -> Result<()> {
            self.stack.runner().run_streaming(command)?;
            if command.program() == "cargo"
                && command.args().first().is_some_and(|arg| arg == "build")
            {
                (self.build)()?;
            }
            Ok(())
        }
        fn spawn_logged(&self, command: &CommandSpec, log: &Path) -> Result<u32> {
            self.stack.runner().spawn_logged(command, log)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.stack.runner().terminate(pid)
        }
    }

    #[test]
    fn watch_start_records_development_mode_and_never_builds_release_module() {
        let mut fixture = Fixture::new();
        fixture.start();
        let state = RuntimeState::load(&fixture.project().state_file()).unwrap();
        assert!(state.native_hot_reload);
        assert!(state.native_runtime);
        let calls = fixture.stack.calls();
        let host = calls
            .iter()
            .find_map(|call| match call {
                Call::Spawn { spec, .. } => Some(spec),
                _ => None,
            })
            .unwrap();
        assert_eq!(host.env_value("LYRACORE_NATIVE_DEV"), Some("1"));
        assert_eq!(fixture.publish_count(), 0);
        assert!(!fixture
            .stack
            .rendered()
            .iter()
            .any(|command| command.contains("cargo build")));
        fixture
            .dev
            .up(
                &fixture.stack.runner(),
                &fixture.stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                Topology::Single,
            )
            .unwrap();
        assert!(
            RuntimeState::load(&fixture.project().state_file())
                .unwrap()
                .native_hot_reload
        );
    }

    #[test]
    fn watch_refuses_a_running_normal_host_without_restarting_it() {
        let mut fixture = Fixture::new();
        fixture
            .dev
            .up_native(
                &fixture.stack.runner(),
                &fixture.stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                NativeMode::Normal,
            )
            .unwrap();
        let before = fixture.stack.calls().len();
        let record = fixture.dev.state.spacetime.clone();
        let error = fixture
            .dev
            .watch(
                &fixture.stack.runner(),
                &fixture.stack.inspector(),
                &FakeHttp::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("dev down"), "{error}");
        assert_eq!(fixture.dev.state.spacetime, record);
        assert!(!fixture.dev.state.native_hot_reload);
        assert!(!fixture.stack.calls()[before..].iter().any(|call| matches!(
            call,
            Call::Spawn { .. } | Call::Stream(_) | Call::Terminate(_)
        )));
    }

    #[test]
    fn old_checkouts_and_wrong_spacetime_versions_fail_before_startup() {
        let mut fixture = Fixture::new();
        fs::remove_file(fixture.project().root.join("native-host/dev-reload.patch")).unwrap();
        let error = fixture
            .dev
            .watch(
                &fixture.stack.runner(),
                &fixture.stack.inspector(),
                &FakeHttp::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("dev-reload.patch"));
        assert!(fixture.stack.calls().is_empty());
        fs::write(
            fixture.project().root.join("native-host/dev-reload.patch"),
            "patch",
        )
        .unwrap();
        let stack = fixture
            .stack
            .clone()
            .with_stdout("spacetime --version", "spacetime 99.0.0");
        let error = fixture
            .dev
            .watch(&stack.runner(), &stack.inspector(), &FakeHttp::new())
            .unwrap_err();
        assert!(error.to_string().contains("requires spacetime"));
        assert!(fixture.dev.state.spacetime.is_none());
    }

    #[test]
    fn only_one_watcher_can_own_a_checkout_and_lock_releases_on_exit() {
        let fixture = Fixture::new();
        fixture.project().ensure_dirs().unwrap();
        let first = watcher_lock(fixture.project()).unwrap();
        assert!(watcher_lock(fixture.project()).is_err());
        drop(first);
        assert!(watcher_lock(fixture.project()).is_ok());
    }

    #[test]
    fn development_commands_pin_artifact_target_identity_and_non_destructive_publish_flags() {
        let mut fixture = Fixture::new();
        fixture.start();
        let project = fixture.project();
        let build = build_command(project);
        assert_eq!(
            build.args(),
            [
                "build",
                "--locked",
                "-p",
                "lyracore-module",
                "--target",
                "wasm32-unknown-unknown",
                "--profile",
                "dev-reload",
                "--features=debug_reducers"
            ]
        );
        assert_eq!(build.cwd_value(), Some(project.root.as_path()));
        assert_eq!(
            build.env_value("CARGO_TARGET_DIR"),
            Some(target_dir(project).to_str().unwrap())
        );
        assert!(build.removes_env("RUSTFLAGS"));
        let publish = publish_command(project, &fixture.stack.inspector()).unwrap();
        assert_eq!(publish.program(), "spacetime");
        assert_eq!(
            publish.args(),
            [
                "--config-path",
                project.native_cli_config().to_str().unwrap(),
                "publish",
                "--bin-path",
                wasm_path(project).to_str().unwrap(),
                "--delete-data=never",
                "--yes=migrate,skip-login",
                "--no-config",
                "-s",
                "http://127.0.0.1:3000",
                "lyracore"
            ]
        );
        assert!(!publish
            .args()
            .iter()
            .any(|arg| arg == "--yes" || arg == "-c" || arg == "local"));
        let record = fixture.dev.state.spacetime.clone().unwrap();
        fixture.stack.runner().terminate(record.pid).unwrap();
        assert!(publish_command(project, &fixture.stack.inspector()).is_err());
    }

    #[test]
    fn unchanged_wasm_skips_publish_but_new_watcher_checks_the_existing_module() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project());
        let compiler = fixture.compiler(|| fixture.artifact(b"wasm A"));
        assert!(reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap());
        assert!(reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap());
        assert_eq!(fixture.publish_count(), 1);
        assert!(Reload::new(fixture.project())
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap());
        assert_eq!(fixture.publish_count(), 2);
    }

    #[test]
    fn compilation_failure_retains_the_previous_module_and_next_edit_can_publish() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project());
        let fail = Cell::new(false);
        let compiler = fixture.compiler(|| {
            if fail.get() {
                Err(Error::Process("compile error".into()))
            } else {
                fixture.artifact(b"working wasm")
            }
        });
        reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap();
        fail.set(true);
        let error = reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap_err();
        assert!(error.to_string().contains("no Module was published"));
        assert_eq!(fixture.publish_count(), 1);
        assert_eq!(
            reload.published.as_ref().unwrap().hash,
            blake3::hash(b"working wasm").to_hex().to_string()
        );
        fail.set(false);
        reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap();
        assert_eq!(fixture.publish_count(), 1);
        let changed = fixture.compiler(|| fixture.artifact(b"fixed wasm"));
        reload
            .update(fixture.project(), &changed, &fixture.stack.inspector())
            .unwrap();
        assert_eq!(fixture.publish_count(), 2);
    }

    #[test]
    fn a_source_change_during_build_discards_the_artifact_and_rebuilds_latest() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project());
        let source = fixture.project().module_sources().join("lib.rs");
        let compiler = fixture.compiler(|| {
            fixture.artifact(b"stale wasm")?;
            fs::write(&source, "fn latest() {}\n")?;
            Ok(())
        });
        assert!(!reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap());
        assert_eq!(fixture.publish_count(), 0);
        let latest = fixture.compiler(|| fixture.artifact(b"latest wasm"));
        assert!(reload
            .update(fixture.project(), &latest, &fixture.stack.inspector())
            .unwrap());
        assert_eq!(fixture.publish_count(), 1);
        assert_eq!(
            reload.published.as_ref().unwrap().hash,
            blake3::hash(b"latest wasm").to_hex().to_string()
        );
    }

    #[test]
    fn source_saved_then_restored_during_compilation_still_discards_the_candidate() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project());
        let source = fixture.project().module_sources().join("lib.rs");
        let original = fs::read(&source).unwrap();
        let compiler = fixture.compiler(|| {
            fs::write(&source, "fn temporary() {}\n")?;
            sleep(POLL * 3);
            fs::write(&source, &original)?;
            fixture.artifact(b"temporary wasm")
        });
        assert!(!reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap());
        assert_eq!(fixture.publish_count(), 0);
    }

    #[test]
    fn publish_failure_forgets_the_confirmed_hash_because_http_outcome_can_be_unknown() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project());
        let compiler = fixture.compiler(|| fixture.artifact(b"old wasm"));
        reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap();
        let failed = FakeStack::new()
            .with_stdout("cargo metadata", r#"{"packages":[]}"#)
            .fail_on(" publish ", "connection reset");
        let changed = Compiler {
            stack: &failed,
            build: || fixture.artifact(b"new wasm"),
        };
        let error = reload
            .update(fixture.project(), &changed, &fixture.stack.inspector())
            .unwrap_err();
        assert!(error.to_string().contains("outcome may be unknown"));
        assert!(reload.published.is_none());
        reload
            .update(fixture.project(), &compiler, &fixture.stack.inspector())
            .unwrap();
        assert_eq!(fixture.publish_count(), 2);
    }

    #[test]
    fn local_dependency_and_build_inputs_are_watched_and_outputs_are_ignored() {
        let fixture = Fixture::new();
        let root = &fixture.project().root;
        fs::create_dir_all(root.join("module/build_support")).unwrap();
        fs::write(root.join("module/build_support/input.txt"), "A").unwrap();
        fs::create_dir_all(root.join("datascripts/runtime-scripts")).unwrap();
        fs::write(root.join("datascripts/runtime-scripts/events.json"), "[]").unwrap();
        let external = TempDir::new().unwrap();
        fs::write(external.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(external.path().join("lib.rs"), "fn external() {}").unwrap();
        let metadata = serde_json::json!({"packages": [{"manifest_path": external.path().join("Cargo.toml"), "source": null}]});
        let stack = fixture
            .stack
            .clone()
            .with_stdout("cargo metadata", &metadata.to_string());
        let mut reload = Reload::new(fixture.project());
        reload
            .refresh_roots(fixture.project(), &stack.runner())
            .unwrap();
        let before = snapshot(&reload.roots).unwrap();
        for path in [
            root.join("module/build_support/input.txt"),
            root.join("datascripts/runtime-scripts/events.json"),
            external.path().join("lib.rs"),
            external.path().join("Cargo.toml"),
        ] {
            assert!(before.contains_key(&path), "{}", path.display());
        }
        fs::create_dir_all(root.join("module/target/out")).unwrap();
        fs::write(root.join("module/target/out/generated.rs"), "generated").unwrap();
        assert_eq!(before, snapshot(&reload.roots).unwrap());
        fs::write(external.path().join("lib.rs"), "fn changed() {}").unwrap();
        assert_ne!(before, snapshot(&reload.roots).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn package_symlinks_follow_source_without_following_cycles_or_output_aliases() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let external = TempDir::new().unwrap();
        fs::create_dir_all(external.path().join("src")).unwrap();
        fs::write(external.path().join("src/mod.rs"), "fn package() {}").unwrap();
        fs::create_dir_all(fixture.project().packages_dir()).unwrap();
        symlink(
            external.path(),
            fixture.project().packages_dir().join("linked"),
        )
        .unwrap();
        symlink(external.path(), external.path().join("src/cycle")).unwrap();
        symlink("loop", external.path().join("src/loop")).unwrap();
        let roots = source_roots(fixture.project());
        let before = snapshot(&roots).unwrap();
        assert!(before.contains_key(&fixture.project().packages_dir().join("linked/src/mod.rs")));
        fs::create_dir_all(external.path().join("target")).unwrap();
        fs::write(external.path().join("target/generated.rs"), "A").unwrap();
        symlink(
            external.path().join("target"),
            external.path().join("output-alias"),
        )
        .unwrap();
        let with_alias = snapshot(&roots).unwrap();
        fs::write(external.path().join("target/generated.rs"), "B").unwrap();
        assert_eq!(with_alias, snapshot(&roots).unwrap());
        fs::write(external.path().join("src/mod.rs"), "fn changed() {}").unwrap();
        assert_ne!(with_alias, snapshot(&roots).unwrap());
    }
}
