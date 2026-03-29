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

struct UppercaseTool;

impl ToolDefinition for UppercaseTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("uppercase_text")
            .description("Uppercase the provided text and return the transformed value.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to transform"
                    }
                },
                "required": ["text"]
            }))
            .capability(ToolCapability::ReadOnly)
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for UppercaseTool {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let text = input
            .get("text")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "text is required".to_string())?;

        Ok(text.to_uppercase())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = common::openai_runtime()?;
    runtime.register_tool(UppercaseTool);

    let model = common::openai_model(&runtime).await?;
    let prompt = common::first_arg_or(
        "Use the uppercase_text tool on the phrase 'mentra runtime' and then explain what the tool returned.",
    );

    let mut agent = runtime.spawn("Custom Tool Demo", model)?;
    let message = agent.send(vec![ContentBlock::text(prompt)]).await?;
    println!("{}", message.text());
    Ok(())
}
