//! Forwarding impls that let a pointer to a tool *be* a tool.
//!
//! [`RuntimeBuilder::with_tool`](crate::runtime::RuntimeBuilder::with_tool)
//! takes `impl ExecutableTool` by value, so without these a host could not
//! register a tool it chose at runtime (`Box<dyn ExecutableTool>`) or hand one
//! shared instance to more than one runtime (`Arc<T>`) — the pointer itself was
//! not a tool. The alternative was every host hand-writing the same eight
//! forwarding methods next to a security-relevant one: forgetting
//! [`ToolExecutor::authorization_preview`] leaves a tool presenting to the
//! approver as something other than what it is, because the trait default
//! silently reconstructs a preview from the descriptor instead of asking the
//! tool. That trap is why the forwarding lives here, once.
//!
//! The forwarding sits on the two *sub*-traits rather than on
//! [`ExecutableTool`](super::ExecutableTool) itself. `ExecutableTool` already
//! has a blanket impl for every `ToolDefinition + ToolExecutor`, so an impl
//! written directly for `Box<T>`/`Arc<T>` would overlap it. Forwarding one
//! layer down composes instead: `Box<dyn ExecutableTool>` gains both
//! sub-traits here and the existing blanket impl carries it the rest of the
//! way.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::{
    ParallelToolContext, RuntimeToolDescriptor, ToolAuthorizationPreview, ToolContext,
    ToolDefinition, ToolExecutionCategory, ToolExecutionMode, ToolExecutor, ToolOutput, ToolResult,
};

/// Forwards to the tool inside.
///
/// Lets a caller hold a tool it chose at runtime — one of several, or one
/// loaded from a plugin — and still hand it to anything taking
/// `impl ExecutableTool`.
impl<T: ToolDefinition + ?Sized> ToolDefinition for Box<T> {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        (**self).descriptor()
    }
}

/// Forwards to the tool inside, for a shared one.
impl<T: ToolDefinition + ?Sized> ToolDefinition for Arc<T> {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        (**self).descriptor()
    }
}

/// Forwards to the tool inside.
///
/// Every method is forwarded, defaulted ones included. A method left to its
/// trait default would answer for the *wrapper* rather than the tool, and for
/// [`authorization_preview`](ToolExecutor::authorization_preview) that means
/// the approver sees a preview rebuilt from the descriptor instead of the one
/// the tool wrote — a tool presenting as something other than what it is.
#[async_trait]
impl<T: ToolExecutor + ?Sized> ToolExecutor for Box<T> {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        (**self).authorization_preview(ctx, input)
    }

    fn execution_category(&self, input: &Value) -> ToolExecutionCategory {
        (**self).execution_category(input)
    }

    fn execution_mode(&self, input: &Value) -> ToolExecutionMode {
        (**self).execution_mode(input)
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        (**self).execute(ctx, input).await
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        (**self).execute_mut(ctx, input).await
    }

    async fn execute_output(
        &self,
        ctx: ParallelToolContext,
        input: Value,
    ) -> Result<ToolOutput, String> {
        (**self).execute_output(ctx, input).await
    }

    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        (**self).execute_mut_output(ctx, input).await
    }
}

/// Forwards to the tool inside, for a shared one.
///
/// This is the path for handing one tool instance to more than one runtime:
/// the tool's own state stays behind a single `Arc`, and each runtime holds a
/// clone of the pointer rather than a copy of the tool. Every method is
/// forwarded here for the same reason as on [`Box`] above.
#[async_trait]
impl<T: ToolExecutor + ?Sized> ToolExecutor for Arc<T> {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        (**self).authorization_preview(ctx, input)
    }

    fn execution_category(&self, input: &Value) -> ToolExecutionCategory {
        (**self).execution_category(input)
    }

    fn execution_mode(&self, input: &Value) -> ToolExecutionMode {
        (**self).execution_mode(input)
    }

    async fn execute(&self, ctx: ParallelToolContext, input: Value) -> ToolResult {
        (**self).execute(ctx, input).await
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        (**self).execute_mut(ctx, input).await
    }

    async fn execute_output(
        &self,
        ctx: ParallelToolContext,
        input: Value,
    ) -> Result<ToolOutput, String> {
        (**self).execute_output(ctx, input).await
    }

    async fn execute_mut_output(
        &self,
        ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        (**self).execute_mut_output(ctx, input).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::{
        agent::Agent,
        test::MockRuntime,
        tool::{
            ExecutableTool, ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory,
            ToolAuthorizationPreview, ToolCapability, ToolContext, ToolDefinition, ToolDurability,
            ToolExecutionCategory, ToolExecutionMode, ToolExecutor, ToolOutput, ToolResult,
            ToolResultContent, ToolSideEffectLevel,
        },
    };

    const PROBE_TOOL_NAME: &str = "forwarding_probe";

    /// A tool that overrides every defaulted method with a value the default
    /// could not produce.
    ///
    /// That is the whole point: a wrapper that quietly falls back to a trait
    /// default instead of forwarding still compiles and still returns
    /// something plausible. Only a tool whose every answer differs from its
    /// own defaults turns that silence into a failing assertion.
    struct ProbeTool;

    impl ToolDefinition for ProbeTool {
        fn descriptor(&self) -> RuntimeToolDescriptor {
            RuntimeToolDescriptor::builder(PROBE_TOOL_NAME)
                .description("Probe every forwarded method")
                .input_schema(json!({ "type": "object", "properties": {} }))
                // Deliberately the opposite of what `authorization_preview`
                // and `execution_category` below report, so a fallback to the
                // defaults — which read exactly these fields — is visible.
                .capabilities([ToolCapability::ReadOnly])
                .side_effect_level(ToolSideEffectLevel::None)
                .durability(ToolDurability::Ephemeral)
                .execution_category(ToolExecutionCategory::ReadOnlyParallel)
                .approval_category(ToolApprovalCategory::Default)
                .build()
        }
    }

    #[async_trait]
    impl ToolExecutor for ProbeTool {
        fn authorization_preview(
            &self,
            ctx: &ParallelToolContext,
            input: &Value,
        ) -> Result<ToolAuthorizationPreview, String> {
            Ok(ToolAuthorizationPreview {
                working_directory: ctx.working_directory().to_path_buf(),
                capabilities: vec![ToolCapability::ProcessExec],
                side_effect_level: ToolSideEffectLevel::External,
                durability: ToolDurability::Persistent,
                execution_category: ToolExecutionCategory::Delegation,
                approval_category: ToolApprovalCategory::Process,
                raw_input: input.clone(),
                structured_input: json!({ "normalized": input }),
            })
        }

        fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
            ToolExecutionCategory::BackgroundJob
        }

        fn execution_mode(&self, _input: &Value) -> ToolExecutionMode {
            // `BackgroundJob.into()` is `Exclusive`, so `Parallel` here is
            // unreachable by the default even when `execution_category`
            // forwards correctly.
            ToolExecutionMode::Parallel
        }

        async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
            Ok("probe::execute".to_string())
        }

        async fn execute_mut(&self, _ctx: ToolContext<'_>, _input: Value) -> ToolResult {
            Ok("probe::execute_mut".to_string())
        }

        async fn execute_output(
            &self,
            _ctx: ParallelToolContext,
            _input: Value,
        ) -> Result<ToolOutput, String> {
            Ok(
                ToolOutput::structured(json!({ "from": "probe::execute_output" }))
                    .with_details(json!({ "lane": "parallel" })),
            )
        }

        async fn execute_mut_output(
            &self,
            _ctx: ToolContext<'_>,
            _input: Value,
        ) -> Result<ToolOutput, String> {
            Ok(
                ToolOutput::structured(json!({ "from": "probe::execute_mut_output" }))
                    .with_details(json!({ "lane": "exclusive" }))
                    .terminating(),
            )
        }
    }

    fn parallel_context(agent: &Agent) -> ParallelToolContext {
        ParallelToolContext {
            agent_id: agent.id().to_string(),
            tool_call_id: "probe-call".to_string(),
            tool_name: PROBE_TOOL_NAME.to_string(),
            working_directory: std::env::temp_dir(),
            runtime: agent.runtime_handle(),
            subagent_template: agent.disposable_subagent_template(),
            agent_name: agent.name().to_string(),
            model: agent.model().to_string(),
            history_len: agent.history().len(),
            tasks: agent.tasks().to_vec(),
            event_tx: agent.event_sender(),
            run_options: crate::runtime::RunOptions::default(),
        }
    }

    fn exclusive_context(agent: &mut Agent) -> ToolContext<'_> {
        let agent_id = agent.id().to_string();
        let runtime = agent.runtime_handle();
        let event_tx = agent.event_sender();
        ToolContext {
            agent_id,
            tool_call_id: "probe-call".to_string(),
            tool_name: PROBE_TOOL_NAME.to_string(),
            working_directory: std::env::temp_dir(),
            runtime,
            agent,
            event_tx,
            run_options: crate::runtime::RunOptions::default(),
        }
    }

    /// Asserts all eight methods of the two sub-traits, observed through
    /// `tool`, answer what `ProbeTool` answers — never what the trait default
    /// would have answered in its place.
    async fn assert_every_method_forwards<T>(tool: &T, agent: &mut Agent)
    where
        T: ExecutableTool + ?Sized,
    {
        let input = json!({ "value": "hi" });

        // 1. ToolDefinition::descriptor
        let descriptor = tool.descriptor();
        assert_eq!(descriptor.provider.name, PROBE_TOOL_NAME);
        assert_eq!(descriptor.capabilities, vec![ToolCapability::ReadOnly]);

        // 2. ToolExecutor::authorization_preview — the security-relevant one.
        // Every field here differs from what the default would rebuild out of
        // the descriptor above.
        let preview = tool
            .authorization_preview(&parallel_context(agent), &input)
            .expect("forwarded authorization preview");
        assert_eq!(preview.capabilities, vec![ToolCapability::ProcessExec]);
        assert_eq!(preview.side_effect_level, ToolSideEffectLevel::External);
        assert_eq!(preview.durability, ToolDurability::Persistent);
        assert_eq!(
            preview.execution_category,
            ToolExecutionCategory::Delegation
        );
        assert_eq!(preview.approval_category, ToolApprovalCategory::Process);
        assert_eq!(preview.raw_input, input);
        assert_eq!(preview.structured_input, json!({ "normalized": input }));

        // 3. ToolExecutor::execution_category
        assert_eq!(
            tool.execution_category(&input),
            ToolExecutionCategory::BackgroundJob
        );

        // 4. ToolExecutor::execution_mode
        assert_eq!(tool.execution_mode(&input), ToolExecutionMode::Parallel);

        // 5. ToolExecutor::execute
        assert_eq!(
            tool.execute(parallel_context(agent), input.clone()).await,
            Ok("probe::execute".to_string())
        );

        // 6. ToolExecutor::execute_output
        let output = tool
            .execute_output(parallel_context(agent), input.clone())
            .await
            .expect("forwarded parallel structured output");
        assert_eq!(
            output.content,
            ToolResultContent::Structured(json!({ "from": "probe::execute_output" }))
        );
        assert_eq!(output.details, Some(json!({ "lane": "parallel" })));
        assert!(!output.terminate);

        // 7. ToolExecutor::execute_mut — `&self` despite the name.
        assert_eq!(
            tool.execute_mut(exclusive_context(agent), input.clone())
                .await,
            Ok("probe::execute_mut".to_string())
        );

        // 8. ToolExecutor::execute_mut_output
        let output = tool
            .execute_mut_output(exclusive_context(agent), input.clone())
            .await
            .expect("forwarded exclusive structured output");
        assert_eq!(
            output.content,
            ToolResultContent::Structured(json!({ "from": "probe::execute_mut_output" }))
        );
        assert_eq!(output.details, Some(json!({ "lane": "exclusive" })));
        assert!(output.terminate);
    }

    async fn probe_agent() -> (MockRuntime, Agent) {
        let mock = MockRuntime::builder().build().expect("build mock runtime");
        let agent = mock
            .runtime()
            .spawn("forwarding-probe", mock.model())
            .expect("spawn probe agent");
        (mock, agent)
    }

    #[tokio::test]
    async fn a_boxed_tool_forwards_every_method() {
        let (_mock, mut agent) = probe_agent().await;
        let tool: Box<dyn ExecutableTool> = Box::new(ProbeTool);

        assert_every_method_forwards(&tool, &mut agent).await;
    }

    #[tokio::test]
    async fn a_boxed_sized_tool_forwards_every_method() {
        let (_mock, mut agent) = probe_agent().await;
        let tool = Box::new(ProbeTool);

        assert_every_method_forwards(&tool, &mut agent).await;
    }

    #[tokio::test]
    async fn a_shared_tool_forwards_every_method() {
        let (_mock, mut agent) = probe_agent().await;
        let tool = Arc::new(ProbeTool);

        assert_every_method_forwards(&tool, &mut agent).await;
    }

    #[tokio::test]
    async fn a_shared_unsized_tool_forwards_every_method() {
        let (_mock, mut agent) = probe_agent().await;
        let tool: Arc<dyn ExecutableTool> = Arc::new(ProbeTool);

        assert_every_method_forwards(&tool, &mut agent).await;
    }

    /// The registration ergonomic the forwarding exists for: a pointer to a
    /// tool is accepted where a tool is, and what lands in the registry still
    /// answers with the inner tool's overrides rather than the defaults.
    #[tokio::test]
    async fn a_runtime_registers_boxed_and_shared_tools() {
        let (mock, agent) = probe_agent().await;

        mock.runtime()
            .register_tool(Box::new(ProbeTool) as Box<dyn ExecutableTool>);
        let registered = agent
            .runtime_handle()
            .get_tool(PROBE_TOOL_NAME)
            .expect("boxed tool registered");
        assert_eq!(registered.descriptor().provider.name, PROBE_TOOL_NAME);
        assert_eq!(
            registered
                .authorization_preview(&parallel_context(&agent), &json!({}))
                .expect("preview through the registered boxed tool")
                .side_effect_level,
            ToolSideEffectLevel::External,
        );

        mock.runtime().unregister_tool(PROBE_TOOL_NAME);

        let shared = Arc::new(ProbeTool);
        mock.runtime().register_tool(Arc::clone(&shared));
        let registered = agent
            .runtime_handle()
            .get_tool(PROBE_TOOL_NAME)
            .expect("shared tool registered");
        assert_eq!(
            registered
                .authorization_preview(&parallel_context(&agent), &json!({}))
                .expect("preview through the registered shared tool")
                .side_effect_level,
            ToolSideEffectLevel::External,
        );
    }
}
