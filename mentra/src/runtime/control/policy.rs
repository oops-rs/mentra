use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use crate::tool::{
    ToolAuthorizationOutcome,
    bash_validation::{CommandIntent, ValidationResult, classify_command, validate_command},
};

/// Controls heuristic validation of builtin shell commands.
///
/// Shell validation is a defense-in-depth guardrail and permission-prompt UX
/// signal. It is heuristic and is not a security boundary; filesystem roots,
/// environment isolation, process groups, and command timeouts remain the
/// enforceable runtime boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellValidationMode {
    /// Classify commands for authorization previews without changing execution.
    #[default]
    Off,
    /// Emit an authorization hook for warnings or blocks, but allow execution.
    Warn,
    /// Deny commands classified as blocked and surface warnings through hooks.
    Enforce,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellValidation {
    pub(crate) mode: ShellValidationMode,
    pub(crate) intent: CommandIntent,
    pub(crate) result: ValidationResult,
    pub(crate) outcome: ToolAuthorizationOutcome,
}

impl ShellValidationMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Enforce => "enforce",
        }
    }
}

impl ShellValidation {
    pub(crate) const fn intent_name(&self) -> &'static str {
        match self.intent {
            CommandIntent::ReadOnly => "read_only",
            CommandIntent::Write => "write",
            CommandIntent::Destructive => "destructive",
            CommandIntent::Network => "network",
            CommandIntent::ProcessManagement => "process_management",
            CommandIntent::PackageManagement => "package_management",
            CommandIntent::SystemAdmin => "system_admin",
            CommandIntent::Unknown => "unknown",
        }
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        self.result.reason()
    }

    pub(crate) fn should_emit_hook(&self) -> bool {
        self.mode != ShellValidationMode::Off && self.outcome != ToolAuthorizationOutcome::Allow
    }

    pub(crate) fn should_deny(&self) -> bool {
        self.mode == ShellValidationMode::Enforce && self.outcome == ToolAuthorizationOutcome::Deny
    }
}

/// Authorization policy for builtin shell, background, and file tools.
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    allow_shell_commands: bool,
    allow_background_commands: bool,
    allowed_working_roots: Vec<PathBuf>,
    allowed_read_roots: Vec<PathBuf>,
    allowed_write_roots: Vec<PathBuf>,
    allowed_env_vars: Vec<String>,
    shell_validation_mode: ShellValidationMode,
    pub(crate) background_task_limit: Option<usize>,
    pub(crate) default_command_timeout: Duration,
    pub(crate) max_command_timeout: Duration,
    pub(crate) max_output_bytes_per_stream: usize,
    pub(crate) max_tool_result_bytes: usize,
    pub(crate) max_tool_result_lines: usize,
    pub(crate) spill_full_tool_output: bool,
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
            shell_validation_mode: ShellValidationMode::Off,
            background_task_limit: Some(8),
            default_command_timeout: Duration::from_secs(30),
            max_command_timeout: Duration::from_secs(30),
            max_output_bytes_per_stream: 64 * 1024,
            max_tool_result_bytes: 50 * 1024,
            max_tool_result_lines: 2_000,
            spill_full_tool_output: true,
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

    /// Returns a workspace-bounded policy that restricts file access and
    /// shell execution to the given workspace root.
    ///
    /// This is the recommended policy for production use — it allows shell
    /// commands and file operations only within the workspace boundary.
    pub fn workspace_bounded(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            allow_shell_commands: true,
            allow_background_commands: true,
            allowed_working_roots: vec![workspace.clone()],
            allowed_read_roots: vec![workspace.clone()],
            allowed_write_roots: vec![workspace],
            default_command_timeout: Duration::from_secs(120),
            max_command_timeout: Duration::from_secs(600),
            ..Self::default()
        }
    }

    /// Returns a read-only policy that allows file reads and shell commands
    /// but blocks all file writes.
    pub fn read_only(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            allow_shell_commands: true,
            allow_background_commands: false,
            allowed_working_roots: vec![workspace.clone()],
            allowed_read_roots: vec![workspace],
            allowed_write_roots: Vec::new(),
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

    /// Selects heuristic validation for builtin shell commands.
    ///
    /// This is a defense-in-depth guardrail and prompt signal, not a security
    /// boundary. [`ShellValidationMode::Off`] preserves execution behavior.
    pub fn shell_validation(mut self, mode: ShellValidationMode) -> Self {
        self.shell_validation_mode = mode;
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

    /// Sets the provider-visible byte limit for each completed tool result.
    ///
    /// The limit applies independently to successful and error results. An
    /// actionable truncation notice is appended outside the retained head.
    pub fn with_max_tool_result_bytes(mut self, max_bytes: usize) -> Self {
        self.max_tool_result_bytes = max_bytes;
        self
    }

    /// Sets the provider-visible line limit for each completed tool result.
    pub fn with_max_tool_result_lines(mut self, max_lines: usize) -> Self {
        self.max_tool_result_lines = max_lines;
        self
    }

    /// Enables or disables spilling a truncated tool result to the agent's
    /// transcript artifact directory.
    pub fn spill_full_tool_output(mut self, spill: bool) -> Self {
        self.spill_full_tool_output = spill;
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

    pub(crate) fn evaluate_shell_command(
        &self,
        command: &str,
        default_workspace: &Path,
    ) -> ShellValidation {
        let workspace = self
            .allowed_working_roots
            .first()
            .map(PathBuf::as_path)
            .unwrap_or(default_workspace);
        let result = validate_command(command, workspace, self.allowed_write_roots.is_empty());
        let outcome = result.authorization_outcome();

        ShellValidation {
            mode: self.shell_validation_mode,
            intent: classify_command(command),
            result,
            outcome,
        }
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
    let candidate_path = normalize_policy_root(path);
    let default_root = normalize_policy_root(default_root);
    candidate_path.starts_with(&default_root)
        || extra_roots
            .iter()
            .map(|root| normalize_policy_root(root))
            .any(|root| candidate_path.starts_with(root))
}

fn normalize_policy_root(path: &Path) -> PathBuf {
    normalize_absolute_path(path)
        .ok()
        .and_then(|normalized| resolve_existing_components(&normalized).ok())
        .unwrap_or_else(|| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
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
    fn shell_validation_defaults_off_and_uses_authorization_semantics() {
        let workspace = test_path("validation-workspace");
        let default_validation =
            RuntimePolicy::default().evaluate_shell_command("rm -rf /tmp/sentinel", &workspace);
        assert_eq!(default_validation.mode, ShellValidationMode::Off);
        assert_eq!(default_validation.intent, CommandIntent::Destructive);
        assert_eq!(default_validation.outcome, ToolAuthorizationOutcome::Deny);
        assert!(!default_validation.should_deny());

        let warned = RuntimePolicy::default()
            .shell_validation(ShellValidationMode::Warn)
            .evaluate_shell_command("rm -rf /tmp/sentinel", &workspace);
        assert!(warned.should_emit_hook());
        assert!(!warned.should_deny());

        let enforced = RuntimePolicy::default()
            .shell_validation(ShellValidationMode::Enforce)
            .evaluate_shell_command("rm -rf /tmp/sentinel", &workspace);
        assert!(enforced.should_emit_hook());
        assert!(enforced.should_deny());

        let enforced_warning = RuntimePolicy::workspace_bounded(&workspace)
            .shell_validation(ShellValidationMode::Enforce)
            .evaluate_shell_command("rm -rf /", &workspace);
        assert_eq!(enforced_warning.outcome, ToolAuthorizationOutcome::Prompt);
        assert!(enforced_warning.should_emit_hook());
        assert!(!enforced_warning.should_deny());
    }

    #[test]
    fn tool_result_limits_have_stable_defaults_and_builders() {
        let defaults = RuntimePolicy::default();
        assert_eq!(defaults.max_tool_result_bytes, 50 * 1024);
        assert_eq!(defaults.max_tool_result_lines, 2_000);
        assert!(defaults.spill_full_tool_output);

        let configured = defaults
            .with_max_tool_result_bytes(123)
            .with_max_tool_result_lines(7)
            .spill_full_tool_output(false);
        assert_eq!(configured.max_tool_result_bytes, 123);
        assert_eq!(configured.max_tool_result_lines, 7);
        assert!(!configured.spill_full_tool_output);
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
