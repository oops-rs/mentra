use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{broadcast, watch},
};

use crate::runtime::{AgentEvent, AgentSnapshot, handle::AgentObserver};

const OUTPUT_PREVIEW_MAX_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    Finished,
    Failed,
}

impl BackgroundTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTaskSummary {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub output_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundNotification {
    pub task_id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub output_preview: String,
}

#[derive(Clone, Default)]
pub(crate) struct BackgroundTaskManager {
    inner: Arc<BackgroundTaskManagerInner>,
}

#[derive(Default)]
struct BackgroundTaskManagerInner {
    next_task_id: AtomicU64,
    state: Mutex<BackgroundTaskManagerState>,
}

#[derive(Default)]
struct BackgroundTaskManagerState {
    agents: HashMap<String, AgentBackgroundState>,
}

#[derive(Default)]
struct AgentBackgroundState {
    tasks: Vec<BackgroundTaskSummary>,
    notifications: VecDeque<BackgroundNotification>,
    observer: Option<BackgroundObserver>,
}

#[derive(Clone)]
struct BackgroundObserver {
    events: broadcast::Sender<AgentEvent>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
}

impl BackgroundTaskManager {
    pub(crate) fn register_agent(
        &self,
        agent_id: &str,
        observer: &AgentObserver,
    ) {
        let tasks = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("background manager poisoned");
            let agent = state.agents.entry(agent_id.to_string()).or_default();
            agent.observer = Some(BackgroundObserver {
                events: observer.events.clone(),
                snapshot_tx: observer.snapshot_tx.clone(),
                snapshot: Arc::clone(&observer.snapshot),
            });
            agent.tasks.clone()
        };

        Self::publish_snapshot(Arc::clone(&observer.snapshot), &tasks);
        let snapshot = observer
            .snapshot
            .lock()
            .expect("agent snapshot poisoned")
            .clone();
        observer.snapshot_tx.send_replace(snapshot);
    }

    pub(crate) fn start_task(
        &self,
        agent_id: &str,
        command: String,
        cwd: PathBuf,
    ) -> BackgroundTaskSummary {
        let task_id = format!(
            "bg-{}",
            self.inner.next_task_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let summary = BackgroundTaskSummary {
            id: task_id.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
            status: BackgroundTaskStatus::Running,
            output_preview: None,
        };

        let (observer, tasks) = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("background manager poisoned");
            let agent = state.agents.entry(agent_id.to_string()).or_default();
            agent.tasks.push(summary.clone());
            (agent.observer.clone(), agent.tasks.clone())
        };
        self.publish_observer(
            observer,
            tasks,
            AgentEvent::BackgroundTaskStarted {
                task: summary.clone(),
            },
        );

        let manager = self.clone();
        let agent_id = agent_id.to_string();
        tokio::spawn(async move {
            let completed = execute_bash_task(task_id, command, cwd).await;
            manager.finish_task(&agent_id, completed);
        });

        summary
    }

    pub(crate) fn drain_notifications(&self, agent_id: &str) -> Vec<BackgroundNotification> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("background manager poisoned");
        let Some(agent) = state.agents.get_mut(agent_id) else {
            return Vec::new();
        };

        agent.notifications.drain(..).collect()
    }

    pub(crate) fn requeue_notifications(
        &self,
        agent_id: &str,
        notifications: Vec<BackgroundNotification>,
    ) {
        if notifications.is_empty() {
            return;
        }

        let mut state = self
            .inner
            .state
            .lock()
            .expect("background manager poisoned");
        let agent = state.agents.entry(agent_id.to_string()).or_default();
        for notification in notifications.into_iter().rev() {
            agent.notifications.push_front(notification);
        }
    }

    pub(crate) fn check_task(
        &self,
        agent_id: &str,
        task_id: Option<&str>,
    ) -> Result<String, String> {
        let state = self
            .inner
            .state
            .lock()
            .expect("background manager poisoned");
        let Some(agent) = state.agents.get(agent_id) else {
            return Ok("No background tasks.".to_string());
        };

        if let Some(task_id) = task_id {
            let task = agent
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("Unknown background task {task_id}"))?;
            return Ok(render_task_detail(task));
        }

        if agent.tasks.is_empty() {
            return Ok("No background tasks.".to_string());
        }

        Ok(agent
            .tasks
            .iter()
            .map(render_task_summary)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn finish_task(&self, agent_id: &str, completed: CompletedBackgroundTask) {
        let summary = BackgroundTaskSummary {
            id: completed.id.clone(),
            command: completed.command.clone(),
            cwd: completed.cwd.clone(),
            status: completed.status.clone(),
            output_preview: Some(completed.output_preview.clone()),
        };
        let notification = BackgroundNotification {
            task_id: completed.id,
            command: completed.command,
            cwd: completed.cwd,
            status: completed.status,
            output_preview: completed.output_preview,
        };

        let (observer, tasks) = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("background manager poisoned");
            let agent = state.agents.entry(agent_id.to_string()).or_default();
            if let Some(existing) = agent.tasks.iter_mut().find(|task| task.id == summary.id) {
                *existing = summary.clone();
            } else {
                agent.tasks.push(summary.clone());
            }
            agent.notifications.push_back(notification);
            (agent.observer.clone(), agent.tasks.clone())
        };

        self.publish_observer(
            observer,
            tasks,
            AgentEvent::BackgroundTaskFinished { task: summary },
        );
    }

    fn publish_observer(
        &self,
        observer: Option<BackgroundObserver>,
        tasks: Vec<BackgroundTaskSummary>,
        event: AgentEvent,
    ) {
        let Some(observer) = observer else {
            return;
        };

        Self::publish_snapshot(Arc::clone(&observer.snapshot), &tasks);
        let snapshot = observer
            .snapshot
            .lock()
            .expect("agent snapshot poisoned")
            .clone();
        observer.snapshot_tx.send_replace(snapshot);
        let _ = observer.events.send(event);
    }

    fn publish_snapshot(snapshot: Arc<Mutex<AgentSnapshot>>, tasks: &[BackgroundTaskSummary]) {
        let mut guard = snapshot.lock().expect("agent snapshot poisoned");
        guard.background_tasks = tasks.to_vec();
    }
}

struct CompletedBackgroundTask {
    id: String,
    command: String,
    cwd: PathBuf,
    status: BackgroundTaskStatus,
    output_preview: String,
}

async fn execute_bash_task(id: String, command: String, cwd: PathBuf) -> CompletedBackgroundTask {
    let mut process = match Command::new("bash")
        .arg("-c")
        .arg(&command)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return CompletedBackgroundTask {
                id,
                command,
                cwd,
                status: BackgroundTaskStatus::Failed,
                output_preview: truncate_preview(&format!("Failed to spawn command: {error}")),
            };
        }
    };

    let mut stdout = process.stdout.take();
    let mut stderr = process.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut stdout) = stdout.take() {
            let _ = stdout.read_to_end(&mut bytes).await;
        }
        bytes
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr.take() {
            let _ = stderr.read_to_end(&mut bytes).await;
        }
        bytes
    });

    let status = process.wait().await;
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    let combined = [stdout, stderr].into_iter().flatten().collect::<Vec<_>>();
    let preview = if combined.is_empty() {
        "(no output)".to_string()
    } else {
        truncate_preview(&String::from_utf8_lossy(&combined))
    };

    let status = match status {
        Ok(exit) if exit.success() => BackgroundTaskStatus::Finished,
        Ok(_) => BackgroundTaskStatus::Failed,
        Err(error) => {
            return CompletedBackgroundTask {
                id,
                command,
                cwd,
                status: BackgroundTaskStatus::Failed,
                output_preview: truncate_preview(&format!("Failed to wait for command: {error}")),
            };
        }
    };

    CompletedBackgroundTask {
        id,
        command,
        cwd,
        status,
        output_preview: preview,
    }
}

fn truncate_preview(text: &str) -> String {
    let mut compact = String::new();
    for (index, chunk) in text.split_whitespace().enumerate() {
        if index > 0 {
            compact.push(' ');
        }
        compact.push_str(chunk);
    }

    let mut truncated = compact
        .chars()
        .take(OUTPUT_PREVIEW_MAX_CHARS)
        .collect::<String>();
    if compact.chars().count() > OUTPUT_PREVIEW_MAX_CHARS {
        truncated.push_str("...");
    }
    truncated
}

fn render_task_summary(task: &BackgroundTaskSummary) -> String {
    format!(
        "{}: [{}] cwd={} {}",
        task.id,
        task.status.as_str(),
        task.cwd.display(),
        task.command
    )
}

fn render_task_detail(task: &BackgroundTaskSummary) -> String {
    let output = task.output_preview.as_deref().unwrap_or("(running)");
    format!(
        "[{}] cwd={}\n{}\n{}",
        task.status.as_str(),
        task.cwd.display(),
        task.command,
        output
    )
}
