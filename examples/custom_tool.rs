mod common;

use mentra::{
    ContentBlock,
    tool::{ToolCapability, ToolDurability, ToolSpec, tool_fn},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct UppercaseInput {
    text: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = common::openai_runtime()?;
    runtime.register_tool(tool_fn(
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
            .durability(ToolDurability::ReplaySafe)
            .build(),
        async |_, input: UppercaseInput| Ok(input.text.to_uppercase()),
    ));

    let model = common::openai_model(&runtime).await?;
    let prompt = common::first_arg_or(
        "Use the uppercase_text tool on the phrase 'mentra runtime' and then explain what the tool returned.",
    );

    let mut agent = runtime.spawn("Custom Tool Demo", model)?;
    let message = agent.send(vec![ContentBlock::text(prompt)]).await?;
    println!("{}", message.text());
    Ok(())
}
