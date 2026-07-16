use serde_json::{Value, json};

use crate::tool::{ParallelToolContext, ToolOutput, ToolResult};

use super::input::{parse_edit, parse_glob, parse_grep, parse_list, parse_read, parse_write};
use crate::tool::files::workspace::{EditOutcome, TextEdit, WorkspaceEditor};

pub(super) async fn execute_read(ctx: ParallelToolContext, input: Value) -> ToolResult {
    let input = parse_read(input)?;
    with_editor(ctx, move |editor| {
        editor.read(
            input.path,
            input.offset.unwrap_or(1),
            input.limit.unwrap_or(2_000),
        )
    })
    .await
}

pub(super) async fn execute_list(ctx: ParallelToolContext, input: Value) -> ToolResult {
    let input = parse_list(input)?;
    with_editor(ctx, move |editor| {
        editor.list(
            input.path.unwrap_or_else(|| ".".to_string()),
            input.depth.unwrap_or(1),
            input.limit.unwrap_or(200),
        )
    })
    .await
}

pub(super) async fn execute_grep(ctx: ParallelToolContext, input: Value) -> ToolResult {
    let input = parse_grep(input)?;
    let options = input.search_options();
    with_editor(ctx, move |editor| {
        editor.grep(
            input.path.unwrap_or_else(|| ".".to_string()),
            &input.pattern,
            options,
            input.limit.unwrap_or(200),
        )
    })
    .await
}

pub(super) async fn execute_glob(ctx: ParallelToolContext, input: Value) -> ToolResult {
    let input = parse_glob(input)?;
    with_editor(ctx, move |editor| {
        editor.glob(
            input.path.unwrap_or_else(|| ".".to_string()),
            &input.pattern,
            input.limit.unwrap_or(200),
        )
    })
    .await
}

pub(super) async fn execute_write(ctx: ParallelToolContext, input: Value) -> ToolResult {
    let input = parse_write(input)?;
    with_editor(ctx, move |editor| editor.write(input.path, input.content)).await
}

pub(super) async fn execute_edit(
    ctx: ParallelToolContext,
    input: Value,
) -> Result<ToolOutput, String> {
    let input = parse_edit(input)?;
    let edits = input
        .edits
        .into_iter()
        .map(TextEdit::from)
        .collect::<Vec<_>>();
    let outcome = with_editor(ctx, move |editor| {
        editor.edit(input.path, edits, input.replace_all)
    })
    .await?;
    Ok(edit_output(outcome))
}

fn edit_output(outcome: EditOutcome) -> ToolOutput {
    let block_label = if outcome.replacement_count == 1 {
        "block"
    } else {
        "blocks"
    };
    ToolOutput::text(format!(
        "Replaced {} {block_label} in {}",
        outcome.replacement_count, outcome.display_path
    ))
    .with_details(json!({
        "diff": outcome.diff,
        "patch": outcome.patch,
        "first_changed_line": outcome.first_changed_line,
    }))
}

async fn with_editor<T, F>(ctx: ParallelToolContext, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut WorkspaceEditor) -> Result<T, String> + Send + 'static,
{
    let base_dir = ctx.runtime.agent_config(&ctx.agent_id)?.base_dir;
    let working_directory = ctx
        .runtime
        .resolve_working_directory(&ctx.agent_id, None)
        .unwrap_or_else(|_| ctx.working_directory().to_path_buf());
    let agent_id = ctx.agent_id;
    let runtime = ctx.runtime;

    tokio::task::spawn_blocking(move || {
        let mut editor = WorkspaceEditor::new(agent_id, runtime, base_dir, working_directory);
        let result = operation(&mut editor)?;
        editor.commit()?;
        Ok(result)
    })
    .await
    .map_err(|error| format!("Coding tool task failed: {error}"))?
}
