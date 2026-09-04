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
/// signal. It is heuristic and is not a security boundary. Working-directory
/// checks do not confine a shell process; filesystem and network isolation
/// require an OS-enforced [`crate::runtime::RuntimeExecutor`].
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
    denied_write_roots: Vec<PathBuf>,
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
            denied_write_roots: Vec::new(),
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

    /// Returns a workspace-bounded policy for builtin file tools.
    ///
    /// Shell and background execution remain disabled because the builtin
    /// local executor runs directly on the host and a working-directory check
    /// cannot confine filesystem or network effects. Hosts that install an
    /// OS-enforced executor through [`crate::runtime::Runtime::builder`] may
    /// explicitly opt in with [`Self::allow_shell_commands`] and
    /// [`Self::allow_background_commands`].
    pub fn workspace_bounded(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            allowed_working_roots: vec![workspace.clone()],
            allowed_read_roots: vec![workspace.clone()],
            allowed_write_roots: vec![workspace],
            default_command_timeout: Duration::from_secs(120),
            max_command_timeout: Duration::from_secs(600),
            ..Self::default()
        }
    }

    /// Returns a policy that allows builtin file reads but blocks builtin file
    /// writes and host shell execution.
    ///
    /// A host may opt into shell execution only after installing an executor
    /// that enforces read-only filesystem and network policy at the OS boundary.
    pub fn read_only(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            allow_background_commands: false,
            allowed_working_roots: vec![workspace.clone()],
            allowed_read_roots: vec![workspace],
            allowed_write_roots: Vec::new(),
            ..Self::default()
        }
    }

    /// Enables or disables foreground shell command execution.
    ///
    /// This switch grants authority to the configured executor; it does not
    /// sandbox the builtin `LocalRuntimeExecutor`.
    pub fn allow_shell_commands(mut self, allow: bool) -> Self {
        self.allow_shell_commands = allow;
        self
    }

    /// Enables or disables background shell command execution.
    ///
    /// This switch grants authority to the configured executor; it does not
    /// sandbox the builtin `LocalRuntimeExecutor`.
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

    /// Carves a hole in the write roots: a path under `path` is refused even
    /// when an allow-root would otherwise permit it.
    ///
    /// For the places inside a workspace that an agent should not be able to
    /// change because changing them changes what runs — `.git/hooks` being the
    /// canonical one, since a file written there executes on the next commit.
    /// Allow-roots alone cannot express it: the whole workspace is writable and
    /// these are inside the workspace.
    ///
    /// **This binds mentra's builtin file tools, not the shell.** A command
    /// like `sh -c 'echo … > .git/hooks/pre-commit'` still reaches the path,
    /// because the runtime does not parse shell and cannot know where a
    /// redirect points. Treat this as hygiene that closes the obvious route,
    /// never as a boundary — the boundary belongs to the OS.
    pub fn with_denied_write_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.denied_write_roots.push(path.into());
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

        // Checked before the allow list, because a denial is only meaningful
        // inside a root that would otherwise permit the write. Both sides
        // normalize through `normalize_policy_root`, so `.git/hooks/../hooks`
        // and a symlink into a denied root resolve to the same answer.
        if path_is_under_any(resolved.as_path(), self.denied_write_roots.as_slice()) {
            return Err(format!(
                "Path '{}' is inside a runtime policy denied write root",
                resolved.display()
            ));
        }

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

/// Whether `path` sits under any of `roots`, comparing resolved forms.
fn path_is_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let candidate = normalize_policy_root(path);
    roots
        .iter()
        .map(|root| normalize_policy_root(root))
        .any(|root| candidate.starts_with(root))
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

/// Returns the best-effort normalized path spelling used for policy-root
/// comparisons.
///
/// Absolute paths have lexical `.` and `..` components folded, their deepest
/// existing prefix canonicalized, and any non-existent suffix preserved. If
/// that process fails, this falls back to [`fs::canonicalize`] and finally to
/// the input path unchanged. A relative or otherwise unresolvable path may
/// therefore remain relative or unresolved.
///
/// This function only normalizes a spelling. It does not validate, authorize,
/// create, or confine filesystem access, and filesystem state can change after
/// it returns. Enforcement that must resist races or shell side effects still
/// requires an OS-level sandbox.
pub fn normalize_policy_root(path: &Path) -> PathBuf {
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
    let mut existing_prefix = path.to_path_buf();
    let mut missing_suffix = Vec::new();

    loop {
        match fs::canonicalize(&existing_prefix) {
            Ok(mut resolved) => {
                for component in missing_suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&existing_prefix) {
                    Ok(_) => {
                        return Err(format!(
                            "Failed to resolve existing path '{}': {error}",
                            existing_prefix.display()
                        ));
                    }
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(metadata_error) => {
                        return Err(format!(
                            "Failed to inspect '{}': {metadata_error}",
                            existing_prefix.display()
                        ));
                    }
                }

                let component = existing_prefix.file_name().ok_or_else(|| {
                    format!(
                        "Path '{}' has no existing prefix to resolve",
                        path.display()
                    )
                })?;
                missing_suffix.push(component.to_os_string());
                existing_prefix.pop();
            }
            Err(error) => {
                return Err(format!(
                    "Failed to resolve existing path '{}': {error}",
                    existing_prefix.display()
                ));
            }
        }
    }
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
    fn bounded_policies_keep_host_shell_execution_disabled() {
        let workspace = test_path("bounded-repo");

        for policy in [
            RuntimePolicy::workspace_bounded(&workspace),
            RuntimePolicy::read_only(&workspace),
        ] {
            let error = policy
                .authorize_command_execution(&workspace, &workspace, false)
                .expect_err("bounded policy must not authorize the host shell");
            assert!(error.contains("Shell command execution is disabled"));
        }
    }

    #[test]
    fn bounded_policy_allows_explicit_external_executor_opt_in() {
        let workspace = test_path("sandboxed-repo");
        let policy = RuntimePolicy::workspace_bounded(&workspace)
            .allow_shell_commands(true)
            .allow_background_commands(true);

        policy
            .authorize_command_execution(&workspace, &workspace, false)
            .expect("foreground shell opt-in");
        policy
            .authorize_command_execution(&workspace, &workspace, true)
            .expect("background shell opt-in");
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
    #[test]
    fn a_denied_root_inside_an_allowed_one_refuses_the_write() {
        let root = unique_temp_dir("policy-deny-root");
        let hooks = root.join(".git").join("hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");

        let policy = RuntimePolicy::default()
            .with_allowed_write_root(&root)
            .with_denied_write_root(&hooks);

        // The whole point: the workspace is writable and this is inside it, so
        // allow-roots alone could never express the carve-out.
        let error = policy
            .authorize_file_write(&root, &hooks.join("pre-commit"))
            .expect_err("a denied root must win over the allow root containing it");
        assert!(error.contains("denied write root"), "got: {error}");

        // A sibling under the same allow root is untouched.
        policy
            .authorize_file_write(&root, &root.join("src.rs"))
            .expect("an ordinary write is unaffected");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_traversal_into_a_denied_root_is_refused() {
        let root = unique_temp_dir("policy-deny-traverse");
        let hooks = root.join(".git").join("hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");

        let policy = RuntimePolicy::default()
            .with_allowed_write_root(&root)
            .with_denied_write_root(&hooks);

        // Spelled to look like it lands elsewhere. Both sides normalize, so
        // the spelling does not decide the answer.
        let sneaky = root.join(".git").join("hooks").join("..").join("hooks");
        let error = policy
            .authorize_file_write(&root, &sneaky.join("pre-push"))
            .expect_err("a path that resolves into a denied root is still denied");
        assert!(error.contains("denied write root"), "got: {error}");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_a_denied_root_is_refused() {
        use std::os::unix::fs::symlink;

        let root = unique_temp_dir("policy-deny-symlink");
        let hooks = root.join(".git").join("hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");
        let link = root.join("shortcut");
        symlink(&hooks, &link).expect("create symlink");

        let policy = RuntimePolicy::default()
            .with_allowed_write_root(&root)
            .with_denied_write_root(&hooks);

        let error = policy
            .authorize_file_write(&root, &link.join("pre-commit"))
            .expect_err("a symlink is not a way around a denied root");
        assert!(error.contains("denied write root"), "got: {error}");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn no_denied_roots_changes_nothing() {
        let root = unique_temp_dir("policy-deny-empty");
        fs::create_dir_all(&root).expect("create root");

        let policy = RuntimePolicy::default().with_allowed_write_root(&root);

        policy
            .authorize_file_write(&root, &root.join(".git").join("hooks").join("pre-commit"))
            .expect("with no deny list the allow root decides alone");

        let _ = fs::remove_dir_all(&root);
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
