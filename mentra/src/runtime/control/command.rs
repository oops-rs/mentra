use std::{
    collections::VecDeque,
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
/// This executor clears unlisted environment variables and enforces output,
/// timeout, and timeout-cleanup limits. It does not sandbox filesystem or
/// network access.
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

/// Bytes held back from `max_bytes` to pay for the elision marker when a
/// capture keeps both ends of a stream. The marker is
/// `\n[... <n> bytes elided ...]\n`, at most 45 bytes for any `u64`.
const ELISION_MARKER_RESERVE: usize = 64;

/// Caps below this keep the head alone. Splitting a budget this small between
/// two windows and a marker leaves neither window long enough to say anything.
const MIN_CAP_FOR_TWO_WINDOWS: usize = 256;

/// How far into the kept tail to look for a line boundary to start on, so the
/// tail does not open mid-line. A stream with no newline that close — one long
/// JSON line, say — is kept as-is rather than searched to its end.
const TAIL_LINE_BOUNDARY_WINDOW: usize = 512;

/// Reads `reader` to EOF, keeping at most `max_bytes` of it.
///
/// The whole stream is always drained, so a child process is never blocked on a
/// full pipe by a cap this side of it. What is *kept* is the head and the tail:
/// a command's most load-bearing output is at both ends — the command echo and
/// early context at the start, the assertion that failed or the stack that
/// unwound at the end — and keeping only the head is keeping the half that says
/// a run started. What fell out between them is replaced by a marker naming its
/// size, so the result never reads as contiguous output.
///
/// The kept bytes never exceed `max_bytes`, marker included.
async fn read_capped<R>(mut reader: R, max_bytes: usize) -> io::Result<StreamCapture>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (head_budget, tail_budget) = if max_bytes < MIN_CAP_FOR_TWO_WINDOWS {
        (max_bytes, 0)
    } else {
        let split = max_bytes - ELISION_MARKER_RESERVE;
        let head = split / 2;
        (head, split - head)
    };

    let mut head = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut elided = 0_u64;
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let mut chunk = &buffer[..read];

        let head_room = head_budget.saturating_sub(head.len());
        if head_room > 0 {
            let take = head_room.min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if chunk.is_empty() {
            continue;
        }

        if tail_budget == 0 {
            elided += chunk.len() as u64;
            continue;
        }

        // Keep the last `tail_budget` bytes seen, counting what falls off the
        // front rather than growing without bound.
        if chunk.len() >= tail_budget {
            elided += tail.len() as u64 + (chunk.len() - tail_budget) as u64;
            tail.clear();
            tail.extend(&chunk[chunk.len() - tail_budget..]);
        } else {
            let overflow = (tail.len() + chunk.len()).saturating_sub(tail_budget);
            elided += overflow as u64;
            tail.drain(..overflow);
            tail.extend(chunk);
        }
    }

    // Nothing fell out: head and tail are still one contiguous run of bytes.
    if elided == 0 {
        head.extend(tail);
        return Ok(StreamCapture {
            bytes: head,
            truncated: false,
        });
    }

    if tail.is_empty() {
        return Ok(StreamCapture {
            bytes: head,
            truncated: true,
        });
    }

    let tail = Vec::from(tail);
    let boundary = tail[..TAIL_LINE_BOUNDARY_WINDOW.min(tail.len())]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .filter(|start| *start < tail.len())
        .unwrap_or(0);
    let elided = elided + boundary as u64;

    let mut bytes = head;
    bytes.extend_from_slice(format!("\n[... {elided} bytes elided ...]\n").as_bytes());
    bytes.extend_from_slice(&tail[boundary..]);

    Ok(StreamCapture {
        bytes,
        truncated: true,
    })
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

    /// Feeds `input` through the capture used for a child process's stdout.
    async fn capture(input: &[u8], max_bytes: usize) -> StreamCapture {
        read_capped(std::io::Cursor::new(input.to_vec()), max_bytes)
            .await
            .expect("cursor never fails to read")
    }

    #[tokio::test]
    async fn a_stream_under_the_cap_is_byte_identical() {
        let input = b"line one\nline two\nline three\n";

        let captured = capture(input, 4096).await;

        assert_eq!(captured.bytes, input);
        assert!(!captured.truncated);
    }

    #[tokio::test]
    async fn a_capped_stream_keeps_the_end_a_failure_is_reported_at() {
        // A test runner names what failed on its last lines. Keeping only the
        // head of a capped stream is keeping the half that says a run started.
        let mut input = String::from("FIRST LINE\n");
        for index in 0..4000 {
            input.push_str(&format!("filler line {index}\n"));
        }
        input.push_str("LAST LINE: assertion failed\n");

        let captured = capture(input.as_bytes(), 4096).await;

        assert!(captured.truncated);
        assert!(captured.bytes.len() <= 4096, "cap is still a hard bound");
        let text = String::from_utf8_lossy(&captured.bytes);
        assert!(text.starts_with("FIRST LINE\n"), "{text}");
        assert!(text.ends_with("LAST LINE: assertion failed\n"), "{text}");
        assert!(text.contains("bytes elided"), "{text}");
    }

    #[tokio::test]
    async fn a_cap_too_small_to_split_keeps_the_head() {
        // Below the split threshold there is no room for two windows and a
        // marker, so the capture stays exactly what it has always been.
        let captured = capture(b"aaaaaaaaaaaaaaaaaaaaaaaa", 8).await;

        assert_eq!(captured.bytes, b"aaaaaaaa");
        assert!(captured.truncated);
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
