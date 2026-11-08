mod common;

use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use dotenvy::dotenv;
use mentra::{
    ContentBlock, Runtime, RuntimePolicy,
    agent::{AgentConfig, ToolProfile, WorkspaceConfig},
    tool::{
        ParallelToolContext, ToolCapability, ToolContext, ToolDefinition, ToolDurability,
        ToolExecutor, ToolResult, ToolSideEffectLevel, ToolSpec,
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

impl ToolDefinition for WorkspaceSummaryTool {
    fn descriptor(&self) -> ToolSpec {
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
}

#[async_trait]
impl ToolExecutor for WorkspaceSummaryTool {
    async fn execute(&self, ctx: ParallelToolContext, _input: Value) -> ToolResult {
        let state = ctx.app_context::<ExampleState>()?;
        Ok(json!({
            "workspaceRoot": state.workspace_root,
            "outputDir": state.output_dir,
        })
        .to_string())
    }
}

impl ToolDefinition for SaveArtifactTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("save_artifact")
            .description("Write a generated artifact into the example output directory.")
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Single filename for the artifact"
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
}

#[async_trait]
impl ToolExecutor for SaveArtifactTool {
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

        let path = artifact_path(&state.output_dir, filename)?;
        let path = path.to_str().ok_or("artifact path must be UTF-8")?;
        let artifact_path = ctx.write_file_bytes(path, content.as_bytes())?;

        Ok(json!({
            "saved": artifact_path,
            "bytes": content.len(),
        })
        .to_string())
    }
}

fn artifact_path(output_dir: &Path, filename: &str) -> Result<PathBuf, &'static str> {
    let path = Path::new(filename);
    match path.components().next() {
        Some(Component::Normal(name)) if name == path.as_os_str() => Ok(output_dir.join(name)),
        _ => Err("filename must be a single filename"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let cli = parse_cli()?;
    let workspace_root = std::env::current_dir()?;
    let output_dir = workspace_root.join(".mentra-output");
    let runtime = build_runtime(&workspace_root, &output_dir, cli.mode)?;
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
    mode: Mode,
) -> Result<Runtime, Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY must be set before running this example")?;
    // Grant writes to a real output directory or let the tool create it.
    match fs::symlink_metadata(output_dir) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!("'{}' must be a real directory", output_dir.display()).into());
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let enable_shell = mode == Mode::Queue;

    Ok(Runtime::builder()
        .with_provider(mentra::BuiltinProvider::OpenAI, api_key)
        .with_policy(
            RuntimePolicy::read_only(workspace_root)
                .with_allowed_write_root(output_dir)
                .allow_shell_commands(enable_shell)
                .allow_background_commands(enable_shell),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_paths_require_a_single_filename() {
        let output_dir = Path::new("artifacts");
        assert_eq!(
            artifact_path(output_dir, "review.md").unwrap(),
            output_dir.join("review.md")
        );
        for filename in [
            "",
            ".",
            "..",
            "../escape",
            "subdir/file",
            "/escape",
            "file/",
        ] {
            assert!(artifact_path(output_dir, filename).is_err(), "{filename}");
        }
        #[cfg(windows)]
        for filename in [r"C:\escape", r"C:escape", r"subdir\file"] {
            assert!(artifact_path(output_dir, filename).is_err(), "{filename}");
        }
    }
}
