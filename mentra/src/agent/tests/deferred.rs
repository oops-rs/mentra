//! `Deferred`: what a tool leaves for its agent after the call has returned.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    BuiltinProvider, ContentBlock, Message, Role, Runtime,
    agent::{AgentEvent, Deferred, DeferredItem, SpawnedAgentStatus},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent},
    runtime::RunOptions,
    tool::{ToolContext, ToolDefinition, ToolExecutor, ToolResult, ToolSpec},
    transcript::{DelegationArtifact, DelegationEdge, DelegationKind, DelegationStatus},
};

use super::support::{
    ScriptedProvider, controlled_stream, model_info, text_stream, tool_use_stream,
};

/// A tool that returns at once and hands its [`Deferred`] to the test, which
/// decides when the background answer lands.
struct Starter {
    handle: Arc<Mutex<Option<Deferred>>>,
    /// The subagent the tool registers, when the test asks for one.
    child: Arc<Mutex<Option<String>>>,
    registers_a_child: bool,
}

impl ToolDefinition for Starter {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("start")
            .description("starts work and returns")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(crate::tool::ToolSideEffectLevel::None)
            .durability(crate::tool::ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for Starter {
    async fn execute_mut(&self, mut ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        if self.registers_a_child {
            let child = ctx.spawn_subagent().map_err(|error| error.to_string())?;
            ctx.register_subagent(&child);
            *self.child.lock().expect("child slot") = Some(child.id().to_string());
        }
        *self.handle.lock().expect("handle slot") = Some(ctx.deferred());
        Ok("started".to_string())
    }
}

fn request_contains(messages: &[Message], needle: &str) -> bool {
    messages
        .iter()
        .any(|message| message.text().contains(needle))
}

async fn wait_for_request_count(provider: &ScriptedProvider, expected: usize) {
    loop {
        if provider.recorded_requests().await.len() >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn send_text_response(
    tx: &mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    model: &str,
    text: &str,
) {
    let events = [
        ProviderEvent::MessageStarted {
            id: format!("msg-{text}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::Text,
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text(text.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ];
    for event in events {
        tx.send(Ok(event)).expect("stream receiver remains alive");
    }
}

/// An item that lands while the model is answering reaches it before the
/// next request, and keeps the run going past a message that would have
/// ended it.
#[tokio::test]
async fn an_item_landing_after_the_last_message_keeps_the_run_going() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (second_stream, second_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "start", "{}"),
            second_stream,
            text_stream(&model.id, "final"),
        ],
    );
    let provider_handle = provider.clone();
    let handle = Arc::new(Mutex::new(None));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(Starter {
            handle: Arc::clone(&handle),
            child: Arc::new(Mutex::new(None)),
            registers_a_child: false,
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");

    let drive = async {
        // The second request is open: the tool has returned, the model is
        // composing what would be its last message, and only now does the
        // background answer land.
        wait_for_request_count(&provider_handle, 2).await;
        let deferred = handle
            .lock()
            .expect("handle slot")
            .clone()
            .expect("the tool took a handle");
        deferred.push(DeferredItem::Context(
            "the background answer: forty-two".to_string(),
        ));
        send_text_response(&second_tx, &model.id, "done, I think");
        drop(second_tx);
    };
    let (result, ()) = tokio::join!(
        agent.run(vec![ContentBlock::text("start")], RunOptions::default()),
        drive
    );

    assert_eq!(
        result.expect("run succeeds").text(),
        "final",
        "the run went one more round so the model could read the answer"
    );
    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(
        !request_contains(&requests[1].messages, "forty-two"),
        "nothing had landed when the second request was built"
    );
    assert!(
        request_contains(&requests[2].messages, "forty-two"),
        "the third request carries it"
    );
}

/// A delegation's result and its subagent's end are recorded on the parent
/// as the `task` intrinsic would have recorded them from inside the call.
#[tokio::test]
async fn a_delegation_result_settles_the_child_and_reaches_the_transcript() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (second_stream, second_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "start", "{}"),
            second_stream,
            text_stream(&model.id, "final"),
        ],
    );
    let provider_handle = provider.clone();
    let handle = Arc::new(Mutex::new(None));
    let child = Arc::new(Mutex::new(None));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(Starter {
            handle: Arc::clone(&handle),
            child: Arc::clone(&child),
            registers_a_child: true,
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model.clone()).expect("spawn agent");
    let mut events = agent.subscribe_events();
    let snapshot = agent.watch_snapshot();

    let drive = async {
        wait_for_request_count(&provider_handle, 2).await;
        let deferred = handle
            .lock()
            .expect("handle slot")
            .clone()
            .expect("the tool took a handle");
        let child_id = child
            .lock()
            .expect("child slot")
            .clone()
            .expect("the tool registered a child");
        deferred.push(DeferredItem::DelegationResult {
            content: "<delegation-result>the child's answer</delegation-result>".to_string(),
            delegation: DelegationArtifact {
                kind: DelegationKind::Subagent,
                agent_id: child_id.clone(),
                agent_name: "child".to_string(),
                role: Some("subagent".to_string()),
                status: DelegationStatus::Finished,
                task_summary: "the task".to_string(),
                result_summary: Some("the child's answer".to_string()),
                artifacts: Vec::new(),
            },
            edge: Some(DelegationEdge {
                kind: DelegationKind::Subagent,
                local_agent_id: "agent".to_string(),
                remote_agent_id: child_id.clone(),
            }),
        });
        deferred.push(DeferredItem::SubagentFinished {
            id: child_id,
            status: SpawnedAgentStatus::Finished,
        });
        send_text_response(&second_tx, &model.id, "done, I think");
        drop(second_tx);
    };
    let (result, ()) = tokio::join!(
        agent.run(vec![ContentBlock::text("start")], RunOptions::default()),
        drive
    );
    assert_eq!(result.expect("run succeeds").text(), "final");

    let requests = provider_handle.recorded_requests().await;
    assert_eq!(requests.len(), 3);
    assert!(request_contains(
        &requests[2].messages,
        "the child's answer"
    ));
    let subagents = &snapshot.borrow().subagents;
    assert_eq!(subagents.len(), 1);
    assert_eq!(
        subagents[0].status,
        SpawnedAgentStatus::Finished,
        "the parent's snapshot settled the child"
    );
    let mut finished = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, AgentEvent::SubagentFinished { .. }) {
            finished = true;
        }
    }
    assert!(finished, "and announced it on the parent's stream");
}
