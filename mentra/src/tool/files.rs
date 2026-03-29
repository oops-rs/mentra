#[path = "files/execution.rs"]
mod execution;
#[path = "files/input.rs"]
mod input;
#[path = "files/preview.rs"]
mod preview;
#[path = "files/schema.rs"]
mod schema;
#[path = "files/workspace.rs"]
mod workspace;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
    ToolExecutor, ToolResult, ToolSideEffectLevel,
};

use self::{
    execution::execute_files_tool, input::file_execution_category,
    preview::build_files_authorization_preview,
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
        file_execution_category(input)
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
