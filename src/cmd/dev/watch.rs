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

struct Reload {
    roots: Vec<PathBuf>,
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
        let mut reload = Reload::new(&self.project, runner)?;
        let mut observed = snapshot(&self.project.root, &reload.roots)?;
        let mut pending = true;
        let mut changed_at = Instant::now() - DEBOUNCE;
        println!("Watching Module source. Ctrl+C stops watch and leaves the Native Host running.");
        loop {
            let current = match snapshot(&self.project.root, &reload.roots) {
                Ok(current) => current,
                Err(error) => {
                    eprintln!("Cannot read watched source: {error}. Retrying.");
                    sleep(POLL);
                    continue;
                }
            };
            if current != observed {
                observed = current;
                changed_at = Instant::now();
                pending = true;
            }
            if pending && changed_at.elapsed() >= DEBOUNCE {
                match reload.update(&self.project, runner, inspector, http) {
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
        .arg("rustc")
        .arg("--locked")
        .arg("--lib")
        .arg("--crate-type=cdylib")
        .arg("-p")
        .arg(ProjectLayout::MODULE_PACKAGE)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--profile")
        .arg("dev-reload")
        .arg(ProjectLayout::DEPLOY_FEATURES)
        .env("CARGO_TARGET_DIR", target_dir(project).to_string_lossy())
}

fn publish_module(
    project: &ProjectLayout,
    inspector: &dyn ProcessInspector,
    http: &dyn HttpClient,
) -> Result<()> {
    publish::require_native_host(project, inspector, &ProjectLayout::stdb_uri())?;
    let state = RuntimeState::load(&project.state_file())?;
    if !state.native_hot_reload {
        return Err(Error::Usage(
            "dev watch requires Development reload; run `lyracore dev down` first".into(),
        ));
    }
    let credential = crate::token::native_owner(project)?;
    let wasm = fs::read(wasm_path(project))?;
    let url = format!(
        "{}/v1/database/{}?host_type=Wasm&clear=false&policy=Compatible",
        ProjectLayout::stdb_uri(),
        ProjectLayout::DATABASE
    );
    http.publish_module(&url, credential.token(), &wasm)
}

impl Reload {
    fn new(project: &ProjectLayout, runner: &dyn ProcessRunner) -> Result<Self> {
        let mut reload = Self {
            roots: source_roots(project),
        };
        reload.refresh_roots(project, runner)?;
        Ok(reload)
    }

    /// False means sources changed during compilation; retry after they settle.
    fn update(
        &mut self,
        project: &ProjectLayout,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        http: &dyn HttpClient,
    ) -> Result<bool> {
        let total = Instant::now();
        self.refresh_roots(project, runner)?;
        let before = snapshot(&project.root, &self.roots)?;
        println!("Building development Module...");
        let build = Instant::now();
        let (built, stale) = monitored_build(&project.root, &self.roots, before, || {
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
        println!("Publishing development Module to the local Native Host...");
        let publish = Instant::now();
        publish_module(project, inspector, http).map_err(|error| Error::Process(format!(
            "{error}\nPublish failed. The previous Module remains installed after a compatibility refusal. For Module definition changes, see docs/quickstart.md#development-reload in this Core checkout. Build {:.2}s, publish {:.2}s, total {:.2}s.",
            build_time.as_secs_f64(), publish.elapsed().as_secs_f64(), total.elapsed().as_secs_f64()
        )))?;
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
        let module = packages
            .iter()
            .find(|package| package["name"] == ProjectLayout::MODULE_PACKAGE)
            .and_then(|package| package["id"].as_str())
            .ok_or_else(|| Error::Process("cargo metadata returned no lyracore-module".into()))?;
        let nodes = metadata["resolve"]["nodes"]
            .as_array()
            .ok_or_else(|| Error::Process("cargo metadata returned no dependency graph".into()))?;
        let nodes: BTreeMap<_, _> = nodes
            .iter()
            .filter_map(|node| node["id"].as_str().map(|id| (id, node)))
            .collect();
        let mut selected = BTreeSet::new();
        let mut pending = vec![module];
        while let Some(id) = pending.pop() {
            if !selected.insert(id) {
                continue;
            }
            if let Some(dependencies) = nodes.get(id).and_then(|node| node["deps"].as_array()) {
                pending.extend(
                    dependencies
                        .iter()
                        .filter(|dependency| {
                            dependency["dep_kinds"]
                                .as_array()
                                .is_some_and(|kinds| kinds.iter().any(|kind| kind["kind"] != "dev"))
                        })
                        .filter_map(|dependency| dependency["pkg"].as_str()),
                );
            }
        }
        for package in packages.iter().filter(|package| {
            package["source"].is_null()
                && package["id"]
                    .as_str()
                    .is_some_and(|id| selected.contains(id))
        }) {
            if let Some(directory) = package["manifest_path"]
                .as_str()
                .and_then(|path| Path::new(path).parent())
            {
                roots.insert(directory.to_path_buf());
            }
        }
        self.roots = roots.into_iter().collect();
        Ok(())
    }
}

fn monitored_build(
    project_root: &Path,
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
                stale |= snapshot(project_root, roots)? != before;
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

fn snapshot(project_root: &Path, roots: &[PathBuf]) -> Result<Snapshot> {
    fn excluded(path: &Path) -> bool {
        path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            name == ".git"
                || name == ".lyracore"
                || name == "target"
                || name.starts_with("target-")
                || name == "node_modules"
        })
    }
    fn absent(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    }
    fn visit(
        path: &Path,
        project_root: &Path,
        source_root: &Path,
        visited: &mut BTreeSet<PathBuf>,
        files: &mut Snapshot,
    ) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if absent(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            let target = match fs::read_link(path) {
                Ok(target) => target,
                Err(error)
                    if absent(&error) || error.kind() == std::io::ErrorKind::InvalidInput =>
                {
                    return Ok(())
                }
                Err(error) => return Err(error.into()),
            };
            files.insert(
                path.to_path_buf(),
                blake3::hash(target.as_os_str().as_encoded_bytes()),
            );
        }
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(error)
                if absent(&error)
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        if [project_root, source_root]
            .iter()
            .any(|root| canonical.strip_prefix(root).is_ok_and(excluded))
            || !visited.insert(canonical.clone())
        {
            return Ok(());
        }
        let source_root =
            if canonical.starts_with(source_root) || canonical.starts_with(project_root) {
                source_root
            } else if path.is_dir() {
                canonical.as_path()
            } else {
                canonical.parent().unwrap_or(&canonical)
            };
        if path.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if absent(&error) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let mut paths = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => paths.push(entry.path()),
                    Err(error) if absent(&error) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            paths.sort();
            for entry in paths {
                let name = entry.file_name().unwrap_or_default().to_string_lossy();
                if !name.starts_with('.') && !excluded(Path::new(name.as_ref())) {
                    visit(&entry, project_root, source_root, visited, files)?;
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
                    "config",
                    "config.toml",
                ]
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
                    Err(error) if absent(&error) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
    let project_root = project_root.canonicalize()?;
    let mut files = BTreeMap::new();
    let mut visited = BTreeSet::new();
    for root in roots {
        visit(root, &project_root, &project_root, &mut visited, &mut files)?;
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

    fn module_metadata(project: &ProjectLayout) -> serde_json::Value {
        serde_json::json!({
            "packages": [{"id": "module", "name": ProjectLayout::MODULE_PACKAGE,
                "manifest_path": project.module_manifest(), "source": null}],
            "resolve": {"nodes": [{"id": "module", "deps": []}]}
        })
    }

    struct Fixture {
        _tmp: TempDir,
        dev: DevManager,
        stack: FakeStack,
        http: FakeHttp,
    }

    impl Fixture {
        fn new() -> Self {
            Self::from_directory(TempDir::new().unwrap())
        }

        fn from_directory(tmp: TempDir) -> Self {
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
            fs::write(
                root.join("native-host/rust-toolchain.toml"),
                "[toolchain]\nchannel = \"1.97.1\"\n",
            )
            .unwrap();
            fs::write(root.join("native-host/dev-reload.patch"), "patch").unwrap();
            let project = ProjectLayout::from_root(root).unwrap();
            let stack = FakeStack::new()
                .with_stdout("cargo metadata", &module_metadata(&project).to_string());
            Self {
                _tmp: tmp,
                dev: DevManager::new(project).unwrap(),
                stack,
                http: FakeHttp::new(),
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
            self.http
                .requests()
                .iter()
                .filter(|request| request.method == "PUT")
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
                && command.args().first().is_some_and(|arg| arg == "rustc")
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
                "rustc",
                "--locked",
                "--lib",
                "--crate-type=cdylib",
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
        fixture.artifact(b"module").unwrap();
        publish_module(project, &fixture.stack.inspector(), &fixture.http).unwrap();
        let requests = fixture.http.requests();
        let publish = requests.last().unwrap();
        assert_eq!(publish.method, "PUT");
        assert_eq!(publish.url, "http://127.0.0.1:3000/v1/database/lyracore?host_type=Wasm&clear=false&policy=Compatible");
        assert_eq!(publish.body, "module");
        assert_eq!(
            publish.bearer.as_deref(),
            Some(crate::proc::fake::FAKE_TOKEN)
        );
        let record = fixture.dev.state.spacetime.clone().unwrap();
        fixture.stack.runner().terminate(record.pid).unwrap();
        assert!(publish_module(project, &fixture.stack.inspector(), &fixture.http).is_err());
        assert_eq!(fixture.publish_count(), 1);
    }

    #[test]
    fn missing_or_mismatched_publisher_files_stop_before_upload() {
        let mut fixture = Fixture::new();
        fixture.start();
        fixture.artifact(b"module").unwrap();
        let config = fixture.project().native_cli_config();
        let original = fs::read(&config).unwrap();
        fs::write(&config, "spacetimedb_token = \"other publisher\"").unwrap();
        assert!(
            publish_module(fixture.project(), &fixture.stack.inspector(), &fixture.http)
                .unwrap_err()
                .to_string()
                .contains("disagree")
        );
        fs::write(&config, original).unwrap();
        fs::remove_file(fixture.project().token_file()).unwrap();
        assert!(
            publish_module(fixture.project(), &fixture.stack.inspector(), &fixture.http).is_err()
        );
        assert_eq!(fixture.publish_count(), 0);
    }

    #[test]
    fn every_successful_build_publishes_even_after_a_manual_publish() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let compiler = fixture.compiler(|| fixture.artifact(b"wasm A"));
        assert!(reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http
            )
            .unwrap());
        fixture.artifact(b"manual wasm").unwrap();
        publish_module(fixture.project(), &fixture.stack.inspector(), &fixture.http).unwrap();
        assert!(reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http
            )
            .unwrap());
        assert_eq!(fixture.publish_count(), 3);
        assert_eq!(fs::read(wasm_path(fixture.project())).unwrap(), b"wasm A");
    }

    #[test]
    fn compilation_failure_retains_the_previous_module_and_next_edit_can_publish() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let fail = Cell::new(false);
        let compiler = fixture.compiler(|| {
            if fail.get() {
                Err(Error::Process("compile error".into()))
            } else {
                fixture.artifact(b"working wasm")
            }
        });
        reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http,
            )
            .unwrap();
        fail.set(true);
        let error = reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http,
            )
            .unwrap_err();
        assert!(error.to_string().contains("no Module was published"));
        assert_eq!(fixture.publish_count(), 1);
        fail.set(false);
        reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http,
            )
            .unwrap();
        assert_eq!(fixture.publish_count(), 2);
        let changed = fixture.compiler(|| fixture.artifact(b"fixed wasm"));
        reload
            .update(
                fixture.project(),
                &changed,
                &fixture.stack.inspector(),
                &fixture.http,
            )
            .unwrap();
        assert_eq!(fixture.publish_count(), 3);
    }

    #[test]
    fn a_source_change_during_build_discards_the_artifact_and_rebuilds_latest() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let source = fixture.project().module_sources().join("lib.rs");
        let compiler = fixture.compiler(|| {
            fixture.artifact(b"stale wasm")?;
            fs::write(&source, "fn latest() {}\n")?;
            Ok(())
        });
        assert!(!reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http
            )
            .unwrap());
        assert_eq!(fixture.publish_count(), 0);
        let latest = fixture.compiler(|| fixture.artifact(b"latest wasm"));
        assert!(reload
            .update(
                fixture.project(),
                &latest,
                &fixture.stack.inspector(),
                &fixture.http
            )
            .unwrap());
        assert_eq!(fixture.publish_count(), 1);
        assert_eq!(
            fs::read(wasm_path(fixture.project())).unwrap(),
            b"latest wasm"
        );
    }

    #[test]
    fn source_saved_then_restored_during_compilation_still_discards_the_candidate() {
        let mut fixture = Fixture::new();
        fixture.start();
        let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let source = fixture.project().module_sources().join("lib.rs");
        let original = fs::read(&source).unwrap();
        let compiler = fixture.compiler(|| {
            fs::write(&source, "fn temporary() {}\n")?;
            sleep(POLL * 3);
            fs::write(&source, &original)?;
            fixture.artifact(b"temporary wasm")
        });
        assert!(!reload
            .update(
                fixture.project(),
                &compiler,
                &fixture.stack.inspector(),
                &fixture.http
            )
            .unwrap());
        assert_eq!(fixture.publish_count(), 0);
    }

    #[test]
    fn publish_failure_reports_the_reason_and_the_next_build_can_publish() {
        for reason in ["connection reset", "Module definitions differ"] {
            let mut fixture = Fixture::new();
            fixture.start();
            let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
            let compiler = fixture.compiler(|| fixture.artifact(b"old wasm"));
            reload
                .update(
                    fixture.project(),
                    &compiler,
                    &fixture.stack.inspector(),
                    &fixture.http,
                )
                .unwrap();
            let failed = FakeHttp::failing(reason);
            let changed = fixture.compiler(|| fixture.artifact(b"new wasm"));
            let error = reload
                .update(
                    fixture.project(),
                    &changed,
                    &fixture.stack.inspector(),
                    &failed,
                )
                .unwrap_err();
            assert!(error.to_string().contains(reason));
            reload
                .update(
                    fixture.project(),
                    &compiler,
                    &fixture.stack.inspector(),
                    &fixture.http,
                )
                .unwrap();
            assert_eq!(fixture.publish_count(), 2);
        }
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
        let mut metadata = module_metadata(fixture.project());
        metadata["packages"].as_array_mut().unwrap().push(serde_json::json!({"id": "external", "manifest_path": external.path().join("Cargo.toml"), "source": null}));
        metadata["resolve"]["nodes"][0]["deps"] =
            serde_json::json!([{"pkg": "external", "dep_kinds": [{"kind": null}]}]);
        let stack = fixture
            .stack
            .clone()
            .with_stdout("cargo metadata", &metadata.to_string());
        let mut reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        reload
            .refresh_roots(fixture.project(), &stack.runner())
            .unwrap();
        let before = snapshot(&fixture.project().root, &reload.roots).unwrap();
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
        assert_eq!(
            before,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
        fs::write(external.path().join("lib.rs"), "fn changed() {}").unwrap();
        assert_ne!(
            before,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
    }

    #[test]
    fn checkout_under_a_target_named_parent_still_observes_source_edits() {
        let parent = TempDir::new().unwrap();
        let target_parent = parent.path().join("target-x");
        fs::create_dir(&target_parent).unwrap();
        let fixture = Fixture::from_directory(TempDir::new_in(&target_parent).unwrap());
        let reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let before = snapshot(&fixture.project().root, &reload.roots).unwrap();
        let source = fixture.project().module_sources().join("lib.rs");
        assert!(before.contains_key(&source));
        fs::write(source, "fn changed() {}\n").unwrap();
        assert_ne!(
            before,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
    }

    #[test]
    fn initial_snapshot_contains_module_dependencies_and_ignores_unrelated_importer_edits() {
        let fixture = Fixture::new();
        let external = TempDir::new().unwrap();
        fs::write(external.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(external.path().join("lib.rs"), "fn dependency() {}").unwrap();
        let importer = fixture.project().root.join("importer");
        fs::create_dir_all(&importer).unwrap();
        fs::write(importer.join("Cargo.toml"), "[package]").unwrap();
        fs::write(importer.join("lib.rs"), "fn importer() {}").unwrap();
        let mut metadata = module_metadata(fixture.project());
        metadata["packages"].as_array_mut().unwrap().extend([
            serde_json::json!({"id": "external", "manifest_path": external.path().join("Cargo.toml"), "source": null}),
            serde_json::json!({"id": "importer", "manifest_path": importer.join("Cargo.toml"), "source": null}),
        ]);
        metadata["resolve"]["nodes"][0]["deps"] =
            serde_json::json!([{"pkg": "external", "dep_kinds": [{"kind": null}]}]);
        let stack = fixture
            .stack
            .clone()
            .with_stdout("cargo metadata", &metadata.to_string());
        let mut reload = Reload::new(fixture.project(), &stack.runner()).unwrap();
        let observed = snapshot(&fixture.project().root, &reload.roots).unwrap();
        assert!(observed.contains_key(&external.path().join("lib.rs")));
        reload
            .refresh_roots(fixture.project(), &stack.runner())
            .unwrap();
        assert_eq!(
            observed,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
        fs::write(importer.join("lib.rs"), "fn unrelated_change() {}").unwrap();
        assert_eq!(
            observed,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
    }

    #[test]
    fn test_only_dependencies_are_not_watched() {
        let fixture = Fixture::new();
        let mut metadata = module_metadata(fixture.project());
        let mut dependencies = Vec::new();
        let mut ids = Vec::new();
        for (id, kinds) in [
            ("normal", serde_json::json!([{"kind": null}])),
            ("build", serde_json::json!([{"kind": "build"}])),
            (
                "mixed",
                serde_json::json!([{"kind": "dev"}, {"kind": null}]),
            ),
            ("tests", serde_json::json!([{"kind": "dev"}])),
        ] {
            let directory = fixture.project().root.join("crates").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("lib.rs"), "fn dependency() {}").unwrap();
            metadata["packages"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": id, "manifest_path": directory.join("Cargo.toml"), "source": null,
                }));
            dependencies.push(serde_json::json!({"pkg": id, "dep_kinds": kinds}));
            ids.push(id);
        }
        metadata["resolve"]["nodes"][0]["dependencies"] = serde_json::json!(ids);
        metadata["resolve"]["nodes"][0]["deps"] = serde_json::json!(dependencies);
        let stack = fixture
            .stack
            .clone()
            .with_stdout("cargo metadata", &metadata.to_string());
        let reload = Reload::new(fixture.project(), &stack.runner()).unwrap();
        let before = snapshot(&fixture.project().root, &reload.roots).unwrap();
        for id in ["normal", "build", "mixed"] {
            assert!(before.contains_key(
                &fixture
                    .project()
                    .root
                    .join("crates")
                    .join(id)
                    .join("lib.rs")
            ));
        }
        fs::write(
            fixture.project().root.join("crates/tests/lib.rs"),
            "fn test_edit() {}",
        )
        .unwrap();
        assert_eq!(
            before,
            snapshot(&fixture.project().root, &reload.roots).unwrap()
        );
    }

    #[test]
    fn deleting_directories_during_scans_does_not_stop_watching() {
        let fixture = Fixture::new();
        let reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let temporary = fixture.project().module_sources().join("temporary");
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for _ in 0..100 {
                    fs::create_dir_all(&temporary).unwrap();
                    fs::write(temporary.join("generated.rs"), "fn temporary() {}").unwrap();
                    fs::remove_dir_all(&temporary).unwrap();
                }
            });
            while !writer.is_finished() {
                snapshot(&fixture.project().root, &reload.roots).unwrap();
            }
            writer.join().unwrap();
        });
        assert!(!snapshot(&fixture.project().root, &reload.roots)
            .unwrap()
            .contains_key(&temporary.join("generated.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn removing_symlinks_during_scans_does_not_stop_watching() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let link = fixture.project().module_sources().join("temporary.rs");
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for _ in 0..100 {
                    symlink("lib.rs", &link).unwrap();
                    fs::remove_file(&link).unwrap();
                }
            });
            while !writer.is_finished() {
                snapshot(&fixture.project().root, &reload.roots).unwrap();
            }
            writer.join().unwrap();
        });
        assert!(!snapshot(&fixture.project().root, &reload.roots)
            .unwrap()
            .contains_key(&link));
    }

    #[cfg(unix)]
    #[test]
    fn aliases_into_checkout_state_and_build_output_are_ignored() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let root = &fixture.project().root;
        for output in [root.join(".lyracore/cache"), root.join("target/out")] {
            fs::create_dir_all(&output).unwrap();
            fs::write(output.join("generated.rs"), "fn generated() {}").unwrap();
        }
        symlink(
            root.join(".lyracore/cache"),
            root.join("module/src/state-alias"),
        )
        .unwrap();
        symlink(
            root.join("target/out"),
            root.join("module/src/output-alias"),
        )
        .unwrap();
        let reload = Reload::new(fixture.project(), &fixture.stack.runner()).unwrap();
        let before = snapshot(root, &reload.roots).unwrap();
        fs::write(
            root.join(".lyracore/cache/generated.rs"),
            "fn changed_state() {}",
        )
        .unwrap();
        fs::write(
            root.join("target/out/generated.rs"),
            "fn changed_output() {}",
        )
        .unwrap();
        assert_eq!(before, snapshot(root, &reload.roots).unwrap());
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
        let before = snapshot(&fixture.project().root, &roots).unwrap();
        assert!(before.contains_key(&fixture.project().packages_dir().join("linked/src/mod.rs")));
        fs::create_dir_all(external.path().join("target")).unwrap();
        fs::write(external.path().join("target/generated.rs"), "A").unwrap();
        symlink(
            external.path().join("target"),
            external.path().join("output-alias"),
        )
        .unwrap();
        let with_alias = snapshot(&fixture.project().root, &roots).unwrap();
        fs::write(external.path().join("target/generated.rs"), "B").unwrap();
        assert_eq!(
            with_alias,
            snapshot(&fixture.project().root, &roots).unwrap()
        );
        fs::write(external.path().join("src/mod.rs"), "fn changed() {}").unwrap();
        assert_ne!(
            with_alias,
            snapshot(&fixture.project().root, &roots).unwrap()
        );
    }
}
