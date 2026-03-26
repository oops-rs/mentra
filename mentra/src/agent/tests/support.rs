use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc},
    time::sleep,
};

use crate::{
    provider::{
        ModelInfo, Provider, ProviderDescriptor, ProviderError, ProviderEvent, ProviderEventStream,
        ProviderId, Request,
    },
    tool::{
        ExecutableTool, ParallelToolContext, ToolContext, ToolExecutionMode, ToolResult, ToolSpec,
    },
};

pub(super) enum StreamScript {
    Buffered(Vec<Result<ProviderEvent, ProviderError>>),
    Receiver(ProviderEventStream),
}

#[derive(Clone)]
pub(super) struct ScriptedProvider {
    kind: ProviderId,
    models: Vec<ModelInfo>,
    scripts: Arc<Mutex<VecDeque<StreamScript>>>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
}

impl ScriptedProvider {
    pub(super) fn new(
        kind: impl Into<ProviderId>,
        models: Vec<ModelInfo>,
        scripts: Vec<StreamScript>,
    ) -> Self {
        Self {
            kind: kind.into(),
            models,
            scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            requests: Arc::new(Mutex::new(Vec::<Request<'static>>::new())),
        }
    }

    pub(super) async fn recorded_requests(&self) -> Vec<Request<'static>> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.kind.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.requests.lock().await.push(request.into_owned());
        match self.scripts.lock().await.pop_front() {
            Some(StreamScript::Buffered(items)) => {
                let (tx, rx) = mpsc::unbounded_channel();
                for item in items {
                    tx.send(item)
                        .expect("test stream receiver dropped unexpectedly");
                }
                Ok(rx)
            }
            Some(StreamScript::Receiver(receiver)) => Ok(receiver),
            None => panic!("no scripted stream available"),
        }
    }
}

pub(super) fn model_info(id: &str, provider: impl Into<ProviderId>) -> ModelInfo {
    ModelInfo::new(id, provider)
}

pub(super) fn ok_stream(events: Vec<ProviderEvent>) -> StreamScript {
    StreamScript::Buffered(events.into_iter().map(Ok).collect())
}

pub(super) fn command_input_json(command: &str) -> String {
    json!({ "command": command }).to_string()
}

pub(super) fn command_input_with_working_directory_json(
    command: &str,
    working_directory: &str,
) -> String {
    json!({
        "command": command,
        "workingDirectory": working_directory,
    })
    .to_string()
}

pub(super) fn shell_pwd_command() -> String {
    #[cfg(unix)]
    {
        "pwd".to_string()
    }

    #[cfg(windows)]
    {
        "cd".to_string()
    }
}

pub(super) fn background_success_command(output: &str, delay_ms: u64) -> String {
    #[cfg(unix)]
    {
        format!(
            "sleep {}; printf {}",
            delay_seconds(delay_ms),
            shell_single_quoted(output)
        )
    }

    #[cfg(windows)]
    {
        format!(
            "powershell.exe -NoProfile -Command \"Start-Sleep -Milliseconds {delay_ms}; [Console]::Out.Write('{}')\"",
            powershell_single_quoted(output)
        )
    }
}

pub(super) fn background_failure_command(stderr: &str, exit_code: i32, delay_ms: u64) -> String {
    #[cfg(unix)]
    {
        format!(
            "sleep {}; printf {} >&2; exit {exit_code}",
            delay_seconds(delay_ms),
            shell_single_quoted(stderr)
        )
    }

    #[cfg(windows)]
    {
        format!(
            "powershell.exe -NoProfile -Command \"Start-Sleep -Milliseconds {delay_ms}; [Console]::Error.Write('{}'); exit {exit_code}\"",
            powershell_single_quoted(stderr)
        )
    }
}

#[cfg(unix)]
fn delay_seconds(delay_ms: u64) -> String {
    format!("{:.3}", delay_ms as f64 / 1000.0)
}

#[cfg(unix)]
fn shell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(windows)]
fn powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

pub(super) fn erroring_stream(events: Vec<ProviderEvent>, error: ProviderError) -> StreamScript {
    let mut items = events.into_iter().map(Ok).collect::<Vec<_>>();
    items.push(Err(error));
    StreamScript::Buffered(items)
}

pub(super) fn controlled_stream() -> (
    StreamScript,
    mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (StreamScript::Receiver(rx), tx)
}

pub(super) struct StaticTool {
    name: &'static str,
    result: ToolResult,
    loading_policy: crate::tool::ToolLoadingPolicy,
}

impl StaticTool {
    pub(super) fn success(name: &'static str, output: &str) -> Self {
        Self {
            name,
            result: Ok(output.to_string()),
            loading_policy: crate::tool::ToolLoadingPolicy::Immediate,
        }
    }

    pub(super) fn failure(name: &'static str, error: &str) -> Self {
        Self {
            name,
            result: Err(error.to_string()),
            loading_policy: crate::tool::ToolLoadingPolicy::Immediate,
        }
    }

    pub(super) fn deferred_success(name: &'static str, output: &str) -> Self {
        Self {
            name,
            result: Ok(output.to_string()),
            loading_policy: crate::tool::ToolLoadingPolicy::Deferred,
        }
    }
}

#[async_trait]
impl ExecutableTool for StaticTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_string(),
            description: Some("test tool".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }),
            capabilities: vec![],
            side_effect_level: crate::tool::ToolSideEffectLevel::None,
            durability: crate::tool::ToolDurability::ReplaySafe,
            loading_policy: self.loading_policy,
            execution_timeout: None,
        }
    }

    async fn execute_mut(&self, _ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        self.result.clone()
    }
}

#[derive(Clone)]
pub(super) struct ProbeTool {
    name: &'static str,
    parallel: bool,
    delay: Duration,
    log: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl ProbeTool {
    pub(super) fn new(
        name: &'static str,
        parallel: bool,
        delay: Duration,
        log: Arc<Mutex<Vec<String>>>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name,
            parallel,
            delay,
            log,
            active,
            max_active,
        }
    }

    async fn run(&self) -> ToolResult {
        self.log.lock().await.push(format!("{}:start", self.name));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self
            .max_active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (active > current).then_some(active)
            });
        sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.log.lock().await.push(format!("{}:end", self.name));
        Ok(format!("{} complete", self.name))
    }
}

#[async_trait]
impl ExecutableTool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_string(),
            description: Some("probe tool".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            capabilities: vec![],
            side_effect_level: crate::tool::ToolSideEffectLevel::None,
            durability: crate::tool::ToolDurability::ReplaySafe,
            loading_policy: crate::tool::ToolLoadingPolicy::Immediate,
            execution_timeout: None,
        }
    }

    fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
        if self.parallel {
            ToolExecutionMode::Parallel
        } else {
            ToolExecutionMode::Exclusive
        }
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.run().await
    }
}
