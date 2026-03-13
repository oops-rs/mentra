use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    ApprovalPolicy, CommandEvaluation, Decision, ExecRule, ParsedCommand, RuleMatch, ShellRequest,
    parse_command,
};

/// Authorization policy for builtin shell, background, and file tools.
#[derive(Debug, Clone)]
pub struct RuntimePolicy {
    allow_shell_commands: bool,
    allow_background_commands: bool,
    allowed_working_roots: Vec<PathBuf>,
    allowed_read_roots: Vec<PathBuf>,
    allowed_env_vars: Vec<String>,
    approval_policy: ApprovalPolicy,
    exec_rules: Vec<ExecRule>,
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
            allowed_env_vars: vec!["PATH".to_string()],
            approval_policy: ApprovalPolicy::UnlessAllowed,
            exec_rules: Vec::new(),
            background_task_limit: Some(8),
            default_command_timeout: Duration::from_secs(30),
            max_command_timeout: Duration::from_secs(30),
            max_output_bytes_per_stream: 64 * 1024,
        }
    }
}

impl RuntimePolicy {
    /// Returns a permissive policy that enables shell and background execution.
    pub fn permissive() -> Self {
        Self {
            allow_shell_commands: true,
            allow_background_commands: true,
            approval_policy: ApprovalPolicy::Never,
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

    /// Adds an explicit shell execution rule.
    pub fn with_exec_rule(mut self, rule: ExecRule) -> Self {
        self.exec_rules.push(rule);
        self
    }

    /// Sets the approval policy for evaluated shell commands.
    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval_policy = policy;
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

    pub(crate) fn evaluate_shell_request(
        &self,
        base_dir: &Path,
        request: &ShellRequest,
    ) -> Result<CommandEvaluation, String> {
        self.authorize_command_roots(base_dir, &request.cwd, request.background)?;

        let parsed = parse_command(&request.command, &request.cwd);
        let mut decision = Decision::Allow;
        let mut matched_rules = Vec::new();

        for stage in &parsed.stages {
            for argv in &stage.commands {
                if let Some(rule) = self.exec_rules.iter().find(|rule| rule.matches(argv)) {
                    decision = decision.merge(rule.decision);
                    matched_rules.push(RuleMatch {
                        summary: rule.summary(),
                        decision: rule.decision,
                        justification: rule.justification.clone(),
                    });
                }
            }
        }

        if matched_rules.is_empty() {
            decision = parsed
                .stages
                .iter()
                .fold(Decision::Allow, |current, stage| {
                    current.merge(match stage.parsed {
                        ParsedCommand::Read { .. }
                        | ParsedCommand::Search { .. }
                        | ParsedCommand::ListFiles { .. } => Decision::Allow,
                        ParsedCommand::Unknown { .. } => Decision::Prompt,
                    })
                });
        }

        decision = match self.approval_policy {
            ApprovalPolicy::Never => {
                if decision == Decision::Prompt {
                    Decision::Allow
                } else {
                    decision
                }
            }
            ApprovalPolicy::Always => {
                if decision == Decision::Allow {
                    Decision::Prompt
                } else {
                    decision
                }
            }
            ApprovalPolicy::UnlessAllowed => decision,
            ApprovalPolicy::OnRequest => {
                if decision == Decision::Allow && request.justification.is_some() {
                    Decision::Prompt
                } else {
                    decision
                }
            }
        };

        Ok(CommandEvaluation {
            parsed: parsed.parsed,
            stages: parsed.stages,
            decision,
            matched_rules,
        })
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
    path.starts_with(default_root) || extra_roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_fallback_allows_safe_commands_and_prompts_unknown() {
        let cwd = PathBuf::from("/tmp/repo");
        let policy = RuntimePolicy::default().allow_shell_commands(true);

        let safe = policy
            .evaluate_shell_request(
                &cwd,
                &ShellRequest {
                    command: "cat README.md".to_string(),
                    cwd: cwd.clone(),
                    requested_timeout: None,
                    justification: None,
                    background: false,
                },
            )
            .expect("safe request");
        assert_eq!(safe.decision, Decision::Allow);

        let unknown = policy
            .evaluate_shell_request(
                &cwd,
                &ShellRequest {
                    command: "python -c 'print(1)'".to_string(),
                    cwd: cwd.clone(),
                    requested_timeout: None,
                    justification: None,
                    background: false,
                },
            )
            .expect("unknown request");
        assert_eq!(unknown.decision, Decision::Prompt);
    }

    #[test]
    fn explicit_rules_override_heuristics() {
        let cwd = PathBuf::from("/tmp/repo");
        let policy = RuntimePolicy::default()
            .allow_shell_commands(true)
            .with_exec_rule(ExecRule::new("cat", ["README.md"], Decision::Forbidden));
        let result = policy
            .evaluate_shell_request(
                &cwd,
                &ShellRequest {
                    command: "cat README.md".to_string(),
                    cwd: cwd.clone(),
                    requested_timeout: None,
                    justification: None,
                    background: false,
                },
            )
            .expect("evaluated request");
        assert_eq!(result.decision, Decision::Forbidden);
        assert_eq!(result.matched_rules.len(), 1);
    }

    #[test]
    fn shell_roots_and_background_switches_short_circuit() {
        let cwd = PathBuf::from("/tmp/repo");
        let policy = RuntimePolicy::default()
            .allow_shell_commands(true)
            .allow_background_commands(false);
        let error = policy
            .evaluate_shell_request(
                &cwd,
                &ShellRequest {
                    command: "cat README.md".to_string(),
                    cwd: cwd.clone(),
                    requested_timeout: None,
                    justification: None,
                    background: true,
                },
            )
            .expect_err("background should be disabled");
        assert!(error.contains("Background command execution is disabled"));
    }
}
