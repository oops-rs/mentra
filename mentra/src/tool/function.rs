use std::{future::Future, marker::PhantomData};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    ExecutableTool, ParallelToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor,
    ToolResult, ToolSpec,
};

/// Adapts an async function or closure into a text-returning tool.
///
/// The runtime deserializes the model's arguments before invoking `handler`.
/// Set the corresponding provider-visible input schema on `descriptor`.
/// The handler receives a [`ParallelToolContext`] in either scheduling lane.
/// Implement [`ToolDefinition`] and [`ToolExecutor`] directly for mutable agent
/// access, structured output, or a custom authorization preview.
pub fn tool_fn<I, F, Fut>(descriptor: ToolSpec, handler: F) -> impl ExecutableTool
where
    I: DeserializeOwned + Send,
    F: Fn(ParallelToolContext, I) -> Fut + Send + Sync,
    Fut: Future<Output = ToolResult> + Send,
{
    FunctionTool {
        descriptor,
        handler,
        input: PhantomData,
    }
}

struct FunctionTool<I, F> {
    descriptor: ToolSpec,
    handler: F,
    input: PhantomData<fn(I)>,
}

impl<I, F: Send + Sync> ToolDefinition for FunctionTool<I, F> {
    fn descriptor(&self) -> ToolSpec {
        self.descriptor.clone()
    }
}

#[async_trait]
impl<I, F, Fut> ToolExecutor for FunctionTool<I, F>
where
    I: DeserializeOwned + Send,
    F: Fn(ParallelToolContext, I) -> Fut + Send + Sync,
    Fut: Future<Output = ToolResult> + Send,
{
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        self.descriptor.execution_category
    }

    async fn execute(&self, context: ParallelToolContext, input: Value) -> ToolResult {
        let input = serde_json::from_value(input)
            .map_err(|error| format!("Invalid {} input: {error}", self.descriptor.name))?;
        (self.handler)(context, input).await
    }
}
