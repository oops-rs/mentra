use async_trait::async_trait;
use serde_json::json;

use crate::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolCapability,
    ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor, ToolResult,
    ToolSideEffectLevel,
};

pub struct CheckBackgroundTool;
pub struct LoadSkillTool;

fn check_background_descriptor() -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder("check_background")
        .description("Check one background task by ID, or list all background tasks when omitted.")
        .input_schema(json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Optional background task ID to inspect"
                }
            }
        }))
        .capability(ToolCapability::ReadOnly)
        .side_effect_level(ToolSideEffectLevel::None)
        .durability(ToolDurability::ReplaySafe)
        .execution_category(ToolExecutionCategory::ReadOnlyParallel)
        .approval_category(ToolApprovalCategory::ReadOnly)
        .build()
}

fn load_skill_descriptor() -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder("load_skill")
        .description("Load the full body of a named skill when it is relevant.")
        .input_schema(json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to load"
                }
            },
            "required": ["name"]
        }))
        .capabilities([ToolCapability::SkillLoad, ToolCapability::ReadOnly])
        .side_effect_level(ToolSideEffectLevel::None)
        .durability(ToolDurability::ReplaySafe)
        .execution_category(ToolExecutionCategory::ReadOnlyParallel)
        .approval_category(ToolApprovalCategory::ReadOnly)
        .build()
}

impl ToolDefinition for CheckBackgroundTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        check_background_descriptor()
    }
}

#[async_trait]
impl ToolExecutor for CheckBackgroundTool {
    async fn execute(&self, ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
        let task_id = input.get("task_id").and_then(|value| value.as_str());
        ctx.check_background_task(task_id)
    }
}

impl ToolDefinition for LoadSkillTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        load_skill_descriptor()
    }
}

#[async_trait]
impl ToolExecutor for LoadSkillTool {
    async fn execute(&self, ctx: ParallelToolContext, input: serde_json::Value) -> ToolResult {
        let name = input
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Skill name is required".to_string())?;
        ctx.load_skill(name)
    }
}
