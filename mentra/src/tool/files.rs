#[path = "files/schema.rs"]
mod schema;
#[path = "files/workspace.rs"]
mod workspace;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ExecutableTool, ParallelToolContext, ToolCapability, ToolContext, ToolDurability,
    ToolExecutionMode, ToolResult, ToolSideEffectLevel, ToolSpec,
};

use self::{
    schema::{FileOperation, FilesInput},
    workspace::WorkspaceEditor,
};

pub struct FilesTool;

#[async_trait]
impl ExecutableTool for FilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "files".to_string(),
            description: Some(
                "Read, search, list, create, update, move, and delete files within the workspace."
                    .into(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workingDirectory": {
                        "type": "string",
                        "description": "Optional directory used to resolve relative operation paths"
                    },
                    "operations": {
                        "type": "array",
                        "description": "Ordered file operations to execute. Later reads can observe earlier staged writes.",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "read" },
                                        "path": { "type": "string" },
                                        "offset": { "type": "integer", "minimum": 1 },
                                        "limit": { "type": "integer", "minimum": 0 }
                                    },
                                    "required": ["op", "path"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "list" },
                                        "path": { "type": "string" },
                                        "depth": { "type": "integer", "minimum": 0 },
                                        "limit": { "type": "integer", "minimum": 0 }
                                    },
                                    "required": ["op", "path"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "search" },
                                        "path": { "type": "string" },
                                        "pattern": { "type": "string" },
                                        "limit": { "type": "integer", "minimum": 0 }
                                    },
                                    "required": ["op", "path", "pattern"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "create" },
                                        "path": { "type": "string" },
                                        "content": { "type": "string" }
                                    },
                                    "required": ["op", "path", "content"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "set" },
                                        "path": { "type": "string" },
                                        "content": { "type": "string" }
                                    },
                                    "required": ["op", "path", "content"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "replace" },
                                        "path": { "type": "string" },
                                        "old": { "type": "string" },
                                        "new": { "type": "string" },
                                        "replaceAll": { "type": "boolean" },
                                        "expectedReplacements": { "type": "integer", "minimum": 0 }
                                    },
                                    "required": ["op", "path", "old", "new"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "insert" },
                                        "path": { "type": "string" },
                                        "anchor": { "type": "string" },
                                        "position": {
                                            "type": "string",
                                            "enum": ["before", "after"]
                                        },
                                        "content": { "type": "string" },
                                        "occurrence": { "type": "integer", "minimum": 1 }
                                    },
                                    "required": ["op", "path", "anchor", "position", "content"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "move" },
                                        "from": { "type": "string" },
                                        "to": { "type": "string" }
                                    },
                                    "required": ["op", "from", "to"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "const": "delete" },
                                        "path": { "type": "string" }
                                    },
                                    "required": ["op", "path"]
                                }
                            ]
                        }
                    }
                },
                "required": ["operations"]
            }),
            capabilities: vec![
                ToolCapability::FilesystemRead,
                ToolCapability::FilesystemWrite,
            ],
            side_effect_level: ToolSideEffectLevel::LocalState,
            durability: ToolDurability::Ephemeral,
        }
    }

    fn execution_mode(&self, input: &Value) -> ToolExecutionMode {
        let Ok(input) = serde_json::from_value::<FilesInput>(input.clone()) else {
            return ToolExecutionMode::Exclusive;
        };

        if input.operations.iter().all(FileOperation::is_read_only) {
            ToolExecutionMode::Parallel
        } else {
            ToolExecutionMode::Exclusive
        }
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_files_tool(
            ctx.agent_id.clone(),
            ctx.runtime.clone(),
            ctx.resolve_working_directory(None)
                .unwrap_or_else(|_| ctx.working_directory().to_path_buf()),
            input,
        )
        .await
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        execute_files_tool(
            ctx.agent_id.clone(),
            ctx.runtime.clone(),
            ctx.resolve_working_directory(None)
                .unwrap_or_else(|_| ctx.working_directory().to_path_buf()),
            input,
        )
        .await
    }
}

async fn execute_files_tool(
    agent_id: String,
    runtime: crate::runtime::RuntimeHandle,
    default_working_directory: std::path::PathBuf,
    input: Value,
) -> ToolResult {
    let input = serde_json::from_value::<FilesInput>(input)
        .map_err(|error| format!("Invalid files input: {error}"))?;
    if input.operations.is_empty() {
        return Err("At least one file operation is required".to_string());
    }

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
            sections.push(editor.apply_operation(operation)?);
        }
        editor.commit()?;

        Ok(sections.join("\n\n"))
    })
    .await
    .map_err(|error| format!("Files tool task failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_tool_metadata_marks_local_mutation() {
        let spec = FilesTool.spec();
        assert!(!spec.capabilities.contains(&ToolCapability::ReadOnly));
        assert_eq!(spec.durability, ToolDurability::Ephemeral);
        assert!(spec.capabilities.contains(&ToolCapability::FilesystemRead));
        assert!(spec.capabilities.contains(&ToolCapability::FilesystemWrite));
    }

    #[test]
    fn read_only_operations_opt_into_parallel_execution() {
        let mode = FilesTool.execution_mode(&json!({
            "operations": [
                { "op": "read", "path": "README.md" },
                { "op": "search", "path": ".", "pattern": "mentra" }
            ]
        }));

        assert_eq!(mode, ToolExecutionMode::Parallel);
    }

    #[test]
    fn mutating_operations_stay_exclusive() {
        let mode = FilesTool.execution_mode(&json!({
            "operations": [
                { "op": "read", "path": "README.md" },
                { "op": "set", "path": "README.md", "content": "updated" }
            ]
        }));

        assert_eq!(mode, ToolExecutionMode::Exclusive);
    }
}
