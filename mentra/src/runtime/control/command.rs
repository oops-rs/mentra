use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub status_code: Option<i32>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.success
    }

    pub fn foreground_result(self) -> Result<String, String> {
        if self.success {
            Ok(self.stdout)
        } else {
            let stderr = self.stderr.trim();
            if stderr.is_empty() {
                Err(match self.status_code {
                    Some(code) => format!("Command exited with status {code}"),
                    None => "Command exited unsuccessfully".to_string(),
                })
            } else {
                Err(self.stderr)
            }
        }
    }
}

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
    pub timeout: Option<Duration>,
}

#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String>;

    async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<CommandOutput, String> {
        self.run(CommandRequest {
            spec: CommandSpec::Shell {
                command: command.to_string(),
            },
            cwd: cwd.to_path_buf(),
            timeout,
        })
        .await
    }
}

pub struct LocalRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for LocalRuntimeExecutor {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        let CommandRequest { spec, cwd, timeout } = request;
        let command = match spec {
            CommandSpec::Shell { command } => command,
        };
        let mut process = Command::new("bash");
        process.arg("-c").arg(&command).current_dir(&cwd);

        let output = if let Some(timeout) = timeout {
            match tokio::time::timeout(timeout, process.output()).await {
                Ok(result) => {
                    result.map_err(|error| format!("Failed to execute command: {error}"))?
                }
                Err(_) => {
                    return Err(format!(
                        "Command timed out after {}s",
                        timeout.as_secs_f64()
                    ));
                }
            }
        } else {
            process
                .output()
                .await
                .map_err(|error| format!("Failed to execute command: {error}"))?
        };

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            success: output.status.success(),
            status_code: output.status.code(),
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
