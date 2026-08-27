use lyracore_cli::cmd::{self, Command};
use lyracore_cli::error::{EXIT_FAILURE, EXIT_OK};
use lyracore_cli::http::LoopbackHttpClient;
use lyracore_cli::proc::{RealProcessInspector, RealProcessRunner};
use lyracore_cli::project::ProjectLayout;
use lyracore_cli::Result;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `dev up` is the one command whose failures are usually a missing prerequisite rather than a
    // bug — so on failure only (never on success, and never for any other command) it gets one
    // extra line pointing at `doctor`. Parsed here, ahead of `run`, so a bad-flags usage error
    // (which never reaches `dev up`'s own logic) doesn't earn the hint.
    let is_dev_up = matches!(Command::parse(&args), Ok(Command::DevUp { .. }));
    std::process::exit(match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lyracore: {e}");
            if is_dev_up {
                eprintln!(
                    "lyracore: if this looks like a missing prerequisite, run `lyracore doctor`"
                );
            }
            e.exit_code()
        }
    });
}

fn run(args: &[String]) -> Result<i32> {
    let command = Command::parse(args)?;
    let runner = RealProcessRunner;
    let inspector = RealProcessInspector;
    // Only ever pointed at the loopback node: minting an identity and claiming the operator as it.
    let http = LoopbackHttpClient;

    match command {
        Command::Help => {
            println!("{}", cmd::USAGE);
            Ok(EXIT_OK)
        }
        Command::HelpAll => {
            println!("{}", cmd::USAGE_ALL);
            Ok(EXIT_OK)
        }
        // `doctor` reports a broken layout as a check rather than failing before it can print —
        // diagnosing "am I in the right place?" is exactly what it is for.
        Command::Doctor => {
            let checks = cmd::doctor::run(&ProjectLayout::discover());
            let blocking = cmd::doctor::report(&checks);
            Ok(if blocking { EXIT_FAILURE } else { EXIT_OK })
        }
        Command::Preflight => {
            cmd::preflight::run(&ProjectLayout::discover()?, &runner).map(|_| EXIT_OK)
        }
        Command::Publish {
            databases,
            skip_preflight,
        } => cmd::publish::run(
            &ProjectLayout::discover()?,
            &runner,
            &databases,
            skip_preflight,
        )
        .map(|_| EXIT_OK),
        Command::DevUp { bind, topology } => {
            let mut dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.up(&runner, &inspector, &http, bind, topology)
                .map(|_| EXIT_OK)
        }
        Command::DevStatus => {
            let dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.status(&runner, &inspector).map(|_| EXIT_OK)
        }
        Command::DevSmoke => {
            let dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.smoke(&runner, &inspector).map(|_| EXIT_OK)
        }
        Command::DevLogs(component) => {
            let dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.logs(component).map(|_| EXIT_OK)
        }
        Command::DevDown { forget } => {
            let mut dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.down(&runner, &inspector, forget).map(|_| EXIT_OK)
        }
        Command::ImportWorld(options) => cmd::import::run_world(
            &ProjectLayout::discover()?,
            &runner,
            &cmd::import::TtyPrompt::import_world(),
            &options,
        )
        .map(|_| EXIT_OK),
        Command::ImportVmaps { options } => cmd::import::run_vmaps(
            &ProjectLayout::discover()?,
            &runner,
            &cmd::import::TtyPrompt::import_vmaps(),
            &options,
        )
        .map(|_| EXIT_OK),
        Command::AccountCreate { user, source } => {
            let project = ProjectLayout::discover()?;
            cmd::account::create(&project, &user, source, &runner).map(|_| EXIT_OK)
        }
        Command::ConfigShow => cmd::config::show(&ProjectLayout::discover()?).map(|_| EXIT_OK),
        Command::ConfigSetClientData { path } => {
            cmd::config::set_client_data(&ProjectLayout::discover()?, &path).map(|_| EXIT_OK)
        }
        Command::ClientSync => {
            cmd::client::sync(&ProjectLayout::discover()?, &runner).map(|_| EXIT_OK)
        }
        Command::PackagesAdd { source, yes } => cmd::packages::add(
            &ProjectLayout::discover()?,
            &runner,
            &cmd::import::TtyPrompt::packages_add(),
            &source,
            yes,
        )
        .map(|_| EXIT_OK),
        Command::PackagesUpdate { name, yes } => cmd::packages::git::update(
            &ProjectLayout::discover()?,
            &runner,
            &cmd::import::TtyPrompt::packages_update(),
            name.as_deref(),
            yes,
        )
        .map(|_| EXIT_OK),
        Command::PackagesList => cmd::packages::list(&ProjectLayout::discover()?).map(|_| EXIT_OK),
        Command::PackagesNew { name } => {
            cmd::packages::new(&ProjectLayout::discover()?, &runner, &name).map(|_| EXIT_OK)
        }
        Command::PackagesBuild => {
            cmd::packages::build::run(&ProjectLayout::discover()?, &runner).map(|_| EXIT_OK)
        }
        // The lifecycle verbs take no process runner: they move or delete a directory and print
        // what is left to run, so there is nothing for them to execute.
        Command::PackagesEnable { name } => {
            cmd::packages::lifecycle::enable(&ProjectLayout::discover()?, &name).map(|_| EXIT_OK)
        }
        Command::PackagesDisable { name } => {
            cmd::packages::lifecycle::disable(&ProjectLayout::discover()?, &name).map(|_| EXIT_OK)
        }
        Command::PackagesReplay(options) => cmd::packages::replay::run(
            &ProjectLayout::discover()?,
            &runner,
            &cmd::import::TtyPrompt::packages_replay(),
            &options,
        )
        .map(|_| EXIT_OK),
        Command::PackagesRemove { name, yes } => cmd::packages::lifecycle::remove(
            &ProjectLayout::discover()?,
            &cmd::import::TtyPrompt::packages_remove(),
            &name,
            yes,
        )
        .map(|_| EXIT_OK),
        Command::CharacterGm { name, enabled } => {
            let project = ProjectLayout::discover()?;
            cmd::character::gm(&project, &runner, &http, &name, enabled).map(|_| EXIT_OK)
        }
        Command::ProductionStatus(options) => {
            let status = cmd::production::inspect(&options, &runner);
            cmd::production::report(&status);
            Ok(if status.blocking() {
                EXIT_FAILURE
            } else {
                EXIT_OK
            })
        }
        Command::ServiceReconcile => {
            cmd::service::reconcile(&ProjectLayout::discover()?, &runner).map(|_| EXIT_OK)
        }
        Command::Update => cmd::update::run(&ProjectLayout::discover()?, &runner).map(|_| EXIT_OK),
    }
}
