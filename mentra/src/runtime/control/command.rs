use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(windows)]
use std::process::Command as StdCommand;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

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
}

#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String>;

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
        })
        .await
    }
}

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
        } = request;
        let command = match spec {
            CommandSpec::Shell { command } => command,
        };

        let mut process = Command::new(platform_shell_program());
        process
            .args(platform_shell_args(&command))
            .current_dir(&cwd)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = process
            .spawn()
            .map_err(|error| format!("Failed to execute command: {error}"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Failed to capture stderr".to_string())?;
        let stdout_task = tokio::spawn(read_capped(stdout, max_output_bytes_per_stream));
        let stderr_task = tokio::spawn(read_capped(stderr, max_output_bytes_per_stream));

        let wait_result = tokio::time::timeout(timeout, child.wait()).await;
        let timed_out = wait_result.is_err();
        let status = if timed_out {
            kill_entire_process_tree(&mut child)
                .map_err(|error| format!("Failed to stop timed out command: {error}"))?;
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            None
        } else {
            Some(
                wait_result
                    .expect("non-timeout wait result")
                    .map_err(|error| format!("Failed to wait for command: {error}"))?,
            )
        };

        let stdout = join_stream(stdout_task).await?;
        let stderr = join_stream(stderr_task).await?;

        let (success, status_code) = if timed_out {
            (false, Some(124))
        } else if let Some(status) = status {
            (status.success(), status.code())
        } else {
            (false, None)
        };

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
            success,
            status_code,
            timed_out,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        })
    }
}

struct StreamCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped<R>(mut reader: R, max_bytes: usize) -> io::Result<StreamCapture>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        let remaining = max_bytes.saturating_sub(bytes.len());
        let take = remaining.min(read);
        bytes.extend_from_slice(&buffer[..take]);
        if take < read {
            truncated = true;
        }
    }

    Ok(StreamCapture { bytes, truncated })
}

async fn join_stream(
    handle: tokio::task::JoinHandle<io::Result<StreamCapture>>,
) -> Result<StreamCapture, String> {
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .map_err(|_| "Timed out while draining command output".to_string())?
        .map_err(|error| format!("Failed to join command output task: {error}"))?
        .map_err(|error| format!("Failed to read command output: {error}"))
}

fn kill_entire_process_tree(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            let status = StdCommand::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status()?;
            if status.success() {
                return Ok(());
            }

            if child.try_wait()?.is_some() {
                return Ok(());
            }
        }
    }

    child.start_kill()
}

#[cfg(unix)]
fn platform_shell_program() -> &'static str {
    "/bin/sh"
}

#[cfg(windows)]
fn platform_shell_program() -> &'static str {
    "cmd.exe"
}

#[cfg(unix)]
fn platform_shell_args(command: &str) -> [&str; 2] {
    ["-c", command]
}

#[cfg(windows)]
fn platform_shell_args(command: &str) -> [&str; 2] {
    ["/C", command]
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
        "powershell.exe -NoProfile -Command \"$stdout='a' * 32; $stderr='b' * 32; [Console]::Out.Write($stdout); [Console]::Error.Write($stderr)\"".to_string()
    }

    #[cfg(unix)]
    fn missing_secret_command() -> String {
        "printf '%s' \"${SECRET:-missing}\"".to_string()
    }

    #[cfg(windows)]
    fn missing_secret_command() -> String {
        "cmd.exe /V:OFF /C \"if defined SECRET (set /p =%SECRET%<nul) else (set /p =missing<nul)\""
            .to_string()
    }

    #[cfg(unix)]
    fn timeout_command() -> String {
        "sleep 1".to_string()
    }

    #[cfg(windows)]
    fn timeout_command() -> String {
        "powershell.exe -NoProfile -Command \"Start-Sleep -Seconds 1\"".to_string()
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
            })
            .await
            .expect("command output");

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
            })
            .await
            .expect("command output");

        assert_eq!(output.stdout, "missing");
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
            })
            .await
            .expect("command output");

        assert!(output.timed_out);
        assert_eq!(output.status_code, Some(124));
        assert!(!output.success);
    }
}
