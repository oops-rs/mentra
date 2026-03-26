use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Authorization policy for builtin shell, background, and file tools.
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    allow_shell_commands: bool,
    allow_background_commands: bool,
    allowed_working_roots: Vec<PathBuf>,
    allowed_read_roots: Vec<PathBuf>,
    allowed_write_roots: Vec<PathBuf>,
    allowed_env_vars: Vec<String>,
    pub(crate) background_task_limit: Option<usize>,
    pub(crate) default_command_timeout: Duration,
    pub(crate) max_command_timeout: Duration,
    pub(crate) max_output_bytes_per_stream: usize,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            allow_shell_commands: false,
            allow_background_commands: false,
            allowed_working_roots: Vec::new(),
            allowed_read_roots: Vec::new(),
            allowed_write_roots: Vec::new(),
            allowed_env_vars: default_allowed_env_vars(),
            background_task_limit: Some(8),
            default_command_timeout: Duration::from_secs(30),
            max_command_timeout: Duration::from_secs(30),
            max_output_bytes_per_stream: 64 * 1024,
        }
    }
}

fn default_allowed_env_vars() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut vars = vec!["PATH".to_string()];
        vars.extend([
            "PATHEXT".to_string(),
            "SystemRoot".to_string(),
            "COMSPEC".to_string(),
            "TEMP".to_string(),
            "TMP".to_string(),
        ]);
        vars
    }

    #[cfg(not(windows))]
    {
        vec!["PATH".to_string()]
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

    /// Adds an extra root allowed for builtin file writes.
    pub fn with_allowed_write_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.allowed_write_roots.push(path.into());
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

    /// Sets the default builtin command timeout.
    pub fn with_default_command_timeout(mut self, timeout: Duration) -> Self {
        self.default_command_timeout = timeout;
        self
    }

    /// Sets the hard timeout cap for builtin commands.
    pub fn with_max_command_timeout(mut self, timeout: Duration) -> Self {
        self.max_command_timeout = timeout;
        self
    }

    /// Sets the maximum captured bytes for each output stream.
    pub fn with_max_output_bytes_per_stream(mut self, max_bytes: usize) -> Self {
        self.max_output_bytes_per_stream = max_bytes;
        self
    }

    /// Backward-compatible shortcut that sets both default and max timeout.
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.default_command_timeout = timeout;
        self.max_command_timeout = timeout;
        self
    }

    pub(crate) fn authorize_command_execution(
        &self,
        base_dir: &Path,
        cwd: &Path,
        background: bool,
    ) -> Result<(), String> {
        self.authorize_command_roots(base_dir, cwd, background)
    }

    pub(crate) fn effective_timeout(&self, requested: Option<Duration>) -> Duration {
        requested
            .unwrap_or(self.default_command_timeout)
            .min(self.max_command_timeout)
    }

    pub(crate) fn allowed_environment(&self) -> Vec<(String, String)> {
        self.allowed_env_vars
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect()
    }

    pub(crate) fn authorize_file_read(
        &self,
        base_dir: &Path,
        path: &Path,
    ) -> Result<PathBuf, String> {
        let resolved = resolve_authorized_path(base_dir, path)?;

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

    pub(crate) fn authorize_file_write(
        &self,
        base_dir: &Path,
        path: &Path,
    ) -> Result<PathBuf, String> {
        let resolved = resolve_authorized_path(base_dir, path)?;

        if path_is_allowed(
            resolved.as_path(),
            base_dir,
            self.allowed_write_roots.as_slice(),
        ) {
            Ok(resolved)
        } else {
            Err(format!(
                "Path '{}' is outside the runtime policy write roots",
                resolved.display()
            ))
        }
    }

    fn authorize_command_roots(
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
}

fn path_is_allowed(path: &Path, default_root: &Path, extra_roots: &[PathBuf]) -> bool {
    let candidate_path = canonicalize_policy_root(path);
    let default_root = canonicalize_policy_root(default_root);
    candidate_path.starts_with(&default_root)
        || extra_roots
            .iter()
            .map(|root| canonicalize_policy_root(root))
            .any(|root| candidate_path.starts_with(root))
}

fn canonicalize_policy_root(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_authorized_path(base_dir: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let normalized = normalize_absolute_path(&resolved)?;
    resolve_existing_components(&normalized)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.is_absolute() {
                    return Err(format!(
                        "Path '{}' escapes the filesystem root",
                        path.display()
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    if !normalized.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    Ok(normalized)
}

fn resolve_existing_components(path: &Path) -> Result<PathBuf, String> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => unreachable!("paths are normalized before resolution"),
            Component::Normal(segment) => {
                resolved.push(segment);
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        resolved = fs::canonicalize(&resolved).map_err(|error| {
                            format!(
                                "Failed to resolve symlink '{}': {error}",
                                resolved.display()
                            )
                        })?;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Failed to inspect '{}': {error}",
                            resolved.display()
                        ));
                    }
                }
            }
        }
    }

    if !resolved.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join("mentra-runtime-policy-tests")
            .join(label)
    }

    #[test]
    fn shell_roots_and_background_switches_short_circuit() {
        let cwd = test_path("repo");
        let policy = RuntimePolicy::default()
            .allow_shell_commands(true)
            .allow_background_commands(false);
        let error = policy
            .authorize_command_execution(&cwd, &cwd, true)
            .expect_err("background should be disabled");
        assert!(error.contains("Background command execution is disabled"));
    }

    #[test]
    fn authorize_command_execution_rejects_working_directory_outside_roots() {
        let base_dir = test_path("repo");
        let cwd = test_path("other");
        let policy = RuntimePolicy::default().allow_shell_commands(true);

        let error = policy
            .authorize_command_execution(&base_dir, &cwd, false)
            .expect_err("working directory should be rejected");
        assert!(error.contains("outside the runtime policy roots"));
    }

    #[test]
    fn normalize_absolute_path_rejects_parent_past_root() {
        let mut path = std::env::temp_dir();
        for _ in 0..10 {
            path.push("..");
        }
        path.push("escape");
        let error = normalize_absolute_path(&path).expect_err("path should be rejected");
        assert!(error.contains("escapes the filesystem root"));
    }

    #[cfg(unix)]
    #[test]
    fn authorize_file_write_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = unique_temp_dir("policy-write-root");
        let outside = unique_temp_dir("policy-write-outside");
        let link = root.join("link");
        symlink(&outside, &link).expect("create symlink");

        let policy = RuntimePolicy::default().with_allowed_write_root(&root);
        let error = policy
            .authorize_file_write(&root, &link.join("escape.txt"))
            .expect_err("symlink escape should be denied");
        assert!(error.contains("outside the runtime policy write roots"));

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("duration")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mentra-{label}-{unique}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
