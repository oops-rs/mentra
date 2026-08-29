use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;

use crate::process::{BoundedCommand, Completion};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub status_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.success
    }
}

pub type CommandOutput = ExecOutput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandSpec {
    Shell { command: String },
}

impl CommandSpec {
    pub fn display(&self) -> &str {
        match self {
            Self::Shell { command } => command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub spec: CommandSpec,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
    pub max_output_bytes_per_stream: usize,
    /// Where the host asked this command to run; `None` is the local executor.
    ///
    /// Execution data, not policy: the executor reads it, nothing else decides
    /// on it. A targeted request is authorized, validated, timeout-clamped and
    /// output-capped exactly like a local one, so routing a command elsewhere
    /// can never be a way around the policy that guards running it here. An
    /// executor that does not serve the named target must refuse the request
    /// rather than run it locally.
    ///
    /// Defaulted on deserialization so a request serialized before this field
    /// existed still loads, as the untargeted request it was.
    #[serde(default)]
    pub target: Option<String>,
}

/// Executes runtime command requests.
///
/// Implementations are trusted host components. A sandboxed implementation
/// should be configured with an immutable filesystem and network policy because
/// [`CommandRequest`] intentionally carries execution data, not authorization
/// policy.
#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String>;

    /// Runs an untargeted command.
    ///
    /// The convenience form keeps the signature it always had, so it can only
    /// build a request with [`CommandRequest::target`] set to `None`. A caller
    /// that needs a target builds the [`CommandRequest`] itself and calls
    /// [`run`](Self::run).
    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Duration,
        env: Vec<(String, String)>,
        max_output_bytes_per_stream: usize,
    ) -> Result<CommandOutput, String> {
        self.run(CommandRequest {
            spec: CommandSpec::Shell {
                command: command.to_string(),
            },
            cwd: cwd.to_path_buf(),
            timeout,
            env,
            max_output_bytes_per_stream,
            target: None,
        })
        .await
    }
}

/// Executes commands directly with the current user's host permissions.
///
/// A thin user of [`BoundedCommand`], which is where the confinement lives:
/// unlisted environment variables are cleared, the command runs in its own
/// process group and is killed as a group at the deadline, and each stream is
/// capped while it is read. It does not sandbox filesystem or network access.
///
/// A host that needs the same discipline for a program of its own — an argv
/// vector, a payload on stdin — should reach for [`BoundedCommand`] directly
/// rather than flattening it into a shell string.
///
/// It serves no named target and refuses any request that carries one: a
/// command the host addressed elsewhere silently running on this machine
/// would be the one failure mode a target is meant to prevent.
pub struct LocalRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for LocalRuntimeExecutor {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        let CommandRequest {
            spec,
            cwd,
            timeout,
            env,
            max_output_bytes_per_stream,
            target,
        } = request;
        if let Some(target) = target {
            return Err(format!(
                "no executor serves target `{target}`; the local executor only runs untargeted commands"
            ));
        }
        let command = match spec {
            CommandSpec::Shell { command } => command,
        };

        let completion = BoundedCommand::shell(command, timeout, max_output_bytes_per_stream)
            .current_dir(cwd)
            .envs(env)
            .run()
            .await
            .map_err(|error| format!("Failed to execute command: {error}"))?;

        let (timed_out, status_code, stdout, stderr) = match completion {
            Completion::Exited {
                code,
                stdout,
                stderr,
            } => (false, code, stdout, stderr),
            // The exit code of `timeout(1)`, which is what a caller reading a
            // number expects a killed command to have produced.
            Completion::TimedOut { stdout, stderr } => (true, Some(124), stdout, stderr),
        };

        Ok(CommandOutput {
            success: !timed_out && status_code == Some(0),
            status_code,
            timed_out,
            stdout_truncated: stdout.truncated(),
            stderr_truncated: stderr.truncated(),
            stdout: stdout.to_string_lossy().into_owned(),
            stderr: stderr.to_string_lossy().into_owned(),
        })
    }
}

pub async fn read_limited_file(path: &Path, max_lines: Option<usize>) -> Result<String, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("Failed to open file: {error}"))?;
    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut content = Vec::new();

    loop {
        if let Some(limit) = max_lines
            && content.len() >= limit
        {
            break;
        }

        match lines.next_line().await {
            Ok(Some(line)) => content.push(line),
            Ok(None) => break,
            Err(error) => return Err(format!("Failed to read file: {error}")),
        }
    }

    Ok(content.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn stdout_and_stderr_command() -> String {
        "printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; printf 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2"
            .to_string()
    }

    #[cfg(windows)]
    fn stdout_and_stderr_command() -> String {
        "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa& echo bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 1>&2"
            .to_string()
    }

    #[cfg(unix)]
    fn missing_secret_command() -> String {
        "printf '%s' \"${SECRET:-missing}\"".to_string()
    }

    #[cfg(windows)]
    fn missing_secret_command() -> String {
        "if defined SECRET (echo unexpected) else (echo missing)".to_string()
    }

    #[cfg(unix)]
    fn timeout_command() -> String {
        "sleep 1".to_string()
    }

    #[cfg(windows)]
    fn timeout_command() -> String {
        "ping.exe -n 2 127.0.0.1 >nul".to_string()
    }

    #[cfg(unix)]
    fn minimal_shell_env() -> Vec<(String, String)> {
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").expect("path available"),
        )]
    }

    #[cfg(windows)]
    fn minimal_shell_env() -> Vec<(String, String)> {
        ["PATH", "PATHEXT", "SystemRoot", "COMSPEC", "TEMP", "TMP"]
            .into_iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| (name.to_string(), value))
            })
            .collect()
    }

    #[tokio::test]
    async fn caps_stdout_and_stderr_independently() {
        let output = LocalRuntimeExecutor
            .run(CommandRequest {
                spec: CommandSpec::Shell {
                    command: stdout_and_stderr_command(),
                },
                cwd: std::env::temp_dir(),
                timeout: Duration::from_secs(5),
                env: minimal_shell_env(),
                max_output_bytes_per_stream: 8,
                target: None,
            })
            .await
            .expect("command output");

        assert!(!output.timed_out, "{output:?}");
        assert!(output.success, "{output:?}");
        assert_eq!(output.stdout.len(), 8);
        assert_eq!(output.stderr.len(), 8);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[tokio::test]
    async fn allowlisted_environment_is_enforced() {
        let output = LocalRuntimeExecutor
            .run(CommandRequest {
                spec: CommandSpec::Shell {
                    command: missing_secret_command(),
                },
                cwd: std::env::temp_dir(),
                timeout: Duration::from_secs(5),
                env: minimal_shell_env(),
                max_output_bytes_per_stream: 1024,
                target: None,
            })
            .await
            .expect("command output");

        assert!(!output.timed_out, "{output:?}");
        assert!(output.success, "{output:?}");
        assert_eq!(output.stdout.trim_end(), "missing");
    }

    #[tokio::test]
    async fn timeout_marks_output_and_uses_timeout_exit_code() {
        let output = LocalRuntimeExecutor
            .run(CommandRequest {
                spec: CommandSpec::Shell {
                    command: timeout_command(),
                },
                cwd: std::env::temp_dir(),
                timeout: Duration::from_millis(50),
                env: minimal_shell_env(),
                max_output_bytes_per_stream: 1024,
                target: None,
            })
            .await
            .expect("command output");

        assert!(output.timed_out);
        assert_eq!(output.status_code, Some(124));
        assert!(!output.success);
    }

    #[tokio::test]
    async fn targeted_request_is_refused_instead_of_running_locally() {
        let error = LocalRuntimeExecutor
            .run(CommandRequest {
                spec: CommandSpec::Shell {
                    command: "printf 'ran locally'".to_string(),
                },
                cwd: std::env::temp_dir(),
                timeout: Duration::from_secs(5),
                env: minimal_shell_env(),
                max_output_bytes_per_stream: 1024,
                target: Some("mac".to_string()),
            })
            .await
            .expect_err("a targeted request must not run locally");

        assert_eq!(
            error,
            "no executor serves target `mac`; the local executor only runs untargeted commands"
        );
    }
}
