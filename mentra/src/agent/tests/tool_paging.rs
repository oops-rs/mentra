//! End-to-end coverage for automatic tool-result paging: what the model sees,
//! what the event stream keeps, and how `read_tool_result` walks a retained
//! result window by window.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, Message, Role, ToolResultPagingConfig,
    agent::AgentEvent,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::{Runtime, RuntimePolicy},
    tool::{
        ParallelToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor,
        ToolResult, ToolSideEffectLevel, ToolSpec,
    },
};

use super::support::{ScriptedProvider, StaticTool, StreamScript, model_info, ok_stream};

/// Builds `count` lines of exactly 20 bytes each (`{tag}-{n:03}` padded), so
/// every window boundary asserted below is exact arithmetic on line counts
/// rather than an approximation.
fn numbered_lines(tag: &str, count: usize) -> String {
    assert_eq!(
        tag.len(),
        4,
        "the fixed 20-byte line layout assumes a 4-byte tag"
    );
    (1..=count)
        .map(|line| format!("{tag}-{line:03}{}\n", "x".repeat(11)))
        .collect()
}

/// Removes the runtime's own tool-result caps from the picture. Paging runs
/// downstream of that limiter, so a threshold above `max_tool_result_bytes`
/// would never be reached — every paging test has to raise the caps first,
/// exactly as a real consumer enabling paging must.
fn unlimited_results() -> RuntimePolicy {
    RuntimePolicy::default()
        .with_max_tool_result_bytes(usize::MAX)
        .with_max_tool_result_lines(usize::MAX)
        .spill_full_tool_output(false)
}

fn paged_config(threshold_bytes: usize, page_bytes: usize) -> AgentConfig {
    AgentConfig {
        tool_result_paging: Some(ToolResultPagingConfig {
            threshold_bytes,
            page_bytes,
        }),
        ..Default::default()
    }
}

fn tool_results(messages: &[Message]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.to_display_string(), *is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn paging_readers_are_exact_agent_lifetime_registrations() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let paged_a = runtime
        .spawn_with_config("paged-a", model.clone(), paged_config(100, 100))
        .expect("paged agent a");
    let paged_b = runtime
        .spawn_with_config("paged-b", model.clone(), paged_config(100, 100))
        .expect("paged agent b");
    let unpaged = runtime.spawn("unpaged", model).expect("unpaged agent");
    let runtime_handle = paged_a.runtime_handle();

    for agent in [&paged_a, &paged_b] {
        assert!(
            agent
                .tools()
                .iter()
                .any(|tool| tool.name == "read_tool_result")
        );
    }
    assert!(
        unpaged
            .tools()
            .iter()
            .all(|tool| tool.name != "read_tool_result")
    );
    assert!(runtime.tool_descriptor("read_tool_result").is_none());
    {
        let registry = runtime_handle
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned");
        assert!(
            registry
                .resolve_agent_tool(paged_a.id(), "read_tool_result")
                .is_some()
        );
        assert!(
            registry
                .resolve_agent_tool(paged_b.id(), "read_tool_result")
                .is_some()
        );
        assert!(
            registry
                .resolve_agent_tool(unpaged.id(), "read_tool_result")
                .is_none()
        );
    }

    let paged_a_id = paged_a.id().to_string();
    drop(paged_a);
    assert!(
        runtime_handle
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .resolve_agent_tool(&paged_a_id, "read_tool_result")
            .is_none()
    );
    assert!(
        paged_b
            .tools()
            .iter()
            .any(|tool| tool.name == "read_tool_result")
    );
    let paged_b_id = paged_b.id().to_string();
    drop(paged_b);
    assert!(
        runtime_handle
            .tooling
            .tool_registry
            .read()
            .expect("tool registry poisoned")
            .resolve_agent_tool(&paged_b_id, "read_tool_result")
            .is_none()
    );
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

fn read_window_input(tool_use_id: &str, start_line: usize) -> String {
    json!({ "tool_use_id": tool_use_id, "start_line": start_line }).to_string()
}

/// A parallel-lane tool returning a caller-supplied oversized result.
struct ParallelPagedTool {
    name: &'static str,
    output: String,
}

impl ToolDefinition for ParallelPagedTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder(self.name)
            .description("test tool: returns an oversized parallel result")
            .input_schema(json!({ "type": "object", "properties": {} }))
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for ParallelPagedTool {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok(self.output.clone())
    }
}

// (a) With paging unconfigured, an oversized result is inserted whole and the
// reader is neither registered nor offered to the model.
#[tokio::test]
async fn unpaged_agents_receive_oversized_results_whole_without_the_reader() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("agent", model).expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the big tool")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, full, "the result must be byte-identical");
    assert!(
        agent
            .tools()
            .iter()
            .all(|tool| tool.name != "read_tool_result"),
        "read_tool_result must not be offered to an unpaged agent"
    );
    assert!(
        runtime.tool_descriptor("read_tool_result").is_none(),
        "read_tool_result must not be registered for an unpaged agent"
    );
}

// (e) With paging enabled, a result at or below the threshold is still
// byte-identical — only the roster changes.
#[tokio::test]
async fn sub_threshold_results_stay_byte_identical_under_paging() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    assert_eq!(full.len(), 800);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(800, 100))
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the big tool")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    assert_eq!(results[0].1, full, "a result at the threshold is not paged");
    assert!(!results[0].1.contains("[paged:"));
    assert!(
        agent
            .tools()
            .iter()
            .any(|tool| tool.name == "read_tool_result"),
        "the reader is offered whenever paging is enabled, not only once it fires"
    );
}

// (b) An oversized result reaches the model as page 1 plus a trailer, while
// the event stream still carries the complete block.
#[tokio::test]
async fn oversized_results_reach_the_model_paged_and_the_event_stream_whole() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(100, 100))
        .expect("spawn agent");
    let mut events = agent.subscribe_events();

    agent
        .send(vec![ContentBlock::text("run the big tool")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    assert_eq!(results.len(), 1);
    let page = &results[0].1;
    assert!(page.starts_with(&numbered_lines("line", 5)));
    assert!(!page.contains("line-006"));
    assert!(
        page.contains(
            "…[paged: lines 1–5 of 40 (0.1 KB of 0.8 KB). \
             Call read_tool_result(tool_use_id=\"call-1\", start_line=6) for the next window.]"
        ),
        "unexpected trailer: {page}"
    );

    let finished = collect_events(&mut events)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionFinished { result } => Some(result),
            _ => None,
        })
        .expect("a ToolExecutionFinished event");
    let ContentBlock::ToolResult { content, .. } = finished else {
        panic!("expected a tool result block");
    };
    assert_eq!(
        content.to_display_string(),
        full,
        "the event stream must keep carrying the unpaged result"
    );
}

// (c) Successive windows tile the result with absolute line numbers, the last
// one is marked as the end, and a start_line past the end is empty.
#[tokio::test]
async fn read_tool_result_windows_tile_the_result_and_mark_the_end() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::tool_use_stream(
                &model.id,
                "call-2",
                "read_tool_result",
                &read_window_input("call-1", 6),
            ),
            super::support::tool_use_stream(
                &model.id,
                "call-3",
                "read_tool_result",
                &read_window_input("call-1", 36),
            ),
            super::support::tool_use_stream(
                &model.id,
                "call-4",
                "read_tool_result",
                &read_window_input("call-1", 41),
            ),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(100, 100))
        .expect("spawn agent");

    let message = agent
        .send(vec![ContentBlock::text("read the whole result")])
        .await
        .expect("send");
    assert_eq!(message.text(), "done");

    let results = tool_results(agent.history());
    assert_eq!(results.len(), 4);

    assert!(results[1].1.starts_with("line-006"));
    assert!(results[1].1.contains("lines 6–10 of 40"));
    assert!(results[1].1.contains("start_line=11"));

    assert!(results[2].1.starts_with("line-036"));
    assert!(results[2].1.contains("line-040"));
    assert!(
        results[2].1.ends_with("…[end of result]"),
        "the window reaching the last line ends the result: {}",
        results[2].1
    );
    assert!(!results[2].1.contains("[paged:"));

    assert_eq!(
        results[3].1, "…[end of result]",
        "a start_line past the end is an empty window, not an error"
    );
    assert!(!results[3].2, "reading past the end is not a tool error");
}

// The reader's own windows are never paged again, even when `page_bytes`
// exceeds `threshold_bytes` and a window is therefore itself "oversized".
#[tokio::test]
async fn read_tool_result_windows_are_never_paged_recursively() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::tool_use_stream(
                &model.id,
                "call-2",
                "read_tool_result",
                &read_window_input("call-1", 16),
            ),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(100, 300))
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("read a large window")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    let window = &results[1].1;
    assert!(window.starts_with("line-016"));
    assert!(window.contains("lines 16–30 of 40"));
    assert_eq!(
        window.matches("[paged:").count(),
        1,
        "a window must carry exactly one trailer, never a trailer nested in a page: {window}"
    );
    assert!(
        !window.contains("call-2"),
        "a window must never be re-paged under its own tool_use_id: {window}"
    );
}

// (d) An unknown tool_use_id is an ordinary tool error and the run continues.
#[tokio::test]
async fn an_unknown_tool_use_id_is_a_tool_error_and_the_run_continues() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let full = numbered_lines("line", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::tool_use_stream(
                &model.id,
                "call-2",
                "read_tool_result",
                &read_window_input("call-does-not-exist", 1),
            ),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(100, 100))
        .expect("spawn agent");

    let message = agent
        .send(vec![ContentBlock::text(
            "read a result that was never paged",
        )])
        .await
        .expect("an unknown id must not fail the run");

    assert_eq!(message.text(), "done");
    let results = tool_results(agent.history());
    assert!(results[1].2, "the failed read is an is_error result");
    assert!(results[1].1.contains("no retained result for tool_use_id"));
    assert!(results[1].1.contains("call-does-not-exist"));
}

// (f) A single line longer than a page is the one case that cuts mid-line:
// it cuts on a character boundary and says so.
#[tokio::test]
async fn a_line_longer_than_a_page_hard_cuts_on_a_character_boundary() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    // 50 four-byte characters: a 200-byte line, plus a short second line.
    let full = format!("{}\ntail line\n", "𝄞".repeat(50));
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            super::support::tool_use_stream(&model.id, "call-1", "big_tool", r#"{}"#),
            super::support::tool_use_stream(
                &model.id,
                "call-2",
                "read_tool_result",
                &read_window_input("call-1", 2),
            ),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(StaticTool::success("big_tool", &full))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        // 102 is not a multiple of the 4-byte character width, so a correct
        // cut must round down to 100.
        .spawn_with_config("agent", model, paged_config(100, 102))
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run the long-line tool")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    let page = &results[0].1;
    assert!(page.starts_with(&"𝄞".repeat(25)));
    assert!(!page.starts_with(&"𝄞".repeat(26)));
    assert!(
        page.contains("…[line 1 hard-cut at 100 of 201 bytes"),
        "unexpected hard-cut marker: {page}"
    );
    assert!(page.contains("start_line=2"));

    assert!(
        results[1].1.starts_with("tail line\n"),
        "the next window resumes at the following whole line: {}",
        results[1].1
    );
    assert!(results[1].1.ends_with("…[end of result]"));
}

// (g) Parallel oversized results page independently, and each one's full text
// is retained under its own tool_use_id.
#[tokio::test]
async fn parallel_oversized_results_page_independently_per_tool_use_id() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let first = numbered_lines("aaaa", 40);
    let second = numbered_lines("bbbb", 40);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            multi_tool_use_stream(
                &model.id,
                &[
                    ("call-a", "parallel_a", r#"{}"#),
                    ("call-b", "parallel_b", r#"{}"#),
                ],
            ),
            super::support::tool_use_stream(
                &model.id,
                "call-c",
                "read_tool_result",
                &read_window_input("call-b", 6),
            ),
            super::support::text_stream(&model.id, "done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_policy(unlimited_results())
        .with_tool(ParallelPagedTool {
            name: "parallel_a",
            output: first,
        })
        .with_tool(ParallelPagedTool {
            name: "parallel_b",
            output: second,
        })
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("agent", model, paged_config(100, 100))
        .expect("spawn agent");

    agent
        .send(vec![ContentBlock::text("run both parallel tools")])
        .await
        .expect("send");

    let results = tool_results(agent.history());
    assert_eq!(results.len(), 3);

    assert_eq!(results[0].0, "call-a");
    assert!(results[0].1.starts_with("aaaa-001"));
    assert!(results[0].1.contains("tool_use_id=\"call-a\""));
    assert!(!results[0].1.contains("bbbb"));

    assert_eq!(results[1].0, "call-b");
    assert!(results[1].1.starts_with("bbbb-001"));
    assert!(results[1].1.contains("tool_use_id=\"call-b\""));
    assert!(!results[1].1.contains("aaaa"));

    assert!(
        results[2].1.starts_with("bbbb-006"),
        "each result is retained under its own id: {}",
        results[2].1
    );
    assert!(results[2].1.contains("lines 6–10 of 40"));
}

fn collect_events(receiver: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}
