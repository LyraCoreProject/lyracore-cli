use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Keep secrets out of arguments; use `ProcessRunner::run_with_secret_stdin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    env_remove: Vec<String>,
    /// None inherits the caller's current directory.
    cwd: Option<PathBuf>,
    own_process_group: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            cwd: None,
            own_process_group: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Isolate Unix terminal signals from a background child.
    pub fn own_process_group(mut self) -> Self {
        self.own_process_group = true;
        self
    }

    pub fn cwd_value(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    pub fn removes_env(&self, key: &str) -> bool {
        self.env_remove.iter().any(|k| k == key)
    }

    pub fn build(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        #[cfg(unix)]
        if self.own_process_group {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.args(&self.args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd
    }

    /// Human-readable form, used in diagnostics and logs.
    pub fn render(&self) -> String {
        let mut rendered = self.program.clone();
        for arg in &self.args {
            rendered.push(' ');
            rendered.push_str(arg);
        }
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_round_trips_program_and_args() {
        let cmd = CommandSpec::new("spacetime")
            .arg("call")
            .arg("claim_operator");
        assert_eq!(cmd.render(), "spacetime call claim_operator");
    }

    #[test]
    fn a_removed_variable_is_not_also_set() {
        // Guards the fixture invariant: whatever the contributor exported, the child does not
        // see LYRACORE_SHARD_MAP.
        let cmd = CommandSpec::new("gateway")
            .env("LYRACORE_DATABASE", "lyracore")
            .env_remove("LYRACORE_SHARD_MAP");
        assert!(!cmd.render().contains("LYRACORE_SHARD_MAP"));
        assert_eq!(cmd.env_value("LYRACORE_SHARD_MAP"), None);
        assert!(cmd.removes_env("LYRACORE_SHARD_MAP"));
    }
}
