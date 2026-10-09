use async_trait::async_trait;
use serde_json::Value;

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, Runtime,
    tool::{
        ParallelToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor, ToolResult,
        ToolSpec,
    },
};

use super::support::{ScriptedProvider, model_info, text_stream, tool_use_stream};

/// One name, a description and an output both saying which copy answered.
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

fn description(agent: &crate::agent::Agent) -> Option<String> {
    agent
        .tools()
        .iter()
        .find(|tool| tool.name == "layered_tool")
        .and_then(|tool| tool.description.clone())
}

fn result_for(agent: &crate::agent::Agent, call_id: &str) -> String {
    agent
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

#[tokio::test]
async fn an_agents_own_tool_shadows_the_global_for_that_agent_alone() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "own-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "own done"),
            tool_use_stream(&model.id, "other-call", "layered_tool", r#"{}"#),
            text_stream(&model.id, "other done"),
        ],
    );
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    runtime.register_tool(LayeredTool { label: "global" });

    let mut own = runtime
        .spawn_with_config("own", model.clone(), AgentConfig::default())
        .expect("own agent");
    let mut other = runtime
        .spawn_with_config("other", model, AgentConfig::default())
        .expect("other agent");
    own.register_tool(LayeredTool { label: "first" });
    own.register_tool(LayeredTool { label: "own" });

    // Offered: the latest registration for this agent, the global for the rest.
    assert_eq!(description(&own).as_deref(), Some("own"));
    assert_eq!(description(&other).as_deref(), Some("global"));

    // Executed: the same split, by the handler each agent resolves.
    own.send(vec![ContentBlock::text("go")])
        .await
        .expect("own run");
    other
        .send(vec![ContentBlock::text("go")])
        .await
        .expect("other run");
    assert_eq!(result_for(&own, "own-call"), "own");
    assert_eq!(result_for(&other, "other-call"), "global");

    // Gone with the agent: its id resolves the global again.
    let handle = own.runtime_handle();
    let id = own.id().to_string();
    drop(own);
    let after = handle
        .visible_tool_registrations(&id)
        .into_iter()
        .find(|registration| registration.name() == "layered_tool")
        .and_then(|registration| registration.descriptor().provider.description.clone());
    assert_eq!(after.as_deref(), Some("global"));
}
