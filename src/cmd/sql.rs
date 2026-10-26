//! `lyracore sql QUERY` — run SQL on the local Realm's database as its publisher.
use crate::cmd::import::sql_command;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};

pub fn run(project: &ProjectLayout, runner: &dyn ProcessRunner, query: &str) -> Result<()> {
    if project.has_native_host() && !project.native_cli_config().is_file() {
        return Err(Error::Usage(format!(
            "no publisher configuration at {} — run `lyracore dev up` first",
            project.native_cli_config().display()
        )));
    }
    let output = runner
        .run_and_wait(&sql_command(project, ProjectLayout::DATABASE, query))
        .map_err(|error| {
            Error::Process(format!(
                "{error}\n  — is the Realm up? check `lyracore dev status`"
            ))
        })?;
    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::FakeStack;
    use tempfile::TempDir;

    fn native_project(tmp: &TempDir) -> ProjectLayout {
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("native-host")).unwrap();
        std::fs::write(tmp.path().join("native-host/prepare-host.py"), "").unwrap();
        ProjectLayout::from_root(tmp.path()).unwrap()
    }

    fn with_publisher_config(project: &ProjectLayout) {
        std::fs::create_dir_all(project.native_cli_config().parent().unwrap()).unwrap();
        std::fs::write(project.native_cli_config(), "spacetimedb_token = \"t\"").unwrap();
    }

    #[test]
    fn the_query_runs_on_the_local_database_as_the_publisher() {
        let tmp = TempDir::new().unwrap();
        let project = native_project(&tmp);
        with_publisher_config(&project);
        let stack = FakeStack::new();

        run(&project, &stack.runner(), "SELECT * FROM account").unwrap();

        assert_eq!(
            stack.rendered(),
            vec![format!(
                "spacetime --config-path {} sql --server http://127.0.0.1:3000 lyracore \
                 SELECT * FROM account",
                project.native_cli_config().display()
            )]
        );
    }

    #[test]
    fn a_missing_publisher_configuration_points_at_dev_up_without_running_spacetime() {
        let tmp = TempDir::new().unwrap();
        let project = native_project(&tmp);
        let stack = FakeStack::new();

        let error = run(&project, &stack.runner(), "SELECT 1").unwrap_err();

        assert!(error.to_string().contains("run `lyracore dev up` first"));
        assert!(stack.rendered().is_empty());
    }

    #[test]
    fn a_failed_query_keeps_spacetimes_message_and_points_at_dev_status() {
        let tmp = TempDir::new().unwrap();
        let project = native_project(&tmp);
        with_publisher_config(&project);
        let stack = FakeStack::new().fail_on("spacetime", "connection refused");

        let message = run(&project, &stack.runner(), "SELECT 1")
            .unwrap_err()
            .to_string();

        assert!(message.contains("connection refused"));
        assert!(message.contains("lyracore dev status"));
    }
}
