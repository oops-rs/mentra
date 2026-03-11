use std::{collections::VecDeque, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::{
    provider::{
        ModelInfo, ModelProviderKind, Provider, ProviderError, ProviderEvent, ProviderEventStream,
        Request,
    },
    tool::{ToolContext, ToolHandler, ToolResult, ToolSpec},
};

pub(super) enum StreamScript {
    Buffered(Vec<Result<ProviderEvent, ProviderError>>),
    Receiver(ProviderEventStream),
}

#[derive(Clone)]
pub(super) struct ScriptedProvider {
    kind: ModelProviderKind,
    models: Vec<ModelInfo>,
    scripts: Arc<Mutex<VecDeque<StreamScript>>>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
}

impl ScriptedProvider {
    pub(super) fn new(
        kind: ModelProviderKind,
        models: Vec<ModelInfo>,
        scripts: Vec<StreamScript>,
    ) -> Self {
        Self {
            kind,
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
    fn kind(&self) -> ModelProviderKind {
        self.kind
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

pub(super) fn model_info(id: &str, provider: ModelProviderKind) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        provider,
        display_name: None,
        description: None,
        created_at: None,
    }
}

pub(super) fn ok_stream(events: Vec<ProviderEvent>) -> StreamScript {
    StreamScript::Buffered(events.into_iter().map(Ok).collect())
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
}

impl StaticTool {
    pub(super) fn success(name: &'static str, output: &str) -> Self {
        Self {
            name,
            result: Ok(output.to_string()),
        }
    }

    pub(super) fn failure(name: &'static str, error: &str) -> Self {
        Self {
            name,
            result: Err(error.to_string()),
        }
    }
}

#[async_trait]
impl ToolHandler for StaticTool {
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
        }
    }

    async fn invoke(&self, _ctx: ToolContext, _input: Value) -> ToolResult {
        self.result.clone()
    }
}
