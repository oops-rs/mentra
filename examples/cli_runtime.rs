mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use dotenvy::dotenv;
use mentra::{
    ContentBlock, Runtime, RuntimePolicy,
    agent::{AgentConfig, ToolProfile, WorkspaceConfig},
    tool::{
        ExecutableTool, ToolCapability, ToolContext, ToolDurability, ToolResult,
        ToolSideEffectLevel, ToolSpec,
    },
};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Queue,
    Direct,
}

#[derive(Debug)]
struct CliArgs {
    mode: Mode,
    prompt: String,
}

#[derive(Debug)]
struct ExampleState {
    workspace_root: PathBuf,
    output_dir: PathBuf,
}

struct WorkspaceSummaryTool;

struct SaveArtifactTool;

#[async_trait]
impl ExecutableTool for WorkspaceSummaryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::builder("workspace_summary")
            .description("Return the configured workspace and output roots.")
            .input_schema(json!({
                "type": "object",
                "properties": {}
            }))
            .capability(ToolCapability::ReadOnly)
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        let state = ctx.app_context::<ExampleState>()?;
        Ok(json!({
            "workspaceRoot": state.workspace_root,
            "outputDir": state.output_dir,
        })
        .to_string())
    }
}

#[async_trait]
impl ExecutableTool for SaveArtifactTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::builder("save_artifact")
            .description("Write a generated artifact into the example output directory.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Name of the artifact file to create"
                    },
                    "content": {
                        "type": "string",
                        "description": "Text content to write"
                    }
                },
                "required": ["filename", "content"]
            }))
            .capability(ToolCapability::FilesystemWrite)
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let state = ctx.app_context::<ExampleState>()?;
        let filename = input
            .get("filename")
            .and_then(Value::as_str)
            .ok_or_else(|| "filename is required".to_string())?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "content is required".to_string())?;

        fs::create_dir_all(&state.output_dir)
            .map_err(|error| format!("failed to create output dir: {error}"))?;
        let artifact_path = state.output_dir.join(filename);
        fs::write(&artifact_path, content)
            .map_err(|error| format!("failed to write artifact: {error}"))?;

        Ok(json!({
            "saved": artifact_path,
            "bytes": content.len(),
        })
        .to_string())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let cli = parse_cli()?;
    let workspace_root = std::env::current_dir()?;
    let output_dir = workspace_root.join(".mentra-output");
    let runtime = build_runtime(&workspace_root, &output_dir)?;
    let model = common::openai_model(&runtime).await?;
    let mut agent = runtime.spawn_with_config(
        "CLI Runtime Demo",
        model,
        AgentConfig {
            workspace: WorkspaceConfig {
                base_dir: workspace_root.clone(),
                ..Default::default()
            },
            tool_profile: tool_profile(cli.mode),
            ..Default::default()
        },
    )?;

    println!("Mode: {}", mode_name(cli.mode));
    println!("Workspace: {}", workspace_root.display());
    println!("Artifacts: {}", output_dir.display());

    let message = agent.send(vec![ContentBlock::text(cli.prompt)]).await?;
    println!("\nFinal response:\n{}\n", message.text());
    print_transcript_summary(agent.history());
    Ok(())
}

fn build_runtime(
    workspace_root: &Path,
    output_dir: &Path,
) -> Result<Runtime, Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY must be set before running this example")?;

    Ok(Runtime::builder()
        .with_provider(mentra::BuiltinProvider::OpenAI, api_key)
        .with_policy(
            RuntimePolicy::permissive()
                .with_allowed_working_root(workspace_root)
                .with_allowed_read_root(workspace_root)
                .with_allowed_write_root(output_dir),
        )
        .with_context(Arc::new(ExampleState {
            workspace_root: workspace_root.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
        }))
        .with_tool(WorkspaceSummaryTool)
        .with_tool(SaveArtifactTool)
        .build()?)
}

fn parse_cli() -> Result<CliArgs, Box<dyn std::error::Error>> {
    let mut mode = Mode::Queue;
    let mut prompt_parts = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                let value = args.next().ok_or("missing value after --mode")?;
                mode = parse_mode(&value)?;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ => prompt_parts.push(arg),
        }
    }

    let prompt = if prompt_parts.is_empty() {
        default_prompt(mode)
    } else {
        prompt_parts.join(" ")
    };

    Ok(CliArgs { mode, prompt })
}

fn parse_mode(value: &str) -> Result<Mode, Box<dyn std::error::Error>> {
    match value {
        "queue" => Ok(Mode::Queue),
        "direct" => Ok(Mode::Direct),
        _ => Err(format!("unsupported mode '{value}', expected 'queue' or 'direct'").into()),
    }
}

fn print_usage() {
    println!("Usage: cli_runtime [--mode queue|direct] [prompt]");
}

fn default_prompt(mode: Mode) -> String {
    match mode {
        Mode::Queue => "Inspect this Rust workspace, summarize the most important packages or directories, and save a short report to review.md if you need to persist the summary.".to_string(),
        Mode::Direct => "Use the available tools to describe the configured workspace and suggest what should be inspected first.".to_string(),
    }
}

fn tool_profile(mode: Mode) -> ToolProfile {
    match mode {
        Mode::Queue => ToolProfile::only([
            "shell",
            "background_run",
            "check_background",
            "files",
            "task",
            "workspace_summary",
            "save_artifact",
        ]),
        Mode::Direct => ToolProfile::only(["files", "workspace_summary", "save_artifact"]),
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Queue => "queue",
        Mode::Direct => "direct",
    }
}

fn print_transcript_summary(history: &[mentra::Message]) {
    println!("Transcript summary:");
    for message in history {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, name, input } => {
                    println!("  tool use: {name} ({id}) input={input}");
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    println!("  tool result: {tool_use_id} error={is_error} content={content}");
                }
                _ => {}
            }
        }
    }
}
