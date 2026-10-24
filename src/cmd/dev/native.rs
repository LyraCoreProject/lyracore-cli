use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeMode {
    Normal,
    HotReload,
}

impl DevManager {
    /// Returns a credential when watch must initialize the Realm after its first publish.
    pub(super) fn up_native(
        &mut self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        http: &dyn HttpClient,
        bind: ClientBind,
        mode: NativeMode,
    ) -> Result<Option<Credential>> {
        self.project.ensure_dirs()?;
        for component in Component::ALL {
            if let Some(record) = self.state.record(component) {
                if inspector.identity(record.pid).is_none() {
                    self.state.set(component, None);
                    continue;
                }
                if component == Component::Gateway || !self.state.native_runtime {
                    return Err(Error::Process(
                        "a legacy local runtime is recorded; run `lyracore dev down` before starting the Native Host".into(),
                    ));
                }
            }
        }
        let current = self.status_for(Component::Spacetime, inspector);
        if matches!(
            current,
            ComponentStatus::External | ComponentStatus::Unhealthy(_)
        ) {
            return Err(Error::Process(
                "the Native Host needs its own local process; inspect `lyracore dev status` and stop the conflicting process before retrying".into(),
            ));
        }
        if matches!(
            current,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) && mode == NativeMode::HotReload
            && !self.state.native_hot_reload
        {
            return Err(Error::Process(
                "the running Native Host uses a different development mode; run `lyracore dev down` first, then `lyracore dev watch` to enable hot reload".into(),
            ));
        }
        if matches!(
            current,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) && self.bind != bind
        {
            return Err(Error::Process(
                "the running Native Host cannot change its listener addresses; run `lyracore dev down` first".into(),
            ));
        }
        if current == ComponentStatus::Healthy
            && inspector.world_ready(&self.bind.host(), ProjectLayout::WORLD_PORT)
            && inspector.serving(&self.bind.host(), ProjectLayout::LOGON_PORT)
        {
            println!("Native Realm is already running.");
            self.print_native_status(runner, inspector);
            return Ok(None);
        }
        self.bind = bind;
        self.topology = Topology::Single;
        self.state.client_host = self.bind.host();
        self.state.database = ProjectLayout::DATABASE.into();
        self.state.topology = Topology::Single.as_str().into();
        self.state.native_runtime = true;
        if !matches!(
            current,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) {
            self.state.native_hot_reload = mode == NativeMode::HotReload;
        }
        self.state.gateway = None;

        if !matches!(
            current,
            ComponentStatus::Healthy | ComponentStatus::Starting
        ) {
            for port in [ProjectLayout::LOGON_PORT, ProjectLayout::WORLD_PORT] {
                if inspector.serving(&self.bind.host(), port) {
                    return Err(Error::Process(format!(
                        "{}:{port} is already in use; the Native Host will not adopt another process",
                        self.bind.host()
                    )));
                }
            }
            let checkout = self.project.state_dir.join("native-host/SpacetimeDB");
            println!("Building the Native Host...");
            runner.run_streaming(
                &CommandSpec::new("python3")
                    .arg(
                        self.project
                            .root
                            .join("native-host/prepare-host.py")
                            .to_string_lossy(),
                    )
                    .arg("--checkout")
                    .arg(checkout.to_string_lossy())
                    .arg("--build")
                    .cwd(&self.project.root),
            )?;
            let mut command = CommandSpec::new(
                checkout
                    .join("target/release/spacetimedb-standalone")
                    .to_string_lossy(),
            )
            .own_process_group()
            .arg("start")
            .arg("--listen-addr")
            .arg(ProjectLayout::stdb_listen())
            .arg("--data-dir")
            .arg(self.project.state_dir.join("native-data").to_string_lossy())
            .arg("--non-interactive")
            .env("LYRACORE_NATIVE_DATABASE", ProjectLayout::DATABASE)
            .env(
                "LYRACORE_NATIVE_WORLD_ADDR",
                ProjectLayout::world_bind(&self.bind),
            )
            .env(
                "LYRACORE_NATIVE_LOGON_ADDR",
                ProjectLayout::logon_bind(&self.bind),
            )
            .cwd(&self.project.root);
            if mode == NativeMode::HotReload {
                command = command.env("LYRACORE_NATIVE_DEV", "1");
            } else {
                command = command.env_remove("LYRACORE_NATIVE_DEV");
            }
            let record = self.spawn_recorded(Component::Spacetime, &command, runner, inspector)?;
            self.state.spacetime = Some(record);
            self.state.save(&self.project.state_file())?;
        }
        self.wait_for_port(Component::Spacetime, inspector)?;
        self.state.save(&self.project.state_file())?;
        let credential = crate::token::resolve_or_mint(
            runner,
            http,
            &self.project.token_file(),
            &ProjectLayout::stdb_uri(),
        )?;
        // Native Core connections and operator calls must use the Module publisher's identity.
        let credential = crate::token::retain_native_owner(&self.project, credential)?;
        if mode == NativeMode::HotReload {
            return Ok(Some(credential));
        }
        self.publish(runner)?;
        self.initialize_native(runner, inspector, http, &credential)?;
        Ok(None)
    }

    pub(super) fn initialize_native(
        &self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
        http: &dyn HttpClient,
        credential: &Credential,
    ) -> Result<()> {
        self.claim_operator(runner, http, credential)?;
        for (reducer, arguments) in [
            ("debug_repair_after_publish", vec![]),
            ("install_guid_range", vec![serde_json::json!(0)]),
        ] {
            call_as_operator(
                &self.project,
                runner,
                http,
                credential,
                ProjectLayout::DATABASE,
                reducer,
                &arguments,
            )?;
        }
        self.provision_realm(runner, http, credential)?;
        if !inspector.world_ready(&self.bind.host(), ProjectLayout::WORLD_PORT) {
            return Err(Error::Process(format!(
                "the Native Host did not return a world authentication challenge; check the Module's Operator identity and {}",
                self.project.log_file(Component::Spacetime).display()
            )));
        }
        println!(
            "Native Realm is ready on {}. State: {}",
            self.bind.host(),
            self.project.state_dir.join("native-data").display()
        );
        println!("Before play, run `lyracore import`, then `lyracore dev check`.");
        Ok(())
    }

    pub(super) fn print_native_status(
        &self,
        runner: &dyn ProcessRunner,
        inspector: &dyn ProcessInspector,
    ) {
        let host = self.status_for(Component::Spacetime, inspector);
        println!("Native Host: {host:?}");
        if self.state.native_hot_reload {
            println!("Development hot reload enabled.");
        }
        for (name, port) in [
            ("logon", ProjectLayout::LOGON_PORT),
            ("world", ProjectLayout::WORLD_PORT),
        ] {
            println!(
                "{name} {}:{port}: {}",
                self.bind.host(),
                if inspector.serving(&self.bind.host(), port) {
                    "listening"
                } else {
                    "stopped"
                }
            );
        }
        println!(
            "Shard {}: {}",
            ProjectLayout::DATABASE,
            self.database_health(runner, &host, ProjectLayout::DATABASE, None)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeHttp;
    use crate::proc::fake::{Call, FakeStack, FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
    use tempfile::TempDir;

    fn project(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!("spacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"),
        )
        .unwrap();
        std::fs::write(root.join("module/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join("native-host")).unwrap();
        std::fs::write(root.join("native-host/prepare-host.py"), "").unwrap();
        ProjectLayout::from_root(root).unwrap()
    }

    #[test]
    fn native_up_runs_one_host_and_publishes_one_shard() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &FakeHttp::new(),
            ClientBind::Loopback,
            Topology::Sharded,
        )
        .unwrap();
        let calls = stack.calls();
        let spawned: Vec<_> = calls
            .iter()
            .filter_map(|call| match call {
                Call::Spawn { spec, .. } => Some(spec),
                _ => None,
            })
            .collect();
        assert_eq!(spawned.len(), 1);
        assert!(spawned[0].program().ends_with("/spacetimedb-standalone"));
        assert_eq!(spawned[0].env_value("LYRACORE_NATIVE_DEV"), None);
        assert!(spawned[0].removes_env("LYRACORE_NATIVE_DEV"));
        assert_eq!(
            spawned[0].env_value("LYRACORE_NATIVE_DATABASE"),
            Some("lyracore")
        );
        assert_eq!(
            spawned[0].env_value("LYRACORE_NATIVE_LOGON_ADDR"),
            Some("127.0.0.1:3724")
        );
        assert_eq!(
            spawned[0].env_value("LYRACORE_NATIVE_WORLD_ADDR"),
            Some("127.0.0.1:8085")
        );
        assert!(!stack
            .rendered()
            .iter()
            .any(|cmd| cmd.contains("lyracore-gateway")));
        let published: Vec<_> = stack
            .rendered()
            .into_iter()
            .filter(|cmd| cmd.contains(" publish "))
            .collect();
        assert_eq!(published.len(), 1);
        assert!(!published[0].contains("kalimdor") && !published[0].contains("realm"));
        assert!(dev.state.gateway.is_none());
        assert!(dev.state.native_runtime);
        assert_eq!(dev.state.topology(), Topology::Single);
        let pid = dev.state.spacetime.as_ref().unwrap().pid;
        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        assert_eq!(stack.terminated(), [pid]);
        for port in [3000, 3724, 8085] {
            assert!(!stack.inspector().serving("127.0.0.1", port));
        }
    }

    #[test]
    fn native_start_refuses_an_external_node_before_building_or_publishing() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().with_port(3000);
        assert!(dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                Topology::Single
            )
            .is_err());
        assert!(stack.calls().is_empty());
    }

    #[test]
    fn a_fresh_native_realm_publishes_and_provisions_as_the_same_local_identity() {
        use crate::http::fake::MINTED_TOKEN;
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().fail_on("login show", "no login");
        let http = FakeHttp::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();
        assert_eq!(http.mints().len(), 1);
        let requests = http.requests();
        assert!(requests
            .iter()
            .filter(|request| request.url.contains("/call/"))
            .all(|request| request.bearer.as_deref() == Some(MINTED_TOKEN)));
        let calls = stack.calls();
        let publish = calls
            .iter()
            .find_map(|call| match call {
                Call::Stream(spec) if spec.args().iter().any(|arg| arg == "publish") => Some(spec),
                _ => None,
            })
            .unwrap();
        assert_eq!(publish.args()[0], "--config-path");
        assert_eq!(
            std::fs::read_to_string(&publish.args()[1]).unwrap(),
            format!("spacetimedb_token = \"{MINTED_TOKEN}\"\n")
        );
        assert!(publish
            .args()
            .iter()
            .any(|arg| arg == "http://127.0.0.1:3000"));
        assert!(stack
            .rendered()
            .iter()
            .all(|command| !command.contains(MINTED_TOKEN)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [dev.project.token_file(), dev.project.native_cli_config()] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn failed_publish_keeps_the_host_record_so_down_can_stop_it() {
        let tmp = TempDir::new().unwrap();
        let project = project(&tmp);
        let state_file = project.state_file();
        let mut dev = DevManager::new(project).unwrap();
        let stack = FakeStack::new().fail_on(" publish ", "publish failed");
        assert!(dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                Topology::Single
            )
            .is_err());
        let state = RuntimeState::load(&state_file).unwrap();
        assert!(state.spacetime.is_some());
        dev.down(&stack.runner(), &stack.inspector(), false)
            .unwrap();
        assert_eq!(stack.terminated().len(), 1);
    }

    #[test]
    fn an_open_world_port_without_a_challenge_does_not_pass_startup() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new().without_world_challenge();
        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &FakeHttp::new(),
                ClientBind::Loopback,
                Topology::Single,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("authentication challenge"),
            "{error}"
        );
    }

    #[test]
    fn an_unchanged_native_host_is_reused_but_cannot_change_its_bind() {
        let tmp = TempDir::new().unwrap();
        let mut dev = DevManager::new(project(&tmp)).unwrap();
        let stack = FakeStack::new();
        let http = FakeHttp::new();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();
        let first = stack.calls().len();
        dev.up(
            &stack.runner(),
            &stack.inspector(),
            &http,
            ClientBind::Loopback,
            Topology::Single,
        )
        .unwrap();
        assert!(!stack.calls()[first..]
            .iter()
            .any(|call| matches!(call, Call::Spawn { .. } | Call::Stream(_))));
        let error = dev
            .up(
                &stack.runner(),
                &stack.inspector(),
                &http,
                ClientBind::parse_lan("192.168.1.20").unwrap(),
                Topology::Single,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cannot change"));
    }
}
