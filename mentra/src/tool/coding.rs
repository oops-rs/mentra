#[path = "coding/execution.rs"]
mod execution;
#[path = "coding/input.rs"]
mod input;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability, ToolContext,
    ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolOutput, ToolResult,
    ToolSideEffectLevel,
};

use execution::{
    execute_edit, execute_glob, execute_grep, execute_list, execute_read, execute_write,
};

pub(crate) struct ReadTool;
pub(crate) struct ListTool;
pub(crate) struct GrepTool;
pub(crate) struct GlobTool;
pub(crate) struct WriteTool;
pub(crate) struct EditTool;

fn read_only_descriptor(
    name: &str,
    description: &str,
    input_schema: Value,
) -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder(name)
        .description(description)
        .input_schema(input_schema)
        .capabilities([ToolCapability::ReadOnly, ToolCapability::FilesystemRead])
        .side_effect_level(ToolSideEffectLevel::None)
        .durability(ToolDurability::ReplaySafe)
        .execution_category(ToolExecutionCategory::ReadOnlyParallel)
        .approval_category(ToolApprovalCategory::ReadOnly)
        .build()
}

fn mutation_descriptor(
    name: &str,
    description: &str,
    input_schema: Value,
) -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder(name)
        .description(description)
        .input_schema(input_schema)
        .capability(ToolCapability::FilesystemWrite)
        .side_effect_level(ToolSideEffectLevel::LocalState)
        .durability(ToolDurability::Ephemeral)
        .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
        .approval_category(ToolApprovalCategory::Filesystem)
        .build()
}

impl ToolDefinition for ReadTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        read_only_descriptor(
            "read",
            "Read a UTF-8 text file from the workspace with line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "file_path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 0 }
                },
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["file_path"] }
                ]
            }),
        )
    }
}

impl ToolDefinition for ListTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        read_only_descriptor(
            "ls",
            "List files and directories within the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 0 }
                }
            }),
        )
    }
}

impl ToolDefinition for GrepTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        read_only_descriptor(
            "grep",
            "Search workspace text files with a regular expression or literal pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "ignore_case": { "type": "boolean" },
                    "literal": { "type": "boolean" },
                    "context": { "type": "integer", "minimum": 0 },
                    "multiline": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 0 }
                },
                "required": ["pattern"]
            }),
        )
    }
}

impl ToolDefinition for GlobTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        read_only_descriptor(
            "glob",
            "Find workspace files whose relative paths match a glob pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 0 }
                },
                "required": ["pattern"]
            }),
        )
    }
}

impl ToolDefinition for WriteTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        mutation_descriptor(
            "write",
            "Create or overwrite a UTF-8 text file within the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "file_path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["content"],
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["file_path"] }
                ]
            }),
        )
    }
}

impl ToolDefinition for EditTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        mutation_descriptor(
            "edit",
            "Replace one or more uniquely matched text blocks in a workspace file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "file_path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" }
                            },
                            "required": ["old_string", "new_string"]
                        },
                        "minItems": 1
                    },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["edits"],
                "anyOf": [
                    { "required": ["path"] },
                    { "required": ["file_path"] }
                ]
            }),
        )
    }
}

#[async_trait]
impl ToolExecutor for ReadTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_read(ctx, input).await
    }
}

#[async_trait]
impl ToolExecutor for ListTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_list(ctx, input).await
    }
}

#[async_trait]
impl ToolExecutor for GrepTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_grep(ctx, input).await
    }
}

#[async_trait]
impl ToolExecutor for GlobTool {
    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        execute_glob(ctx, input).await
    }
}

#[async_trait]
impl ToolExecutor for WriteTool {
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        execute_write(ctx.into(), input).await
    }
}

#[async_trait]
impl ToolExecutor for EditTool {
    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        execute_edit(ctx.into(), input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tools_have_static_scheduler_categories() {
        for descriptor in [
            ReadTool.descriptor(),
            ListTool.descriptor(),
            GrepTool.descriptor(),
            GlobTool.descriptor(),
        ] {
            assert_eq!(
                descriptor.execution_category,
                ToolExecutionCategory::ReadOnlyParallel
            );
        }
        for descriptor in [WriteTool.descriptor(), EditTool.descriptor()] {
            assert_eq!(
                descriptor.execution_category,
                ToolExecutionCategory::ExclusiveLocalMutation
            );
        }
    }
}
