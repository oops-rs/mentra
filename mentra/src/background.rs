mod hook;
mod observer;
mod store;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use strum::Display;

use crate::agent::AgentEvent;
use crate::runtime::control::{CommandOutput, CommandRequest, RuntimeExecutor};

pub(crate) use hook::BackgroundHookSink;
pub(crate) use observer::{BackgroundObserverSink, BackgroundRegistration};
pub use store::BackgroundStore;

const OUTPUT_PREVIEW_MAX_CHARS: usize = 500;
const NOTIFICATION_PENDING: i64 = 0;
const NOTIFICATION_ACKED: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Running,
    Finished,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskSummary {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub output_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundNotification {
    pub task_id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: BackgroundTaskStatus,
    pub output_preview: String,
}

#[derive(Clone)]
pub(crate) struct BackgroundTaskManager {
    inner: Arc<BackgroundTaskManagerInner>,
}

struct BackgroundTaskManagerInner {
    store: Arc<dyn BackgroundStore>,
    executor: Arc<dyn RuntimeExecutor>,
    hooks: Arc<dyn BackgroundHookSink>,
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
    observer: Option<BackgroundObserver>,
}

#[derive(Clone)]
struct BackgroundObserver {
    sink: Arc<dyn BackgroundObserverSink>,
}

impl BackgroundTaskManager {
    pub(crate) fn new(
        store: Arc<dyn BackgroundStore>,
        executor: Arc<dyn RuntimeExecutor>,
        hooks: Arc<dyn BackgroundHookSink>,
    ) -> Self {
        Self {
            inner: Arc::new(BackgroundTaskManagerInner {
                store,
                executor,
                hooks,
                next_task_id: AtomicU64::default(),
                state: Mutex::new(BackgroundTaskManagerState::default()),
            }),
        }
    }

    pub(crate) fn register_agent(&self, registration: BackgroundRegistration) {
        let BackgroundRegistration { agent_id, observer } = registration;
        let tasks = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("background manager poisoned");
            let agent = state.agents.entry(agent_id.clone()).or_default();
            agent.tasks = self
                .inner
                .store
                .load_background_tasks(&agent_id)
                .unwrap_or_default();
            agent.observer = Some(BackgroundObserver {
                sink: observer.clone(),
            });
            agent.tasks.clone()
        };

        observer.publish_snapshot(&tasks);
    }

    pub(crate) fn start_task(
        &self,
        agent_id: &str,
        request: CommandRequest,
    ) -> Result<BackgroundTaskSummary, String> {
        let task_id = format!(
            "bg-{}",
            self.inner.next_task_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let summary = BackgroundTaskSummary {
            id: task_id.clone(),
            command: request.spec.display().to_string(),
            cwd: request.cwd.clone(),
            status: BackgroundTaskStatus::Running,
            output_preview: None,
        };
        let _ = self
            .inner
            .store
            .upsert_background_task(agent_id, &summary, NOTIFICATION_ACKED);

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
        let _ =
            self.inner
                .hooks
                .task_started(agent_id, &summary.id, &summary.command, &summary.cwd);

        let manager = self.clone();
        let agent_id = agent_id.to_string();
        let executor = self.inner.executor.clone();
        tokio::spawn(async move {
            let completed = execute_task(task_id, request, executor).await;
            manager.finish_task(&agent_id, completed);
        });

        Ok(summary)
    }

    pub(crate) fn running_task_count(&self, agent_id: &str) -> usize {
        let state = self
            .inner
            .state
            .lock()
            .expect("background manager poisoned");
        state
            .agents
            .get(agent_id)
            .map(|agent| {
                agent
                    .tasks
                    .iter()
                    .filter(|task| task.status == BackgroundTaskStatus::Running)
                    .count()
            })
            .unwrap_or(0)
    }

    pub(crate) fn drain_notifications(&self, agent_id: &str) -> Vec<BackgroundNotification> {
        self.inner
            .store
            .drain_background_notifications(agent_id)
            .unwrap_or_default()
    }

    pub(crate) fn has_pending_notifications(&self, agent_id: &str) -> bool {
        self.inner
            .store
            .has_pending_background_notifications(agent_id)
            .unwrap_or(false)
    }

    pub(crate) fn has_deliverable_notifications(&self, agent_id: &str) -> bool {
        self.inner
            .store
            .has_deliverable_background_notifications(agent_id)
            .unwrap_or(false)
    }

    pub(crate) fn requeue_notifications(
        &self,
        agent_id: &str,
        notifications: Vec<BackgroundNotification>,
    ) {
        if notifications.is_empty() {
            return;
        }
        let _ = self.inner.store.requeue_background_notifications(agent_id);
    }

    pub(crate) fn acknowledge_notifications(&self, agent_id: &str) {
        let _ = self.inner.store.ack_background_notifications(agent_id);
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
            (agent.observer.clone(), agent.tasks.clone())
        };
        let _ = self
            .inner
            .store
            .upsert_background_task(agent_id, &summary, NOTIFICATION_PENDING);
        let status = summary.status.to_string();
        let _ = self
            .inner
            .hooks
            .task_finished(agent_id, &summary.id, &status);

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

        observer.sink.publish_snapshot(&tasks);
        observer.sink.publish_event(event);
    }
}

struct CompletedBackgroundTask {
    id: String,
    command: String,
    cwd: PathBuf,
    status: BackgroundTaskStatus,
    output_preview: String,
}

async fn execute_task(
    id: String,
    request: CommandRequest,
    executor: Arc<dyn RuntimeExecutor>,
) -> CompletedBackgroundTask {
    let command = request.spec.display().to_string();
    let cwd = request.cwd.clone();
    match executor.run(request).await {
        Ok(output) => completed_task_from_output(id, command, cwd, output),
        Err(error) => CompletedBackgroundTask {
            id,
            command,
            cwd,
            status: BackgroundTaskStatus::Failed,
            output_preview: truncate_preview(&error),
        },
    }
}

fn completed_task_from_output(
    id: String,
    command: String,
    cwd: PathBuf,
    output: CommandOutput,
) -> CompletedBackgroundTask {
    let combined = format!("{} {}", output.stdout, output.stderr);
    let preview = if combined.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncate_preview(&combined)
    };
    let status = if output.success() {
        BackgroundTaskStatus::Finished
    } else {
        BackgroundTaskStatus::Failed
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
        task.status,
        task.cwd.display(),
        task.command
    )
}

fn render_task_detail(task: &BackgroundTaskSummary) -> String {
    let output = task.output_preview.as_deref().unwrap_or("(running)");
    format!(
        "[{}] cwd={}\n{}\n{}",
        task.status,
        task.cwd.display(),
        task.command,
        output
    )
}
