use lyracore_cli::cmd::{self, Command};
use lyracore_cli::error::{EXIT_FAILURE, EXIT_OK};
use lyracore_cli::proc::{RealProcessInspector, RealProcessRunner};
use lyracore_cli::project::ProjectLayout;
use lyracore_cli::Result;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lyracore: {e}");
            e.exit_code()
        }
    });
}

fn run(args: &[String]) -> Result<i32> {
    let command = Command::parse(args)?;
    let runner = RealProcessRunner;
    let inspector = RealProcessInspector;

    match command {
        Command::Help => {
            println!("{}", cmd::USAGE);
            Ok(EXIT_OK)
        }
        // `doctor` reports a broken layout as a check rather than failing before it can print —
        // diagnosing "am I in the right place?" is exactly what it is for.
        Command::Doctor => {
            let checks = cmd::doctor::run(&ProjectLayout::discover());
            let blocking = cmd::doctor::report(&checks);
            Ok(if blocking { EXIT_FAILURE } else { EXIT_OK })
        }
        Command::DevUp => {
            let mut dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.up(&runner, &inspector).map(|_| EXIT_OK)
        }
        Command::DevStatus => {
            let dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.status(&inspector).map(|_| EXIT_OK)
        }
        Command::DevLogs(component) => {
            let dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.logs(component).map(|_| EXIT_OK)
        }
        Command::DevDown { forget } => {
            let mut dev = cmd::dev::DevManager::new(ProjectLayout::discover()?)?;
            dev.down(&runner, &inspector, forget).map(|_| EXIT_OK)
        }
        Command::AccountCreate { user, source } => {
            let project = ProjectLayout::discover()?;
            cmd::account::create(&project, &user, source, &runner).map(|_| EXIT_OK)
        }
    }
}
