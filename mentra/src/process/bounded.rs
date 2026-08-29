use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[cfg(windows)]
use std::process::Command as StdCommand;

use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout, Command},
    task::JoinHandle,
    time::Instant,
};

use super::capture::{CapturedStream, read_capped};

/// How long a program that exited on time still gets for its pipes to drain.
///
/// One that answers with a millisecond to spare should not lose its output to
/// the scheduling latency between its exit and the last bytes arriving.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// How long cleanup after a kill may take: reaping the child, then reading
/// what its now-closed pipes still hold.
const CLEANUP_GRACE: Duration = Duration::from_secs(2);

/// How a program ended.
///
/// Both variants carry whatever the program printed, bounded the same way: a
/// half-written answer is not an answer, but it is often the only evidence of
/// what went wrong, and discarding it is the caller's decision to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// The program ran to completion inside its budget.
    Exited {
        /// The exit code, or `None` when a signal ended it — a failure like
        /// any other, and not something every platform gives a number for.
        code: Option<i32>,
        stdout: CapturedStream,
        stderr: CapturedStream,
    },
    /// The budget ran out first, and the process tree was killed.
    ///
    /// Reached either because the program was still running at the deadline or
    /// because its output was still arriving past it — a descendant holding
    /// the pipes open is the same missed deadline as a program that never
    /// exited.
    TimedOut {
        stdout: CapturedStream,
        stderr: CapturedStream,
    },
}

impl Completion {
    /// Whether the budget, rather than the program, decided when this ended.
    pub fn timed_out(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }

    /// The exit code, or `None` for a signal or a timeout.
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Exited { code, .. } => *code,
            Self::TimedOut { .. } => None,
        }
    }

    /// What the program printed on stdout before it ended.
    pub fn stdout(&self) -> &CapturedStream {
        match self {
            Self::Exited { stdout, .. } | Self::TimedOut { stdout, .. } => stdout,
        }
    }

    /// What the program printed on stderr before it ended.
    pub fn stderr(&self) -> &CapturedStream {
        match self {
            Self::Exited { stderr, .. } | Self::TimedOut { stderr, .. } => stderr,
        }
    }
}

/// Runs another program under the process discipline mentra applies to its own
/// commands.
///
/// The guarantees a caller gets, all of them enforced by
/// [`run`](Self::run) rather than by convention:
///
/// - **The environment is exactly what was passed.** The child's environment is
///   cleared before the pairs given to [`env`](Self::env) / [`envs`](Self::envs)
///   are set, so nothing this process happens to be holding — a token, a proxy
///   setting, a `PATH` — reaches the program unless the caller listed it.
/// - **The process tree is grouped and killed as a unit.** On unix the child
///   is put in its own session with `setsid`, and the deadline kills the whole
///   group; a program that backgrounds work cannot leave it running. On Windows
///   the tree is killed with `taskkill /T /F`.
/// - **One budget covers spawning, running, and reading.** A descendant that
///   inherits the pipes cannot hold a caller past the deadline: output still
///   arriving then is a [`Completion::TimedOut`], not a wait.
/// - **Output is capped while it is read.** Neither stream can cost more than
///   the configured stdout/stderr cap, however much the program prints, and both
///   are still drained so the program is never blocked on a full pipe.
/// - **A dropped run kills the child.** The command is spawned with
///   `kill_on_drop`, so a cancelled future does not leak a process.
///
/// Both bounds are constructor arguments rather than options with defaults:
/// there is no way to spell an unbounded run of this type, which is the whole
/// of what "bounded" means here.
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// use std::time::Duration;
/// use mentra::process::{BoundedCommand, Completion};
///
/// let completion = BoundedCommand::new("./hooks/guard.sh", Duration::from_secs(5), 64 * 1024)
///     .current_dir("/repo")
///     .env("BASIS_EVENT", "pre_tool_use")
///     .stdin(r#"{"tool":"shell"}"#)
///     .run()
///     .await?;
///
/// match completion {
///     Completion::Exited { code, stdout, .. } if code == Some(0) => {
///         println!("{}", stdout.to_string_lossy());
///     }
///     _ => println!("the hook did not answer"),
/// }
/// # Ok(())
/// # }
/// ```
pub struct BoundedCommand {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
}

/// The small environment baseline used by interactive host-owned processes.
///
/// `BoundedCommand` itself always starts from an empty environment and callers
/// choose which names to add. MCP stdio uses this shared baseline so its bare
/// command lookup and ordinary language runtimes keep working without
/// receiving ambient credentials or shell state.
#[cfg(not(windows))]
const BASELINE_ENVIRONMENT: &[&str] = &["PATH", "HOME", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL"];

#[cfg(windows)]
const BASELINE_ENVIRONMENT: &[&str] = &["PATH", "PATHEXT", "SystemRoot", "COMSPEC", "TEMP", "TMP"];

pub(crate) fn baseline_environment() -> Vec<(OsString, OsString)> {
    BASELINE_ENVIRONMENT
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect()
}

/// A child started with the process discipline, kept alive for a bidirectional
/// protocol such as MCP rather than consumed by [`BoundedCommand::run`].
pub(crate) struct BoundedChild {
    child: Child,
    group: Option<u32>,
    stderr_task: Option<JoinHandle<io::Result<CapturedStream>>>,
    armed: bool,
}

impl BoundedChild {
    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Terminates the process group and waits briefly for the direct child.
    pub(crate) async fn terminate(&mut self) -> io::Result<()> {
        if self.armed {
            kill_tree_and_reap(&mut self.child, self.group).await?;
            self.armed = false;
        }
        self.abort_stderr_reader();
        Ok(())
    }

    fn abort_stderr_reader(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    /// Whether the stderr reader is still installed for this child.
    ///
    /// Only exposed to crate tests: the observable contract is that a server
    /// cannot block on a full stderr pipe, not the task representation.
    #[cfg(test)]
    pub(crate) fn drains_stderr(&self) -> bool {
        self.stderr_task.is_some()
    }
}

impl Drop for BoundedChild {
    fn drop(&mut self) {
        if self.armed {
            // Drop cannot await, but the group signal is synchronous. The
            // Child's `kill_on_drop` flag covers the direct process as well;
            // this extra group signal is what prevents descendants surviving a
            // disconnected protocol client.
            let _ = kill_entire_process_tree(&mut self.child, self.group);
            self.armed = false;
        }
        self.abort_stderr_reader();
    }
}

impl fmt::Debug for BoundedCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_names = self
            .env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        f.debug_struct("BoundedCommand")
            .field("program", &self.program)
            .field("arg_count", &self.args.len())
            .field("current_dir", &self.current_dir)
            .field("env_names", &env_names)
            .field("stdin_bytes", &self.stdin.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("stdout_max_bytes", &self.max_stdout_bytes)
            .field("stderr_max_bytes", &self.max_stderr_bytes)
            .finish()
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;

    #[test]
    fn debug_redacts_values_and_payload_but_identifies_the_command() {
        let command = BoundedCommand::new("/bin/echo", Duration::from_secs(3), 128)
            .arg("--api-token")
            .arg("argv-secret")
            .env("API_TOKEN", "environment-secret")
            .stdin("stdin-secret")
            .max_stdout_bytes(256)
            .max_stderr_bytes(64);

        let rendered = format!("{command:?}");
        assert!(rendered.contains("/bin/echo"), "{rendered}");
        assert!(rendered.contains("API_TOKEN"), "{rendered}");
        assert!(rendered.contains("stdout_max_bytes: 256"), "{rendered}");
        assert!(rendered.contains("stderr_max_bytes: 64"), "{rendered}");
        assert!(!rendered.contains("environment-secret"), "{rendered}");
        assert!(!rendered.contains("argv-secret"), "{rendered}");
        assert!(!rendered.contains("stdin-secret"), "{rendered}");
    }
}

impl BoundedCommand {
    /// Runs `program` directly, with no shell between the caller and it.
    ///
    /// The argv vector is passed through as given, so an argument containing
    /// spaces, quotes or a `$` needs no escaping and means exactly itself.
    pub fn new(
        program: impl Into<OsString>,
        timeout: Duration,
        max_output_bytes_per_stream: usize,
    ) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            stdin: None,
            timeout,
            max_stdout_bytes: max_output_bytes_per_stream,
            max_stderr_bytes: max_output_bytes_per_stream,
        }
    }

    /// Runs `command` through the platform shell — `/bin/sh -c` on unix,
    /// `cmd.exe /C` on Windows.
    ///
    /// This is what a shell tool wants and what an argv caller does not: the
    /// command is one string the shell parses, so everything in it is the
    /// caller's to quote.
    pub fn shell(
        command: impl Into<OsString>,
        timeout: Duration,
        max_output_bytes_per_stream: usize,
    ) -> Self {
        let command = command.into();
        #[cfg(unix)]
        let (program, flag) = ("/bin/sh", "-c");
        #[cfg(windows)]
        let (program, flag) = ("cmd.exe", "/C");

        Self::new(program, timeout, max_output_bytes_per_stream)
            .arg(flag)
            .arg(command)
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Runs the program in `dir`.
    ///
    /// A relative program path with a directory part — `./hooks/guard.sh` — is
    /// resolved against `dir`, so it means the same thing wherever the host
    /// process was started from. A bare name is left for `PATH` to answer.
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Adds one variable to the child's otherwise empty environment.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Adds several variables to the child's otherwise empty environment.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.extend(
            vars.into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    /// Writes `payload` to the program's stdin, then closes it.
    ///
    /// Without this the child gets a null stdin, which is what a program that
    /// reads nothing should see. The payload is written from its own task, so a
    /// program that answers without ever reading — `echo '{"allow":true}'` is a
    /// legitimate hook — cannot deadlock a caller whose payload outgrew the
    /// pipe buffer. The write ending in `EPIPE` is not an error: what the
    /// program printed and how it exited are the answer.
    pub fn stdin(mut self, payload: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(payload.into());
        self
    }

    /// Sets the maximum bytes retained from stdout.
    ///
    /// The constructor's `max_output_bytes_per_stream` remains the initial cap
    /// for both streams; use this additive builder when stdout needs its own
    /// budget.
    pub fn max_stdout_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stdout_bytes = max_bytes;
        self
    }

    /// Sets the maximum bytes retained from stderr.
    ///
    /// The constructor's `max_output_bytes_per_stream` remains the initial cap
    /// for both streams; use this additive builder when stderr needs its own
    /// budget.
    pub fn max_stderr_bytes(mut self, max_bytes: usize) -> Self {
        self.max_stderr_bytes = max_bytes;
        self
    }

    /// Starts an interactive process with piped stdin/stdout and a bounded
    /// stderr drain. This is crate-internal because the public primitive's
    /// contract is the one-shot [`run`](Self::run) operation; protocol clients
    /// own their request deadlines separately.
    pub(crate) fn spawn_piped(&self) -> io::Result<BoundedChild> {
        let mut child = self.spawn_with_stdio(Stdio::piped(), Stdio::piped(), Stdio::piped())?;
        let group = child.id();
        let stderr = child.stderr.take().ok_or_else(|| missing_pipe("stderr"))?;
        let stderr_task = tokio::spawn(read_capped(stderr, self.max_stderr_bytes));
        Ok(BoundedChild {
            child,
            group,
            stderr_task: Some(stderr_task),
            armed: true,
        })
    }

    /// Spawns the program and waits for it, inside the budget.
    ///
    /// `Err` means the program could not be started or supervised at all. A
    /// program that ran and misbehaved — a non-zero exit, a flood of output, a
    /// missed deadline — is a [`Completion`], because what that means is the
    /// caller's to decide.
    pub async fn run(self) -> io::Result<Completion> {
        // Started before the spawn, because forking a process is part of what
        // the program is being given time for.
        let deadline = Instant::now() + self.timeout;
        let mut child = self.spawn()?;
        // Recorded now because a reaped child has no id left to ask for, and
        // the group kill below can outlive the program that led the group.
        let group = child.id();

        let stdin_task = match self.stdin {
            Some(payload) => {
                let mut pipe = child.stdin.take().ok_or_else(|| missing_pipe("stdin"))?;
                Some(tokio::spawn(async move {
                    // A broken pipe here is not an error: a program that
                    // answers without reading is legitimate, and this only
                    // offers the payload. Dropping the handle at the end of the
                    // task is what tells a program that does read where the
                    // payload stopped.
                    let _ = pipe.write_all(&payload).await;
                    let _ = pipe.shutdown().await;
                }))
            }
            None => None,
        };
        let stdout = child.stdout.take().ok_or_else(|| missing_pipe("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing_pipe("stderr"))?;
        let stdout_cap = self.max_stdout_bytes;
        let stderr_cap = self.max_stderr_bytes;
        let mut streams = Drain {
            stdout: DrainingStream::new(tokio::spawn(read_capped(stdout, stdout_cap))),
            stderr: DrainingStream::new(tokio::spawn(read_capped(stderr, stderr_cap))),
        };

        let waited = tokio::time::timeout_at(deadline, child.wait()).await;
        let mut timed_out = waited.is_err();
        let code = match waited {
            Ok(status) => status?.code(),
            Err(_) => {
                kill_tree_and_reap(&mut child, group).await?;
                None
            }
        };

        // A program that exited on time still gets the rest of its budget, and
        // at least the grace, for output already in flight. Output still
        // arriving after that is a descendant holding the pipes: the same
        // missed deadline as a program that never exited, so the tree goes and
        // the completion says so.
        let drain_deadline = if timed_out {
            Instant::now() + CLEANUP_GRACE
        } else {
            deadline.max(Instant::now() + DRAIN_GRACE)
        };
        if !streams.drained_by(drain_deadline).await? {
            timed_out = true;
            kill_tree_and_reap(&mut child, group).await?;
            if !streams.drained_by(Instant::now() + CLEANUP_GRACE).await? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out while draining command output",
                ));
            }
        }

        // The payload is no longer wanted by anyone: a program that read it has
        // exited, and one that did not was killed.
        if let Some(task) = stdin_task {
            task.abort();
        }

        let (stdout, stderr) = streams.into_captures();
        Ok(if timed_out {
            Completion::TimedOut { stdout, stderr }
        } else {
            Completion::Exited {
                code,
                stdout,
                stderr,
            }
        })
    }

    /// Starts the child with every process and environment rule this type
    /// promises applied.
    fn spawn(&self) -> io::Result<Child> {
        self.spawn_with_stdio(
            if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            },
            Stdio::piped(),
            Stdio::piped(),
        )
    }

    fn spawn_with_stdio(&self, stdin: Stdio, stdout: Stdio, stderr: Stdio) -> io::Result<Child> {
        let mut process = Command::new(resolve_program(&self.program, self.current_dir.as_deref()));
        process
            .args(&self.args)
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        if let Some(dir) = &self.current_dir {
            process.current_dir(dir);
        }

        #[cfg(unix)]
        {
            // Its own session, so the deadline can kill the whole group and not
            // just the program that happens to be holding the handle.
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        process.spawn()
    }
}

/// One output stream being read into a [`CapturedStream`].
///
/// Holds the finished capture once the reader task has produced it, so a drain
/// that has to be retried after a kill never polls a completed task again.
struct DrainingStream {
    task: JoinHandle<io::Result<CapturedStream>>,
    captured: Option<CapturedStream>,
}

impl DrainingStream {
    fn new(task: JoinHandle<io::Result<CapturedStream>>) -> Self {
        Self {
            task,
            captured: None,
        }
    }

    /// Whether the stream is fully read by `deadline`.
    ///
    /// A `false` return leaves the reader running: it is still holding the only
    /// copy of what arrived, and it will finish the moment the pipe closes.
    async fn drained_by(&mut self, deadline: Instant) -> io::Result<bool> {
        if self.captured.is_some() {
            return Ok(true);
        }
        match tokio::time::timeout_at(deadline, &mut self.task).await {
            Ok(joined) => {
                let captured = joined.map_err(|error| {
                    io::Error::other(format!("command output task failed: {error}"))
                })??;
                self.captured = Some(captured);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

struct Drain {
    stdout: DrainingStream,
    stderr: DrainingStream,
}

impl Drain {
    async fn drained_by(&mut self, deadline: Instant) -> io::Result<bool> {
        // Both, always: a stderr still arriving is as unfinished as a stdout,
        // and the second call is free once the first has landed.
        let stdout = self.stdout.drained_by(deadline).await?;
        let stderr = self.stderr.drained_by(deadline).await?;
        Ok(stdout && stderr)
    }

    /// The two captures. Only call after [`drained_by`](Self::drained_by)
    /// returned `true`.
    fn into_captures(self) -> (CapturedStream, CapturedStream) {
        let expect = "a drained stream has its capture";
        (
            self.stdout.captured.expect(expect),
            self.stderr.captured.expect(expect),
        )
    }
}

/// Kills the child's whole process tree and waits, briefly, for it to be reaped.
///
/// `group` is the id recorded at spawn, which is still needed when the program
/// itself has already exited and left the descendants this is here to end.
async fn kill_tree_and_reap(child: &mut Child, group: Option<u32>) -> io::Result<()> {
    kill_entire_process_tree(child, group)?;
    let _ = tokio::time::timeout(CLEANUP_GRACE, child.wait()).await;
    Ok(())
}

fn kill_entire_process_tree(child: &mut Child, group: Option<u32>) -> io::Result<()> {
    // `None` once the child has been waited on, which is exactly the case where
    // the descendants are still the problem.
    let live = child.id();

    #[cfg(unix)]
    {
        // Negative pid: the process group `setsid` gave the child, which is
        // every descendant that has not left it. The group outlives its leader
        // — an orphaned group keeps its id while any member is alive — so the
        // id recorded at spawn still names the right group after the program
        // that led it has been reaped.
        if let Some(pid) = live.or(group) {
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
        // Only while the handle is live: Windows has no orphaned-group handle
        // to aim at, and a pid the system may already have recycled is not a
        // thing to kill.
        let _ = group;
        if let Some(pid) = live {
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

    if live.is_none() {
        return Ok(());
    }

    child.start_kill()
}

fn missing_pipe(stream: &str) -> io::Error {
    io::Error::other(format!("failed to capture the child's {stream}"))
}

/// Where a relative program lives.
///
/// A path with a directory part is resolved against the working directory, so
/// `./hooks/guard.sh` means what it says regardless of where this process was
/// started — the platforms disagree about whether a relative program is
/// resolved before or after the child changes directory, and this removes the
/// question. A bare name is left alone for `PATH` to answer, which is what
/// someone writing `python3` expects.
fn resolve_program(program: &OsStr, current_dir: Option<&Path>) -> PathBuf {
    let path = Path::new(program);
    let has_directory = path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());

    match current_dir {
        Some(dir) if has_directory && path.is_relative() => dir.join(path),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 64 * 1024;

    fn seconds(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    #[cfg(unix)]
    fn exit_with_code_command() -> &'static str {
        "printf out; printf err >&2; exit 3"
    }

    #[cfg(windows)]
    fn exit_with_code_command() -> &'static str {
        "echo out& echo err 1>&2& exit 3"
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
    async fn an_exit_code_and_both_streams_survive() {
        let completion = BoundedCommand::shell(exit_with_code_command(), seconds(5), CAP)
            .current_dir(std::env::temp_dir())
            .envs(minimal_shell_env())
            .run()
            .await
            .expect("the program is supervised");

        assert_eq!(completion.code(), Some(3), "{completion:?}");
        assert_eq!(completion.stdout().to_string_lossy().trim(), "out");
        assert_eq!(completion.stderr().to_string_lossy().trim(), "err");
        assert!(!completion.timed_out());
    }

    #[test]
    fn a_relative_program_is_resolved_against_the_working_directory() {
        assert_eq!(
            resolve_program(OsStr::new("./hooks/guard.sh"), Some(Path::new("/repo"))),
            PathBuf::from("/repo/./hooks/guard.sh")
        );
        assert_eq!(
            resolve_program(OsStr::new("python3"), Some(Path::new("/repo"))),
            PathBuf::from("python3"),
            "a bare name belongs to PATH"
        );
        assert_eq!(
            resolve_program(OsStr::new("./guard.sh"), None),
            PathBuf::from("./guard.sh"),
            "with no working directory there is nothing to resolve against"
        );
    }

    // Gated to unix: these fixtures are `/bin/sh` scripts, which is the
    // cheapest way to exercise a real process tree, a held pipe and a program
    // that ignores its stdin. The code under test is portable; the fixtures are
    // not, and inventing a Windows equivalent per case would test the fixture.
    #[cfg(unix)]
    mod unix {
        use super::*;

        /// Whether `pid` is gone, waiting up to `CLEANUP_GRACE` for the kernel
        /// to finish reaping it.
        async fn wait_until_dead(pid: i32) -> bool {
            let deadline = std::time::Instant::now() + CLEANUP_GRACE;
            loop {
                // Signal 0 asks the question without sending anything.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[tokio::test]
        async fn the_child_environment_holds_only_what_the_caller_passed() {
            // `/usr/bin/env` adds nothing of its own, so every name it prints
            // was either passed here or inherited — and inheriting is the bug.
            let completion = BoundedCommand::new("/usr/bin/env", seconds(5), CAP)
                .env("MENTRA_PASSED", "yes")
                .run()
                .await
                .expect("the program is supervised");

            let stdout = completion.stdout().to_string_lossy().into_owned();
            let listed: Vec<&str> = stdout
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(name, _)| name)
                .collect();

            assert!(
                stdout.contains("MENTRA_PASSED=yes"),
                "the passed variable must arrive: {stdout}"
            );
            for (name, _) in std::env::vars() {
                assert!(
                    !listed.contains(&name.as_str()),
                    "`{name}` leaked from this process into the child: {stdout}"
                );
            }
        }

        #[tokio::test]
        async fn a_stdin_payload_reaches_the_program() {
            let completion = BoundedCommand::shell("cat", seconds(5), CAP)
                .envs(minimal_shell_env())
                .stdin("hello")
                .run()
                .await
                .expect("the program is supervised");

            assert_eq!(completion.code(), Some(0), "{completion:?}");
            assert_eq!(completion.stdout().to_string_lossy(), "hello");
        }

        #[tokio::test]
        async fn a_program_that_never_reads_stdin_still_answers() {
            // The deadlock this guards against needs a payload larger than the
            // pipe buffer; 256 KiB is comfortably past every platform's.
            let payload = "x".repeat(256 * 1024);

            let completion = BoundedCommand::shell("echo done", seconds(5), CAP)
                .envs(minimal_shell_env())
                .stdin(payload)
                .run()
                .await
                .expect("the program is supervised");

            assert_eq!(completion.code(), Some(0), "{completion:?}");
            assert_eq!(completion.stdout().to_string_lossy(), "done\n");
        }

        #[tokio::test]
        async fn stdout_is_capped_while_it_is_read() {
            // Far more output than the cap, printed as fast as the program can:
            // the bound has to hold during the read, not after it.
            let completion =
                BoundedCommand::shell("echo FIRST; seq 1 100000; echo LAST", seconds(30), 4096)
                    .envs(minimal_shell_env())
                    .run()
                    .await
                    .expect("the program is supervised");

            let stdout = completion.stdout();
            assert!(stdout.truncated(), "{completion:?}");
            assert!(stdout.len() <= 4096, "kept {} bytes", stdout.len());
            let text = stdout.to_string_lossy();
            assert!(text.starts_with("FIRST\n"), "{text}");
            assert!(text.ends_with("LAST\n"), "{text}");
            assert!(text.contains("bytes elided"), "{text}");
        }

        #[tokio::test]
        async fn stdout_and_stderr_caps_can_be_configured_independently() {
            let completion =
                BoundedCommand::shell("printf 1234567890; printf abcdefghij >&2", seconds(5), 4)
                    .envs(minimal_shell_env())
                    .max_stdout_bytes(9)
                    .max_stderr_bytes(3)
                    .run()
                    .await
                    .expect("the program is supervised");

            assert!(completion.stdout().truncated(), "{completion:?}");
            assert!(completion.stderr().truncated(), "{completion:?}");
            assert!(completion.stdout().len() <= 9, "{completion:?}");
            assert!(completion.stderr().len() <= 3, "{completion:?}");
        }

        #[tokio::test]
        async fn a_backgrounded_descendant_dies_with_the_process_group() {
            // The case a per-child kill misses: the shell outlives its budget
            // and has already started something that would outlive it too.
            let completion = BoundedCommand::shell(
                "sleep 60 & echo $!; sleep 60",
                Duration::from_millis(200),
                CAP,
            )
            .envs(minimal_shell_env())
            .run()
            .await
            .expect("the program is supervised");

            assert!(completion.timed_out(), "{completion:?}");
            let pid: i32 = completion
                .stdout()
                .to_string_lossy()
                .trim()
                .parse()
                .expect("the backgrounded pid was printed");
            assert!(
                wait_until_dead(pid).await,
                "the backgrounded descendant {pid} outlived the deadline"
            );
        }

        #[tokio::test]
        async fn a_descendant_holding_the_pipe_cannot_outlast_the_deadline() {
            // The program answers and exits at once, leaving a child holding
            // stdout open. Reading to EOF would wait for that child, so the
            // budget has to cover reading as well as waiting.
            let started = std::time::Instant::now();

            let completion =
                BoundedCommand::shell("sleep 60 & echo ready", Duration::from_millis(200), CAP)
                    .envs(minimal_shell_env())
                    .run()
                    .await
                    .expect("the program is supervised");

            assert!(completion.timed_out(), "{completion:?}");
            assert!(
                started.elapsed() < seconds(10),
                "the deadline, not the descendant, decides how long this takes: {:?}",
                started.elapsed()
            );
        }

        #[tokio::test]
        async fn a_missing_program_is_an_error_not_a_completion() {
            let error = BoundedCommand::new("/definitely/not/a/real/program", seconds(5), CAP)
                .run()
                .await
                .expect_err("cannot be started");

            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }
    }
}
