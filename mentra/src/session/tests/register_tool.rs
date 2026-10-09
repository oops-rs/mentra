use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{
    ContentBlock, ToolAudience,
    runtime::SessionOptions,
    session::Session,
    test::{MockRuntime, MockToolCall},
    tool::{
        ParallelToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor, ToolResult,
        ToolSpec,
    },
};

/// One name; the description and the output both say which copy answered.
struct LayeredTool {
    label: &'static str,
}

impl ToolDefinition for LayeredTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("layered_tool")
            .description(self.label)
            .execution_category(ToolExecutionCategory::ReadOnlyParallel)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for LayeredTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        ToolExecutionCategory::ReadOnlyParallel
    }

    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        Ok(self.label.to_string())
    }
}

fn result_for(session: &Session, call_id: &str) -> String {
    session
        .history()
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.to_display_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {call_id}"))
}

/// Two sessions in one audience — the pair an audience cannot tell apart —
/// and only the one that registered resolves its own copy.
#[tokio::test]
async fn a_sessions_own_tool_shadows_the_global_for_that_session_alone() {
    let mock = MockRuntime::builder()
        .tool_calls([MockToolCall::new("layered_tool", json!({})).with_id("own-call")])
        .text("own done")
        .tool_calls([MockToolCall::new("layered_tool", json!({})).with_id("sibling-call")])
        .text("sibling done")
        .build()
        .expect("mock runtime");
    mock.runtime()
        .register_tool(LayeredTool { label: "global" });
    let audience = ToolAudience::new("one-directory");
    let open = |name: &str| {
        mock.runtime()
            .create_session_with_options(
                name,
                mock.model(),
                SessionOptions {
                    tool_audience: Some(audience.clone()),
                    ..SessionOptions::default()
                },
            )
            .expect("create session")
    };
    let mut own = open("own");
    let mut sibling = open("sibling");
    own.register_tool(LayeredTool { label: "own" });

    own.append_turn(vec![ContentBlock::text("go")])
        .await
        .expect("own turn");
    sibling
        .append_turn(vec![ContentBlock::text("go")])
        .await
        .expect("sibling turn");

    assert_eq!(result_for(&own, "own-call"), "own");
    assert_eq!(result_for(&sibling, "sibling-call"), "global");
    // The audience's view, which has no agent in it, never sees the override.
    let offered = mock
        .runtime()
        .tools_for_audience(Some(&audience))
        .into_iter()
        .find(|descriptor| descriptor.provider.name == "layered_tool")
        .and_then(|descriptor| descriptor.provider.description);
    assert_eq!(offered.as_deref(), Some("global"));
}
