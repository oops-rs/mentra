#[path = "files/schema.rs"]
mod schema;
#[path = "files/workspace.rs"]
mod workspace;

use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, PathBuf};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
    ToolExecutor, ToolResult, ToolSideEffectLevel,
};

use self::{
    schema::{FileOperation, FilesInput},
    workspace::WorkspaceEditor,
};

pub struct FilesTool;

impl ToolDefinition for FilesTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder("files")
            .description("Read, search, list, create, update, move, and delete files within the workspace.")
            .input_schema(json!({
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
            }))
            .capabilities([
                ToolCapability::FilesystemRead,
                ToolCapability::FilesystemWrite,
            ])
            .side_effect_level(ToolSideEffectLevel::LocalState)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::Filesystem)
            .build()
    }

}

#[async_trait]
impl ToolExecutor for FilesTool {
    fn execution_category(&self, input: &Value) -> ToolExecutionCategory {
        let Ok(input) = serde_json::from_value::<FilesInput>(input.clone()) else {
            return ToolExecutionCategory::ExclusiveLocalMutation;
        };

        if input.operations.iter().all(FileOperation::is_read_only) {
            ToolExecutionCategory::ReadOnlyParallel
        } else {
            ToolExecutionCategory::ExclusiveLocalMutation
        }
    }

    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        build_files_authorization_preview(self.descriptor(), ctx, input)
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

fn build_files_authorization_preview(
    descriptor: RuntimeToolDescriptor,
    ctx: &ParallelToolContext,
    input: &Value,
) -> Result<ToolAuthorizationPreview, String> {
    let raw_input = input.clone();
    let input = serde_json::from_value::<FilesInput>(input.clone())
        .map_err(|error| format!("Invalid files input: {error}"))?;
    if input.operations.is_empty() {
        return Err("At least one file operation is required".to_string());
    }

    let working_directory = match input.working_directory.as_deref() {
        Some(directory) => ctx.resolve_working_directory(Some(directory))?,
        None => ctx
            .runtime
            .resolve_working_directory(&ctx.agent_id, None)
            .unwrap_or_else(|_| ctx.working_directory().to_path_buf()),
    };

    let operations = input
        .operations
        .into_iter()
        .map(|operation| preview_file_operation(&working_directory, operation))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ToolAuthorizationPreview {
        working_directory: working_directory.clone(),
        capabilities: descriptor.capabilities,
        side_effect_level: descriptor.side_effect_level,
        durability: descriptor.durability,
        execution_category: descriptor.execution_category,
        approval_category: descriptor.approval_category,
        raw_input,
        structured_input: json!({
            "kind": "files",
            "working_directory": working_directory,
            "operations": operations,
        }),
    })
}

fn preview_file_operation(
    working_directory: &std::path::Path,
    operation: FileOperation,
) -> Result<Value, String> {
    match operation {
        FileOperation::Read {
            path,
            offset,
            limit,
        } => Ok(json!({
            "op": "read",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "offset": offset,
            "limit": limit,
        })),
        FileOperation::List { path, depth, limit } => Ok(json!({
            "op": "list",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "depth": depth,
            "limit": limit,
        })),
        FileOperation::Search {
            path,
            pattern,
            limit,
        } => Ok(json!({
            "op": "search",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "pattern": pattern,
            "limit": limit,
        })),
        FileOperation::Create { path, .. } => Ok(json!({
            "op": "create",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
        FileOperation::Set { path, .. } => Ok(json!({
            "op": "set",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
        FileOperation::Replace {
            path,
            replace_all,
            expected_replacements,
            ..
        } => Ok(json!({
            "op": "replace",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "replace_all": replace_all,
            "expected_replacements": expected_replacements,
        })),
        FileOperation::Insert {
            path,
            position,
            occurrence,
            ..
        } => Ok(json!({
            "op": "insert",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "position": match position {
                schema::InsertPosition::Before => "before",
                schema::InsertPosition::After => "after",
            },
            "occurrence": occurrence,
        })),
        FileOperation::Move { from, to } => Ok(json!({
            "op": "move",
            "from_resolved_path": resolve_preview_path(working_directory, &from)?,
            "to_resolved_path": resolve_preview_path(working_directory, &to)?,
        })),
        FileOperation::Delete { path } => Ok(json!({
            "op": "delete",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
    }
}

fn resolve_preview_path(working_directory: &std::path::Path, raw: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(raw);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        working_directory.join(candidate)
    };
    normalize_preview_path(path)
}

fn normalize_preview_path(path: PathBuf) -> Result<PathBuf, String> {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.is_absolute() {
                    return Err(format!(
                        "Path '{}' escapes the filesystem root",
                        path.display()
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    if !normalized.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_tool_metadata_marks_local_mutation() {
        let spec = FilesTool.descriptor();
        assert!(!spec.capabilities.contains(&ToolCapability::ReadOnly));
        assert_eq!(spec.durability, ToolDurability::Ephemeral);
        assert!(spec.capabilities.contains(&ToolCapability::FilesystemRead));
        assert!(spec.capabilities.contains(&ToolCapability::FilesystemWrite));
        assert_eq!(
            spec.execution_category,
            ToolExecutionCategory::ExclusiveLocalMutation
        );
    }

    #[test]
    fn read_only_operations_opt_into_parallel_execution() {
        let category = FilesTool.execution_category(&json!({
            "operations": [
                { "op": "read", "path": "README.md" },
                { "op": "search", "path": ".", "pattern": "mentra" }
            ]
        }));

        assert_eq!(category, ToolExecutionCategory::ReadOnlyParallel);
    }

    #[test]
    fn mutating_operations_stay_exclusive() {
        let category = FilesTool.execution_category(&json!({
            "operations": [
                { "op": "read", "path": "README.md" },
                { "op": "set", "path": "README.md", "content": "updated" }
            ]
        }));

        assert_eq!(category, ToolExecutionCategory::ExclusiveLocalMutation);
    }
}
