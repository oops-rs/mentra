mod common;

use async_trait::async_trait;
use mentra::{
    ContentBlock,
    tool::{
        ParallelToolContext, ToolCapability, ToolDefinition, ToolDurability, ToolExecutor,
        ToolResult, ToolSideEffectLevel, ToolSpec,
    },
};
use serde_json::{Value, json};

struct DelegateSummaryTool;

impl ToolDefinition for DelegateSummaryTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("delegate_summary")
            .description("Ask a disposable subagent to solve the prompt and return its summary.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Delegated prompt for the disposable subagent"
                    }
                },
                "required": ["prompt"]
            }))
            .capability(ToolCapability::Delegation)
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for DelegateSummaryTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        let prompt = input
            .get("prompt")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "prompt is required".to_string())?;

        let mut child = ctx.spawn_subagent().map_err(|error| error.to_string())?;
        let message = child
            .send(vec![ContentBlock::text(prompt)])
            .await
            .map_err(|error| format!("Subagent failed: {error}"))?;

        let summary = message.text();
        if summary.trim().is_empty() {
            Ok("(empty subagent summary)".to_string())
        } else {
            Ok(summary)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = common::openai_runtime()?;
    runtime.register_tool(DelegateSummaryTool);

    let model = common::openai_model(&runtime).await?;
    let prompt = common::first_arg_or(
        "Use delegate_summary to explain, in two concise sentences, why disposable subagents are useful.",
    );

    let mut agent = runtime.spawn("Subagent Tool Demo", model)?;
    let message = agent.send(vec![ContentBlock::text(prompt)]).await?;
    println!("{}", message.text());
    Ok(())
}
