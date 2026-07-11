//! Tests for the M2 structured `ToolOutput` seam (ADR-0001 §3): the bridge
//! from `ToolResult`, structured content + opaque `details`, and the two
//! layers of termination exclusivity.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    sync::Mutex as TokioMutex,
    time::{Duration, sleep},
};

use crate::{
    BuiltinProvider, ContentBlock, Role,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{RunOptions, Runtime, RuntimeError, RuntimeHookEvent},
    tool::{
        ParallelToolContext, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
        ToolExecutor, ToolOutput, ToolResult, ToolResultContent, ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{ScriptedProvider, StaticTool, StreamScript, model_info, ok_stream};

/// A tool that only implements the new structured, exclusive-lane entry
/// point directly (no `execute_mut` override) — proves a tool need not touch
/// the old `String` surface at all to opt into structured content, details,
/// or termination.
struct StructuredDetailsTool;

#[async_trait]
impl ToolDefinition for StructuredDetailsTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("structured_details_tool")
            .description("test tool: returns structured content plus opaque details")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for StructuredDetailsTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(
            ToolOutput::structured(json!({ "answer": 42 }))
                .with_details(json!({ "secret": "shh" })),
        )
    }
}

/// A tool that ends the run as the value of its own execution.
struct TerminatingTool;

#[async_trait]
impl ToolDefinition for TerminatingTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("terminating_tool")
            .description("test tool: ends the run via ToolOutput::terminate")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for TerminatingTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text("final answer").terminating())
    }
}

/// A tool that overrides the new structured surface directly and returns a
/// tool-level failure — proves `Err(String)` behaves identically on the new
/// entry point as it does on the old one.
struct FailingOutputTool;

#[async_trait]
impl ToolDefinition for FailingOutputTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("failing_output_tool")
            .description("test tool: fails via the new structured surface")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for FailingOutputTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Err("boom".to_string())
    }
}

/// A timing probe (start/end log, like the shared `ProbeTool`) that declares
/// `ReadOnlyParallel` on its descriptor but is also marked `.terminal()` —
/// used to prove the scheduler coerces a terminal-marked tool to exclusive
/// scheduling regardless of its declared category.
struct TerminalParallelProbe {
    name: &'static str,
    log: Arc<TokioMutex<Vec<String>>>,
}

#[async_trait]
impl ToolDefinition for TerminalParallelProbe {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name)
            .description("test tool: declares ReadOnlyParallel but is terminal")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .terminal()
            .build()
    }
}

#[async_trait]
impl ToolExecutor for TerminalParallelProbe {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        self.log.lock().await.push(format!("{}:start", self.name));
        sleep(Duration::from_millis(15)).await;
        self.log.lock().await.push(format!("{}:end", self.name));
        Ok(format!("{} complete", self.name))
    }
}

/// A genuinely parallel-eligible tool (not `.terminal()`) that nonetheless
/// tries to request termination from the parallel lane — the RUNTIME defense
/// scenario: misuse independent of the static descriptor marker.
struct MisbehavingParallelTerminateTool;

#[async_trait]
impl ToolDefinition for MisbehavingParallelTerminateTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("misbehaving_parallel_terminate")
            .description("test tool: wrongly requests termination from a parallel execution")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for MisbehavingParallelTerminateTool {
    async fn execute_output(
        &self,
        _ctx: ParallelToolContext,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text("i should not be able to stop the run").terminating())
    }
}

fn tool_use_stream(model: &str, id: &str, name: &str, input_json: &str) -> StreamScript {
    ok_stream(vec![
        ProviderEvent::MessageStarted {
            id: format!("msg-{id}"),
            model: model.to_string(),
            role: Role::Assistant,
        },
        ProviderEvent::ContentBlockStarted {
            index: 0,
            kind: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
            },
        },
        ProviderEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolUseInputJson(input_json.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ])
}

fn multi_tool_use_stream(model: &str, calls: &[(&str, &str, &str)]) -> StreamScript {
    let mut events = vec![ProviderEvent::MessageStarted {
        id: "msg-multi-tool".to_string(),
        model: model.to_string(),
        role: Role::Assistant,
    }];

    for (index, (id, name, input_json)) in calls.iter().enumerate() {
        events.push(ProviderEvent::ContentBlockStarted {
            index,
            kind: ContentBlockStart::ToolUse {
                id: (*id).to_string(),
                name: (*name).to_string(),
            },
        });
        events.push(ProviderEvent::ContentBlockDelta {
            index,
            delta: ContentBlockDelta::ToolUseInputJson((*input_json).to_string()),
        });
        events.push(ProviderEvent::ContentBlockStopped { index });
    }

    events.push(ProviderEvent::MessageStopped);
    ok_stream(events)
}

fn text_stream(model: &str, text: &str) -> StreamScript {
    ok_stream(vec![
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
    ])
}

fn tool_result_blocks(messages: &[crate::Message]) -> Vec<ContentBlock> {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.content.iter().cloned())
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .collect()
}

#[derive(Clone, Default)]
struct RecordingHook {
    events: Arc<std::sync::Mutex<Vec<RuntimeHookEvent>>>,
}

impl crate::runtime::control::RuntimeHook for RecordingHook {
    fn on_event(
        &self,
        _store: &dyn crate::runtime::AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        self.events
            .lock()
            .expect("hook events poisoned")
            .push(event.clone());
        Ok(())
    }
}

// 1. A string tool compiles unchanged and produces Text through the bridge,
// byte-identical transcript output vs today.
#[tokio::test]
async fn string_tool_bridges_to_text_output_unchanged() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "echo_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("echo_tool", "echoed"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the echo tool")])
        .await
        .expect("send");

    let blocks = tool_result_blocks(agent.history());
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0],
        ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: ToolResultContent::Text("echoed".to_string()),
            is_error: false,
        }
    );
}

// 7. Err(String) from the new surface behaves exactly like today (is_error
// block, model sees it, run continues) — exercised both through the bridge
// (StaticTool::failure, unchanged `execute_mut`) and directly on the new
// `execute_mut_output` surface.
#[tokio::test]
async fn err_string_behaves_identically_through_bridge_and_new_surface() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-1", "bridged_failure", r#"{}"#),
                    ("call-2", "failing_output_tool", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::failure("bridged_failure", "old bridge error"))
        .with_tool(FailingOutputTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let message = agent
        .send(vec![ContentBlock::text("run the failing tools")])
        .await
        .expect("send should still succeed: tool errors don't fail the run");

    assert_eq!(message.text(), "done");
    let blocks = tool_result_blocks(agent.history());
    assert_eq!(
        blocks[0],
        ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: ToolResultContent::Text("old bridge error".to_string()),
            is_error: true,
        }
    );
    assert_eq!(
        blocks[1],
        ContentBlock::ToolResult {
            tool_use_id: "call-2".to_string(),
            content: ToolResultContent::Text("boom".to_string()),
            is_error: true,
        }
    );
}

// 2. A structured tool returns Structured content plus opaque details; the
// recorded provider request contains only the content projection, no
// details bytes.
#[tokio::test]
async fn structured_tool_projects_content_and_hides_details_from_provider() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "structured_details_tool", r#"{}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let hook = RecordingHook::default();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider.clone())
        .with_tool(StructuredDetailsTool)
        .with_hook(hook.clone())
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the structured tool")])
        .await
        .expect("send");

    let blocks = tool_result_blocks(agent.history());
    assert_eq!(
        blocks[0],
        ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: ToolResultContent::Structured(json!({ "answer": 42 })),
            is_error: false,
        }
    );

    // The follow-up request (the one carrying the tool result back to the
    // model) must contain the projected content and nothing from `details`.
    let requests = provider.recorded_requests().await;
    assert_eq!(requests.len(), 2, "tool round, then the follow-up round");
    let follow_up = requests[1]
        .messages
        .iter()
        .map(|message| format!("{message:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(follow_up.contains("answer"));
    assert!(!follow_up.contains("secret"));
    assert!(!follow_up.contains("shh"));

    // `details` is still observable at the execution-outcome boundary via
    // the runtime hook, for a host (or a later slice) to recover.
    let events = hook.events.lock().expect("hook events poisoned").clone();
    let details = events.iter().find_map(|event| match event {
        RuntimeHookEvent::ToolExecutionFinished {
            tool_name, details, ..
        } if tool_name == "structured_details_tool" => Some(details.clone()),
        _ => None,
    });
    assert_eq!(details, Some(Some(json!({ "secret": "shh" }))));
}

// 3. A terminate:true tool ends the run successfully with the transcript
// committed. Mirrors `run_options_stop_after_tool_round_commits_transcript_and_halts`:
// the outer `Agent::run`/`send` returns `Err(EmptyAssistantResponse)` because
// the last committed message is the tool result (not assistant text) — this
// is the established, documented "honest stop, not a failure" contract for
// any tool-driven round ending (see that test and `team/actor.rs`'s
// `Ok(_) | Err(EmptyAssistantResponse) => Ok(())` handling); the run itself
// is NOT rolled back.
#[tokio::test]
async fn terminating_tool_commits_transcript_without_rollback() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "terminating_tool", r#"{}"#),
            // Must NOT be consumed: termination halts the run before this
            // round's model request is issued.
            text_stream(&model.id, "must not run"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(TerminatingTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let result = agent
        .run(
            vec![ContentBlock::text("go")],
            RunOptions {
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));
    assert_eq!(
        agent.history().len(),
        3,
        "user, the assistant tool call, and the committed tool result"
    );
    let blocks = tool_result_blocks(agent.history());
    assert_eq!(
        blocks[0],
        ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: ToolResultContent::Text("final answer".to_string()),
            is_error: false,
        }
    );
}

// 4. A terminating call is never scheduled concurrently with retrieval
// (barrier test mirroring `parallel_batches_respect_exclusive_barriers`),
// and 5. calls scheduled after it in the same round get explicit
// not-executed results, in call order — exercised together since the
// skipped batch here is itself a parallel batch.
#[tokio::test]
async fn terminating_call_creates_a_barrier_and_skips_the_scheduled_parallel_batch() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![multi_tool_use_stream(
            &model.id,
            &[
                ("call-1", "probe_one", r#"{}"#),
                ("call-2", "probe_two", r#"{}"#),
                ("call-3", "terminating_tool", r#"{}"#),
                ("call-4", "probe_three", r#"{}"#),
            ],
        )],
    );
    let log = Arc::new(TokioMutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(super::support::ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(super::support::ProbeTool::new(
            "probe_two",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(TerminatingTool)
        .with_tool(super::support::ProbeTool::new(
            "probe_three",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let result = agent
        .run(vec![ContentBlock::text("go")], RunOptions::default())
        .await;
    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));

    // probe_three's batch was scheduled after the terminating call and must
    // never have run.
    let log = log.lock().await.clone();
    assert!(!log.contains(&"probe_three:start".to_string()));
    assert!(!log.contains(&"probe_three:end".to_string()));
    assert!(
        log.contains(&"probe_one:start".to_string())
            && log.contains(&"probe_two:start".to_string()),
        "the earlier parallel batch still ran before the barrier"
    );
    assert!(max_active.load(Ordering::SeqCst) >= 2);

    // call-4 (probe_three) gets an explicit not-executed error result, after
    // call-3's real terminate result, in call order.
    let blocks = tool_result_blocks(agent.history());
    assert_eq!(blocks.len(), 4);
    assert_eq!(
        blocks[2],
        ContentBlock::ToolResult {
            tool_use_id: "call-3".to_string(),
            content: ToolResultContent::Text("final answer".to_string()),
            is_error: false,
        }
    );
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = &blocks[3]
    else {
        panic!("expected a tool result block");
    };
    assert_eq!(tool_use_id, "call-4");
    assert!(*is_error);
    let text = content.to_display_string();
    assert!(text.contains("not executed"));
    assert!(text.contains("terminating_tool"));
}

// 6a. A terminal-marked tool declared with a parallel category is coerced
// to exclusive scheduling (STATIC layer).
#[tokio::test]
async fn terminal_marked_tool_declared_parallel_is_coerced_to_exclusive() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-1", "probe_one", r#"{}"#),
                    ("call-2", "probe_two", r#"{}"#),
                    ("call-3", "terminal_parallel_probe", r#"{}"#),
                    ("call-4", "probe_three", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let log = Arc::new(TokioMutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(super::support::ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(super::support::ProbeTool::new(
            "probe_two",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .with_tool(TerminalParallelProbe {
            name: "terminal_parallel_probe",
            log: Arc::clone(&log),
        })
        .with_tool(super::support::ProbeTool::new(
            "probe_three",
            true,
            Duration::from_millis(30),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text(
            "run probes with a terminal barrier",
        )])
        .await
        .expect("send");

    let log = log.lock().await.clone();
    let position = |entry: &str| {
        log.iter()
            .position(|logged| logged == entry)
            .unwrap_or_else(|| panic!("missing log entry: {entry}"))
    };
    let terminal_start = position("terminal_parallel_probe:start");
    let terminal_end = position("terminal_parallel_probe:end");
    let probe_one_end = position("probe_one:end");
    let probe_two_end = position("probe_two:end");
    let probe_three_start = position("probe_three:start");

    // Deliberately compares END markers, not just start order: if the
    // terminal marker were ignored, all four probes would share a single
    // parallel batch (all declare ReadOnlyParallel) and run concurrently —
    // the terminal probe's shorter 15ms delay would then very likely make it
    // finish (`terminal_end`) *before* the 30ms probes even start their own
    // end, so `probe_one_end < terminal_start` would fail. Only genuine
    // exclusive-batch sequencing (this batch fully awaited before the next
    // begins) guarantees the terminal probe starts after the earlier batch
    // is entirely done, and the later batch starts after the terminal probe
    // is entirely done.
    assert!(
        probe_one_end < terminal_start,
        "terminal probe must not start until probe_one has fully finished"
    );
    assert!(
        probe_two_end < terminal_start,
        "terminal probe must not start until probe_two has fully finished"
    );
    assert!(
        terminal_end < probe_three_start,
        "probe_three must not start until the terminal probe has fully finished"
    );
    assert!(
        max_active.load(Ordering::SeqCst) >= 2,
        "the surrounding probes still ran in parallel with each other"
    );
}

// 6b. A parallel-lane terminate is rejected as an error result, not honored
// (RUNTIME layer) — independent of the static marker: this tool never
// declares `.terminal()`, it just misbehaves at runtime.
#[tokio::test]
async fn parallel_lane_terminate_is_rejected_as_misuse_and_run_continues() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-1", "misbehaving_parallel_terminate", r#"{}"#),
                    ("call-2", "probe_one", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let log = Arc::new(TokioMutex::new(Vec::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(MisbehavingParallelTerminateTool)
        .with_tool(super::support::ProbeTool::new(
            "probe_one",
            true,
            Duration::from_millis(10),
            Arc::clone(&log),
            Arc::clone(&active),
            Arc::clone(&max_active),
        ))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let message = agent
        .send(vec![ContentBlock::text("run the misbehaving tool")])
        .await
        .expect("send should succeed: the misuse is a tool error, not a run failure");

    // The run proceeded to the follow-up round instead of ending — proof the
    // bogus terminate was never honored.
    assert_eq!(message.text(), "done");

    let blocks = tool_result_blocks(agent.history());
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = &blocks[0]
    else {
        panic!("expected a tool result block");
    };
    assert_eq!(tool_use_id, "call-1");
    assert!(*is_error);
    let text = content.to_display_string();
    assert!(text.contains("not honored"));
    assert!(text.contains("parallel"));
}
