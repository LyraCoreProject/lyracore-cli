use std::collections::BTreeMap;
use std::process::Command;

/// A command the CLI intends to run.
///
/// There is deliberately no way to put a secret in here. A password reaches a child only through
/// [`ProcessRunner::run_with_secret_stdin`](super::ProcessRunner::run_with_secret_stdin), which
/// takes it as a separate argument and writes it to the child's stdin — so "the rendered command
/// contains no secret" is a property of the type, not of remembering to call a masking helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    /// Variables explicitly unset for the child. The production gateway recipe is exported in
    /// plenty of contributor shells; inheriting it would silently turn the single-database
    /// fixture into a five-database gateway pointed at databases this CLI never published.
    env_remove: Vec<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_remove: Vec::new(),
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
        cmd.args(&self.args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
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
        let cmd = CommandSpec::new("spacetime").arg("call").arg("claim_operator");
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
