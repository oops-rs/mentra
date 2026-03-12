use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Authorization policy for builtin shell, background, and file tools.
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    allow_shell_commands: bool,
    allow_background_commands: bool,
    allowed_working_roots: Vec<PathBuf>,
    allowed_read_roots: Vec<PathBuf>,
    allowed_env_vars: Vec<String>,
    pub(crate) background_task_limit: Option<usize>,
    pub(crate) command_timeout: Option<Duration>,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            allow_shell_commands: false,
            allow_background_commands: false,
            allowed_working_roots: Vec::new(),
            allowed_read_roots: Vec::new(),
            allowed_env_vars: Vec::new(),
            background_task_limit: Some(8),
            command_timeout: Some(Duration::from_secs(30)),
        }
    }
}

impl RuntimePolicy {
    /// Returns a permissive policy that enables shell and background execution.
    pub fn permissive() -> Self {
        Self {
            allow_shell_commands: true,
            allow_background_commands: true,
            ..Self::default()
        }
    }

    /// Enables or disables foreground shell command execution.
    pub fn allow_shell_commands(mut self, allow: bool) -> Self {
        self.allow_shell_commands = allow;
        self
    }

    /// Enables or disables background shell command execution.
    pub fn allow_background_commands(mut self, allow: bool) -> Self {
        self.allow_background_commands = allow;
        self
    }

    /// Adds an extra working-directory root allowed for shell commands.
    pub fn with_allowed_working_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowed_working_roots.push(path.into());
        self
    }

    /// Adds an extra root allowed for builtin file reads.
    pub fn with_allowed_read_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowed_read_roots.push(path.into());
        self
    }

    /// Records an environment variable name that callers may expose to tools.
    pub fn with_allowed_env_var(mut self, name: impl Into<String>) -> Self {
        self.allowed_env_vars.push(name.into());
        self
    }

    /// Sets the maximum number of concurrently tracked background tasks per agent.
    pub fn with_max_background_tasks(mut self, limit: usize) -> Self {
        self.background_task_limit = Some(limit);
        self
    }

    /// Sets the builtin command timeout.
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = Some(timeout);
        self
    }

    pub(crate) fn authorize_command(
        &self,
        base_dir: &Path,
        cwd: &Path,
        background: bool,
    ) -> Result<(), String> {
        if !self.allow_shell_commands {
            return Err(
                "Shell command execution is disabled by the runtime policy. Use RuntimeBuilder::with_policy(...) to opt in."
                    .to_string(),
            );
        }
        if background && !self.allow_background_commands {
            return Err(
                "Background command execution is disabled by the runtime policy.".to_string(),
            );
        }

        if !path_is_allowed(cwd, base_dir, self.allowed_working_roots.as_slice()) {
            return Err(format!(
                "Working directory '{}' is outside the runtime policy roots",
                cwd.display()
            ));
        }

        Ok(())
    }

    pub(crate) fn authorize_file_read(
        &self,
        base_dir: &Path,
        path: &Path,
    ) -> Result<PathBuf, String> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };

        if path_is_allowed(
            resolved.as_path(),
            base_dir,
            self.allowed_read_roots.as_slice(),
        ) {
            Ok(resolved)
        } else {
            Err(format!(
                "Path '{}' is outside the runtime policy read roots",
                resolved.display()
            ))
        }
    }
}

fn path_is_allowed(path: &Path, default_root: &Path, extra_roots: &[PathBuf]) -> bool {
    path.starts_with(default_root) || extra_roots.iter().any(|root| path.starts_with(root))
}
