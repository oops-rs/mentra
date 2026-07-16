//! Tests for the M2 structured `ToolOutput` seam (ADR-0001 §3): the bridge
//! from `ToolResult`, structured content + opaque `details`, and the two
//! layers of termination exclusivity.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex as TokioMutex, mpsc},
    time::{Duration, sleep},
};

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, FileToolProfile, Role, TerminalOutputSpec,
    agent::{CompactionConfig, ToolProfile, WorkspaceConfig},
    provider::{ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, ToolChoice},
    runtime::{RunOptions, Runtime, RuntimeError, RuntimeHookEvent, RuntimePolicy},
    tool::{
        ParallelToolContext, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
        ToolExecutor, ToolOutput, ToolResult, ToolResultContent, ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{
    ScriptedProvider, StaticTool, StreamScript, controlled_stream, model_info, ok_stream,
};

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

/// A pair of parallel-eligible tools that each attach distinct `details`,
/// used to prove a round with several tool calls maps each result's
/// metadata to its own `tool_use_id` rather than to the wrong call or a
/// single collapsed value (M3 test 4).
struct DetailsToolA;

#[async_trait]
impl ToolDefinition for DetailsToolA {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("details_tool_a")
            .description("test tool: returns details keyed to call A")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for DetailsToolA {
    async fn execute_output(
        &self,
        _ctx: ParallelToolContext,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text("a-result").with_details(json!({ "who": "a" })))
    }
}

struct DetailsToolB;

#[async_trait]
impl ToolDefinition for DetailsToolB {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("details_tool_b")
            .description("test tool: returns details keyed to call B")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for DetailsToolB {
    async fn execute_output(
        &self,
        _ctx: ParallelToolContext,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text("b-result").with_details(json!({ "who": "b" })))
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

struct ParallelOutputTool {
    name: &'static str,
    output: &'static str,
    delay: Duration,
}

#[async_trait]
impl ToolDefinition for ParallelOutputTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name)
            .description("test tool: returns a large parallel result")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for ParallelOutputTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        sleep(self.delay).await;
        Ok(self.output.to_string())
    }
}

struct OversizedStructuredTerminatingTool;

#[async_trait]
impl ToolDefinition for OversizedStructuredTerminatingTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("oversized_structured_terminal")
            .description("test tool: spills structured output while preserving metadata")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .terminal()
            .build()
    }
}

#[async_trait]
impl ToolExecutor for OversizedStructuredTerminatingTool {
    async fn execute_mut_output(
        &self,
        _ctx: ToolContext<'_>,
        _input: Value,
    ) -> Result<ToolOutput, String> {
        Ok(
            ToolOutput::structured(json!({ "payload": "x".repeat(128) }))
                .with_details(json!({ "private": 42 }))
                .terminating(),
        )
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

#[tokio::test]
async fn exclusive_success_and_error_outputs_truncate_with_identical_rules() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-ok", "large_success", r#"{}"#),
                    ("call-err", "large_error", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(
            RuntimePolicy::default()
                .with_max_tool_result_bytes(usize::MAX)
                .with_max_tool_result_lines(1)
                .spill_full_tool_output(false),
        )
        .with_tool(StaticTool::success("large_success", "one\ntwo\nthree"))
        .with_tool(StaticTool::failure("large_error", "bad\nworse\nworst"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run both tools")])
        .await
        .expect("send");

    let blocks = tool_result_blocks(agent.history());
    assert_eq!(blocks.len(), 2);
    assert!(matches!(
        &blocks[0],
        ContentBlock::ToolResult { content: ToolResultContent::Text(content), is_error: false, .. }
            if content.starts_with("one\n[truncated: showing 1 of 3 lines;")
    ));
    assert!(matches!(
        &blocks[1],
        ContentBlock::ToolResult { content: ToolResultContent::Text(content), is_error: true, .. }
            if content.starts_with("bad\n[truncated: showing 1 of 3 lines;")
    ));
}

#[tokio::test]
async fn parallel_outputs_truncate_per_result_and_preserve_call_order() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-a", "large_parallel_a", r#"{}"#),
                    ("call-b", "large_parallel_b", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(
            RuntimePolicy::default()
                .with_max_tool_result_bytes(usize::MAX)
                .with_max_tool_result_lines(1)
                .spill_full_tool_output(false),
        )
        .with_tool(ParallelOutputTool {
            name: "large_parallel_a",
            output: "a-one\na-two",
            delay: Duration::from_millis(25),
        })
        .with_tool(ParallelOutputTool {
            name: "large_parallel_b",
            output: "b-one\nb-two",
            delay: Duration::from_millis(1),
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run both parallel tools")])
        .await
        .expect("send");

    let blocks = tool_result_blocks(agent.history());
    assert!(matches!(
        &blocks[0],
        ContentBlock::ToolResult { tool_use_id, content: ToolResultContent::Text(content), .. }
            if tool_use_id == "call-a" && content.starts_with("a-one\n[truncated:")
    ));
    assert!(matches!(
        &blocks[1],
        ContentBlock::ToolResult { tool_use_id, content: ToolResultContent::Text(content), .. }
            if tool_use_id == "call-b" && content.starts_with("b-one\n[truncated:")
    ));
}

#[tokio::test]
async fn oversized_structured_output_spills_without_losing_details_or_termination() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![tool_use_stream(
            &model.id,
            "call-structured",
            "oversized_structured_terminal",
            r#"{}"#,
        )],
    );
    let spill_root = std::env::temp_dir().join(format!(
        "mentra-structured-spill-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default().with_max_tool_result_bytes(32))
        .with_tool(OversizedStructuredTerminatingTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    transcript_dir: spill_root.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn agent");

    let result = agent
        .run(
            vec![ContentBlock::text("run structured tool")],
            RunOptions::default(),
        )
        .await;
    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));

    let blocks = tool_result_blocks(agent.history());
    assert!(matches!(
        &blocks[0],
        ContentBlock::ToolResult { content: ToolResultContent::Text(content), is_error: false, .. }
            if content.contains("structured tool output") && content.contains("full output at")
    ));
    let item = agent
        .transcript()
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item.kind, crate::TranscriptKind::ToolExchange { .. }))
        .expect("tool exchange item");
    assert_eq!(
        item.detail("call-structured"),
        Some(&json!({ "private": 42 }))
    );

    let output_dir = spill_root.join("tool-output");
    let files = std::fs::read_dir(&output_dir)
        .expect("read spill directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("read spill files");
    assert_eq!(files.len(), 1);
    let stored = std::fs::read_to_string(files[0].path()).expect("read spill output");
    assert_eq!(
        stored,
        serde_json::to_string(&json!({ "payload": "x".repeat(128) })).unwrap()
    );
    std::fs::remove_dir_all(spill_root).expect("remove spill root");
}

#[tokio::test]
async fn spill_failure_preserves_structured_details_and_termination() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![tool_use_stream(
            &model.id,
            "call-structured",
            "oversized_structured_terminal",
            r#"{}"#,
        )],
    );
    let blocking_file = std::env::temp_dir().join(format!(
        "mentra-structured-spill-blocker-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::write(&blocking_file, "not a directory").expect("create blocking file");
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(RuntimePolicy::default().with_max_tool_result_bytes(32))
        .with_tool(OversizedStructuredTerminatingTool)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                compaction: CompactionConfig {
                    transcript_dir: blocking_file.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn agent");

    let result = agent
        .run(
            vec![ContentBlock::text("run structured tool")],
            RunOptions::default(),
        )
        .await;
    assert!(matches!(result, Err(RuntimeError::EmptyAssistantResponse)));

    let blocks = tool_result_blocks(agent.history());
    assert!(matches!(
        &blocks[0],
        ContentBlock::ToolResult { content: ToolResultContent::Text(content), is_error: false, .. }
            if content.contains("full output could not be saved")
    ));
    let item = agent
        .transcript()
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item.kind, crate::TranscriptKind::ToolExchange { .. }))
        .expect("tool exchange item");
    assert_eq!(
        item.detail("call-structured"),
        Some(&json!({ "private": 42 }))
    );
    assert!(blocking_file.is_file());
    std::fs::remove_file(blocking_file).expect("remove blocking file");
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

#[tokio::test]
async fn edit_tool_details_never_enter_provider_projected_result_content() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(
                &model.id,
                "call-edit",
                "edit",
                r#"{"path":"note.txt","edits":[{"old_string":"before","new_string":"after"}]}"#,
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("duration")
        .as_nanos();
    let workspace = std::env::temp_dir().join(format!("mentra-edit-projection-{unique}"));
    std::fs::create_dir_all(&workspace).expect("create workspace");
    std::fs::write(workspace.join("note.txt"), "before\n").expect("write note");
    let runtime = Runtime::builder()
        .with_file_tools(FileToolProfile::Split)
        .with_provider_instance(provider.clone())
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                workspace: WorkspaceConfig {
                    base_dir: workspace.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("apply the requested edit")])
        .await
        .expect("send");

    let detail = agent
        .transcript()
        .items()
        .iter()
        .find_map(|item| item.detail("call-edit"))
        .expect("edit details must survive locally");
    assert!(detail.get("diff").is_some());
    assert!(detail.get("patch").is_some());
    assert_eq!(detail["first_changed_line"], json!(1));

    let requests = provider.recorded_requests().await;
    let projected = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "call-edit" => Some(content),
            _ => None,
        })
        .expect("provider-projected edit result");
    assert_eq!(
        projected,
        &ToolResultContent::Text("Replaced 1 block in note.txt".to_string())
    );

    std::fs::remove_dir_all(workspace).expect("remove workspace");
}

// M3 tests 3 & 4: a round with two parallel tool calls, each returning
// distinct `details`, maps every result's metadata to its own `tool_use_id`
// on the committed transcript item — not to the wrong call, and not
// collapsed into one value — and a host recovers it afterward through the
// plain public `TranscriptItem` accessor, with no mentra-side host type or
// downcast involved.
#[tokio::test]
async fn parallel_round_maps_each_results_details_to_its_own_tool_use_id() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-a", "details_tool_a", r#"{}"#),
                    ("call-b", "details_tool_b", r#"{}"#),
                ],
            ),
            text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(DetailsToolA)
        .with_tool(DetailsToolB)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run both details tools")])
        .await
        .expect("send");

    // Public accessor, reached straight off `Agent::transcript()` — no
    // downcast, no mentra-side knowledge of what "who" means.
    let item = agent
        .transcript()
        .items()
        .iter()
        .rev()
        .find(|item| matches!(item.kind, crate::TranscriptKind::ToolExchange { .. }))
        .expect("tool exchange item committed");

    let expected: BTreeMap<String, Value> = BTreeMap::from([
        ("call-a".to_string(), json!({ "who": "a" })),
        ("call-b".to_string(), json!({ "who": "b" })),
    ]);
    assert_eq!(item.details(), Some(&expected));
    assert_eq!(item.detail("call-a"), Some(&json!({ "who": "a" })));
    assert_eq!(item.detail("call-b"), Some(&json!({ "who": "b" })));
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

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct TypedAnswer {
    answer: u64,
    evidence: Vec<String>,
}

#[tokio::test]
async fn run_to_output_forces_scoped_terminal_tool_and_extracts_exact_call_detail() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![stream],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "target",
            model.clone(),
            AgentConfig {
                tool_profile: ToolProfile::only(["ordinary_tool_only"]),
                ..AgentConfig::default()
            },
        )
        .expect("spawn target");
    let other_agent = runtime.spawn("other", model).expect("spawn other");
    let steering = agent.steering_handle();
    steering.steer(vec![ContentBlock::text("keep this queued")]);

    let drive = async {
        wait_for_recorded_requests(&provider_handle, 1).await;
        let requests = provider_handle.recorded_requests().await;
        let ToolChoice::Tool { name } = requests[0]
            .tool_choice
            .clone()
            .expect("terminal tool choice")
        else {
            panic!("run_to_output must force its terminal tool");
        };
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(requests[0].tools[0].name, name);
        assert!(name.starts_with("mentra_terminal_finish_report_"));
        assert!(
            other_agent.tools().iter().all(|tool| tool.name != name),
            "a scoped terminal tool must not leak into another agent's request"
        );
        send_tool_response(
            &tx,
            "model",
            "terminal-call-42",
            &name,
            r#"{"answer":42,"evidence":["a","b"]}"#,
        );
        drop(tx);
        name
    };
    let (result, tool_name) = tokio::join!(
        agent.run_to_output::<TypedAnswer>(
            vec![ContentBlock::text("produce typed output")],
            RunOptions::default(),
            TerminalOutputSpec::new(
                "finish-report",
                "Return the final report",
                json!({
                    "type": "object",
                    "properties": {
                        "answer": { "type": "integer" },
                        "evidence": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["answer", "evidence"]
                }),
            ),
        ),
        drive
    );

    let output = result.expect("typed terminal output succeeds");
    assert_eq!(
        output.value,
        TypedAnswer {
            answer: 42,
            evidence: vec!["a".to_string(), "b".to_string()],
        }
    );
    assert_eq!(output.message.role, Role::User);
    assert!(matches!(
        output.message.content.as_slice(),
        [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "terminal-call-42"
    ));
    let last = agent.transcript().items().last().expect("terminal item");
    assert_eq!(
        last.detail("terminal-call-42"),
        Some(&json!({ "answer": 42, "evidence": ["a", "b"] }))
    );
    assert!(
        steering.has_pending(),
        "terminal end_turn precedes steering"
    );
    assert!(
        runtime.tool_descriptor(&tool_name).is_none(),
        "the generated tool is unregistered after the run"
    );
}

#[tokio::test]
async fn run_to_output_never_reuses_stale_terminal_details() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (first_stream, first_tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![first_stream, text_stream(&model.id, "plain answer")],
    );
    let provider_handle = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    let drive = async {
        wait_for_recorded_requests(&provider_handle, 1).await;
        let requests = provider_handle.recorded_requests().await;
        let ToolChoice::Tool { name } = requests[0]
            .tool_choice
            .clone()
            .expect("terminal tool choice")
        else {
            panic!("expected forced tool");
        };
        send_tool_response(
            &first_tx,
            "model",
            "first-terminal-call",
            &name,
            r#"{"answer":1,"evidence":[]}"#,
        );
        drop(first_tx);
    };
    let (first, ()) = tokio::join!(
        agent.run_to_output::<TypedAnswer>(
            vec![ContentBlock::text("first")],
            RunOptions::default(),
            terminal_spec(),
        ),
        drive
    );
    assert_eq!(first.expect("first output").value.answer, 1);

    let error = agent
        .run_to_output::<TypedAnswer>(
            vec![ContentBlock::text("second")],
            RunOptions::default(),
            terminal_spec(),
        )
        .await
        .expect_err("plain response must not reuse prior details");
    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool")
    );
}

fn terminal_spec() -> TerminalOutputSpec {
    TerminalOutputSpec::new(
        "finish",
        "Return typed output",
        json!({
            "type": "object",
            "properties": {
                "answer": { "type": "integer" },
                "evidence": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["answer", "evidence"]
        }),
    )
}

async fn wait_for_recorded_requests(provider: &ScriptedProvider, expected: usize) {
    loop {
        if provider.recorded_requests().await.len() >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn send_tool_response(
    tx: &mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    model: &str,
    id: &str,
    name: &str,
    input: &str,
) {
    let events = [
        ProviderEvent::MessageStarted {
            id: format!("message-{id}"),
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
            delta: ContentBlockDelta::ToolUseInputJson(input.to_string()),
        },
        ProviderEvent::ContentBlockStopped { index: 0 },
        ProviderEvent::MessageStopped,
    ];
    for event in events {
        tx.send(Ok(event)).expect("stream receiver remains alive");
    }
}
