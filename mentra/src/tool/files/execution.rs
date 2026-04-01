use serde_json::Value;
use tokio::sync::broadcast;

use crate::{agent::AgentEvent, runtime::RuntimeHandle, tool::ToolResult};

use super::{
    input::{ensure_files_have_operations, parse_files_input},
    workspace::WorkspaceEditor,
};

pub(crate) async fn execute_files_tool(
    agent_id: String,
    tool_call_id: String,
    tool_name: String,
    runtime: RuntimeHandle,
    default_working_directory: std::path::PathBuf,
    event_tx: broadcast::Sender<AgentEvent>,
    input: Value,
) -> ToolResult {
    let input = parse_files_input(&input)?;
    ensure_files_have_operations(&input)?;

    let working_directory = match input.working_directory.as_deref() {
        Some(directory) => runtime.resolve_working_directory(&agent_id, Some(directory))?,
        None => runtime
            .resolve_working_directory(&agent_id, None)
            .unwrap_or(default_working_directory),
    };
    let base_dir = runtime.agent_config(&agent_id)?.base_dir;

    tokio::task::spawn_blocking(move || {
        let mut editor = WorkspaceEditor::new(agent_id, runtime, base_dir, working_directory);
        let mut sections = Vec::with_capacity(input.operations.len());
        for operation in input.operations {
            let section = editor.apply_operation(operation)?;
            if let Some(progress) = file_op_progress(&section) {
                let _ = event_tx.send(AgentEvent::ToolExecutionProgress {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    progress,
                });
            }
            sections.push(section);
        }
        editor.commit()?;

        Ok(sections.join("\n\n"))
    })
    .await
    .map_err(|error| format!("Files tool task failed: {error}"))?
}

/// Derives a `file_op:` progress string from the operation summary returned by
/// `WorkspaceEditor::apply_operation`.  Only mutating operations that produce a
/// recognisable prefix are surfaced; read-only operations return `None`.
fn file_op_progress(section: &str) -> Option<String> {
    // Mutating operation prefixes produced by WorkspaceEditor.
    let mutating_prefixes = ["create ", "set ", "replace ", "insert ", "move ", "delete "];
    let first_line = section.lines().next().unwrap_or(section);
    if mutating_prefixes
        .iter()
        .any(|prefix| first_line.starts_with(prefix))
    {
        Some(format!("file_op: {first_line}"))
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn file_op_progress_create() {
        let result = file_op_progress("create src/lib.rs");
        assert_eq!(result, Some("file_op: create src/lib.rs".to_string()));
    }

    #[test]
    fn file_op_progress_set() {
        let result = file_op_progress("set src/main.rs");
        assert_eq!(result, Some("file_op: set src/main.rs".to_string()));
    }

    #[test]
    fn file_op_progress_replace() {
        let result = file_op_progress("replace src/lib.rs (1 replacement)");
        assert_eq!(
            result,
            Some("file_op: replace src/lib.rs (1 replacement)".to_string())
        );
    }

    #[test]
    fn file_op_progress_insert() {
        let result = file_op_progress("insert src/lib.rs");
        assert_eq!(result, Some("file_op: insert src/lib.rs".to_string()));
    }

    #[test]
    fn file_op_progress_move() {
        let result = file_op_progress("move old.rs -> new.rs");
        assert_eq!(result, Some("file_op: move old.rs -> new.rs".to_string()));
    }

    #[test]
    fn file_op_progress_delete() {
        let result = file_op_progress("delete src/old.rs");
        assert_eq!(result, Some("file_op: delete src/old.rs".to_string()));
    }

    #[test]
    fn file_op_progress_read_returns_none() {
        let result = file_op_progress("read src/lib.rs\nL1: fn main() {}");
        assert_eq!(result, None);
    }

    #[test]
    fn file_op_progress_list_returns_none() {
        let result = file_op_progress("list src/\n[file] main.rs");
        assert_eq!(result, None);
    }

    #[test]
    fn file_op_progress_search_returns_none() {
        let result = file_op_progress("search src/ /fn /\nsrc/lib.rs:1: fn main() {}");
        assert_eq!(result, None);
    }

    #[test]
    fn file_op_progress_uses_only_first_line() {
        let result = file_op_progress("create foo.rs\nsome extra\ncontent");
        assert_eq!(result, Some("file_op: create foo.rs".to_string()));
    }
}
