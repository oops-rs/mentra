//! Tool orchestration pipeline for scheduling, authorization, execution, and result ordering.

use std::{future::Future, path::PathBuf, sync::Arc, time::Duration};

use tokio::task::JoinSet;

use crate::{
    ContentBlock,
    agent::{Agent, AgentEvent, AgentStatus},
    error::RuntimeError,
    runtime::control::{HookDecision, PreExecutionContext},
    runtime::{RunOptions, RuntimeHookEvent},
    tool::{
        ExecutableTool, ParallelToolContext, RuntimeToolDescriptor, ToolAuthorizationOutcome,
        ToolAuthorizationRequest, ToolCall, ToolCapability, ToolContext, ToolExecutionCategory,
    },
};

const PARALLEL_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct ToolExecutionOutcome {
    pub(crate) results: Vec<ContentBlock>,
    pub(crate) successful_task: bool,
    pub(crate) end_turn: bool,
}

pub(crate) struct ToolRuntime {
    runtime: crate::runtime::handle::RuntimeHandle,
    agent_id: String,
    tool_calls: usize,
    working_directory: Option<PathBuf>,
}

#[derive(Clone)]
enum ToolCallBatch {
    Exclusive(ToolCall),
    Parallel(Vec<ToolCall>),
}

struct ToolCallSchedule {
    batches: Vec<ToolCallBatch>,
}

struct CompletedToolExecution {
    result: ContentBlock,
    task_succeeded: bool,
    /// Ends the current round: true when this execution consumed
    /// [`crate::tool::ToolContext::request_idle`] (exclusive lane) or its
    /// [`crate::tool::ToolOutput::terminate`] successor. Controls whether
    /// `TurnRunner::run` issues another model round.
    should_end_turn: bool,
    /// True only when `should_end_turn` came from `ToolOutput::terminate`
    /// specifically (never from the pre-existing idle-request signal).
    /// Distinct from `should_end_turn` because it additionally drives
    /// skipping not-yet-executed batches later in the same round — a new
    /// behavior scoped to genuine termination, not to idle requests, so
    /// existing `request_idle` callers see unchanged behavior.
    terminated: bool,
    tool_name: String,
}

/// How a single execution affects the current round — bundled so
/// [`ToolRuntime::completed_execution`] stays within a reasonable argument
/// count. `Default` is "continues": neither ends the round nor terminates.
#[derive(Debug, Clone, Copy, Default)]
struct RoundEffect {
    should_end_turn: bool,
    terminated: bool,
}

impl ToolRuntime {
    pub(crate) fn new(agent: &Agent) -> Self {
        Self {
            runtime: agent.runtime_handle(),
            agent_id: agent.id().to_string(),
            tool_calls: 0,
            working_directory: None,
        }
    }

    pub(crate) async fn execute_calls(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        calls: Vec<ToolCall>,
    ) -> Result<ToolExecutionOutcome, RuntimeError> {
        let mut results = Vec::new();
        let mut successful_task = false;
        let mut end_turn = false;

        let mut batches = ToolCallSchedule::new(self, agent, calls)
            .batches
            .into_iter();

        while let Some(batch) = batches.next() {
            options.check_limits()?;
            let execution_count = batch.execution_count();
            if self.tool_calls + execution_count > options.tool_budget() {
                return Err(RuntimeError::ToolBudgetExceeded(options.tool_budget()));
            }
            self.tool_calls += execution_count;

            let executions = match batch {
                ToolCallBatch::Exclusive(call) => vec![self.execute_one_tool(agent, call).await?],
                ToolCallBatch::Parallel(calls) => {
                    self.execute_parallel_batch(agent, options, calls).await?
                }
            };

            let mut terminator = None;
            for execution in executions {
                successful_task |= execution.task_succeeded;
                end_turn |= execution.should_end_turn;
                if execution.terminated {
                    terminator.get_or_insert(execution.tool_name);
                }
                results.push(execution.result);
            }

            // A terminating call ends the round as the value of its own
            // execution; calls already scheduled for later batches in this
            // round are never executed. Each still gets an explicit
            // is_error result so the transcript always has one result block
            // per tool_use — never a silent drop.
            if let Some(terminator) = terminator {
                for remaining_batch in batches {
                    for call in remaining_batch.into_calls() {
                        results.push(not_executed_result(&call, &terminator));
                    }
                }
                break;
            }
        }

        Ok(ToolExecutionOutcome {
            results,
            successful_task,
            end_turn,
        })
    }

    fn call_execution_category_for_agent(
        &self,
        call: &ToolCall,
        agent: Option<&Agent>,
    ) -> ToolExecutionCategory {
        if agent.is_some_and(|agent| !agent.can_use_tool(&call.name)) {
            return ToolExecutionCategory::ExclusiveLocalMutation;
        }

        let Some(tool) = self.runtime.get_tool(&call.name) else {
            return ToolExecutionCategory::ExclusiveLocalMutation;
        };
        let category = tool.execution_category(&call.input);
        let terminal = self
            .runtime
            .get_tool_descriptor(&call.name)
            .is_some_and(|descriptor| descriptor.terminal);

        // STATIC exclusivity: a terminal-marked tool is never scheduled in a
        // parallel batch, regardless of its declared execution_category —
        // coerce rather than panic, matching the existing fallback-to-exclusive
        // precedent above.
        if terminal && category.allows_parallel() {
            eprintln!(
                "warning: tool '{}' is marked terminal but declared a parallel \
                 execution category; coercing to exclusive scheduling",
                call.name
            );
            return ToolExecutionCategory::ExclusiveLocalMutation;
        }

        category
    }

    fn note_tool_started(
        &mut self,
        agent: &mut Agent,
        call: &ToolCall,
    ) -> Result<(), RuntimeError> {
        agent.set_status(AgentStatus::ExecutingTool {
            id: call.id.clone(),
            name: call.name.clone(),
        });
        agent.emit_event(AgentEvent::ToolExecutionStarted { call: call.clone() });
        agent.update_run_state("executing_tool", None)
    }

    fn emit_tool_runtime_started(&self, call: &ToolCall) -> Result<(), RuntimeError> {
        self.runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionStarted {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
            })
    }

    fn emit_tool_runtime_finished(
        &self,
        call: &ToolCall,
        result: &ContentBlock,
        details: Option<serde_json::Value>,
    ) {
        let is_error = matches!(result, ContentBlock::ToolResult { is_error: true, .. });
        let output_preview = match result {
            ContentBlock::ToolResult { content, .. } => content.to_display_string(),
            _ => String::new(),
        };
        let error = is_error.then_some(output_preview.clone());
        let _ = self
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionFinished {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                is_error,
                error,
                output_preview,
                details,
            });
    }

    fn emit_tool_authorization_started(
        &self,
        call: &ToolCall,
        preview: crate::tool::ToolAuthorizationPreview,
    ) -> Result<(), RuntimeError> {
        self.runtime
            .emit_hook(RuntimeHookEvent::ToolAuthorizationStarted {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                preview,
            })
    }

    fn emit_tool_authorization_finished(
        &self,
        call: &ToolCall,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.runtime
            .emit_hook(RuntimeHookEvent::ToolAuthorizationFinished {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                outcome,
                reason,
            })
    }

    fn emit_tool_authorization_blocked(
        &self,
        call: &ToolCall,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.runtime
            .emit_hook(RuntimeHookEvent::ToolAuthorizationBlocked {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                outcome,
                reason,
            })
    }

    fn run_pre_hooks(&self, call: &ToolCall) -> Result<HookDecision, RuntimeError> {
        let context = PreExecutionContext {
            agent_id: self.agent_id.clone(),
            tool_name: call.name.clone(),
            tool_call_id: call.id.clone(),
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
        };
        self.runtime.pre_hooks().run(&context)
    }

    fn emit_tool_execution_blocked(&self, call: &ToolCall, reason: &str) {
        let _ = self
            .runtime
            .emit_hook(RuntimeHookEvent::ToolExecutionBlocked {
                agent_id: self.agent_id.clone(),
                tool_name: call.name.clone(),
                tool_call_id: call.id.clone(),
                reason: reason.to_string(),
            });
    }

    fn unavailable_tool_result(&self, call: ToolCall) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Tool '{}' is not available for this agent", call.name).into(),
            is_error: true,
        }
    }

    fn blocked_tool_result(&self, call: &ToolCall, error: RuntimeError) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: format!("Tool execution blocked: {error}").into(),
            is_error: true,
        }
    }

    fn blocked_authorization_result(
        &self,
        call: &ToolCall,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    ) -> ContentBlock {
        let content = match outcome {
            ToolAuthorizationOutcome::Allow => "Tool execution blocked by authorizer".to_string(),
            ToolAuthorizationOutcome::Prompt => reason
                .map(|reason| format!("Tool execution requires approval: {reason}"))
                .unwrap_or_else(|| "Tool execution requires approval".to_string()),
            ToolAuthorizationOutcome::Deny => reason
                .map(|reason| format!("Tool execution denied: {reason}"))
                .unwrap_or_else(|| "Tool execution denied by authorizer".to_string()),
        };

        ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: content.into(),
            is_error: true,
        }
    }

    /// Splits a structured tool outcome into its provider-visible
    /// projection, opaque host metadata, and requested termination — the
    /// single boundary where `details` is separated from what a provider
    /// ever sees (only `content` reaches `ContentBlock::ToolResult`).
    fn tool_output_block(
        call: &ToolCall,
        output: Result<crate::tool::ToolOutput, String>,
    ) -> (ContentBlock, Option<serde_json::Value>, bool) {
        match output {
            Ok(output) => (
                ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: output.content,
                    is_error: false,
                },
                output.details,
                output.terminate,
            ),
            Err(content) => (
                ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: content.into(),
                    is_error: true,
                },
                None,
                false,
            ),
        }
    }

    fn completed_execution(
        &self,
        agent: &Agent,
        call: &ToolCall,
        descriptor: &RuntimeToolDescriptor,
        result: ContentBlock,
        effect: RoundEffect,
        details: Option<serde_json::Value>,
    ) -> CompletedToolExecution {
        self.emit_tool_runtime_finished(call, &result, details);
        agent.emit_event(AgentEvent::ToolExecutionFinished {
            result: result.clone(),
        });
        let task_succeeded = matches!(
            &result,
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ) && descriptor
            .capabilities
            .iter()
            .any(|capability| matches!(capability, ToolCapability::TaskMutation));

        CompletedToolExecution {
            result,
            task_succeeded,
            should_end_turn: effect.should_end_turn,
            terminated: effect.terminated,
            tool_name: call.name.clone(),
        }
    }

    fn working_directory(&mut self) -> std::path::PathBuf {
        if let Some(path) = &self.working_directory {
            return path.clone();
        }

        let path = self
            .runtime
            .resolve_working_directory(&self.agent_id, None)
            .unwrap_or_else(|_| self.runtime.default_working_directory(&self.agent_id));
        self.working_directory = Some(path.clone());
        path
    }

    fn parallel_tool_context(&mut self, agent: &Agent, call: &ToolCall) -> ParallelToolContext {
        ParallelToolContext {
            agent_id: self.agent_id.clone(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            working_directory: self.working_directory(),
            runtime: self.runtime.clone(),
            subagent_template: agent.disposable_subagent_template(),
            agent_name: agent.name().to_string(),
            model: agent.model().to_string(),
            history_len: agent.history().len(),
            tasks: agent.tasks().to_vec(),
            event_tx: agent.event_sender(),
        }
    }

    fn registered_tool(
        &self,
        name: &str,
    ) -> Option<(Arc<dyn ExecutableTool>, RuntimeToolDescriptor)> {
        let tool = self.runtime.get_tool(name)?;
        let descriptor = self.runtime.get_tool_descriptor(name)?;
        Some((tool, descriptor))
    }

    async fn authorize_tool_call(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn ExecutableTool>,
        ctx: &ParallelToolContext,
    ) -> Result<Option<ContentBlock>, RuntimeError> {
        let Some(authorizer) = self.runtime.execution.tool_authorizer.clone() else {
            return Ok(None);
        };

        let preview = match tool.authorization_preview(ctx, &call.input) {
            Ok(preview) => preview,
            Err(error) => {
                return Ok(Some(self.blocked_authorization_result(
                    call,
                    ToolAuthorizationOutcome::Deny,
                    Some(error),
                )));
            }
        };

        self.emit_tool_authorization_started(call, preview.clone())?;
        let request = ToolAuthorizationRequest {
            agent_id: self.agent_id.clone(),
            agent_name: ctx.agent_name().to_string(),
            model: ctx.model().to_string(),
            history_len: ctx.history_len(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            preview,
        };

        let result = match authorizer.timeout() {
            Some(timeout) => {
                match tokio::time::timeout(timeout, authorizer.authorize(&request)).await {
                    Ok(result) => result,
                    Err(_) => {
                        return self.handle_authorization_block(
                            call,
                            ToolAuthorizationOutcome::Deny,
                            Some(format!(
                                "authorizer timed out after {}",
                                format_duration(timeout)
                            )),
                        );
                    }
                }
            }
            None => authorizer.authorize(&request).await,
        };

        match result {
            Ok(decision) => match decision.outcome {
                ToolAuthorizationOutcome::Allow => {
                    self.emit_tool_authorization_finished(call, decision.outcome, decision.reason)?;
                    Ok(None)
                }
                outcome => self.handle_authorization_block(call, outcome, decision.reason),
            },
            Err(error) => self.handle_authorization_block(
                call,
                ToolAuthorizationOutcome::Deny,
                Some(error.to_string()),
            ),
        }
    }

    fn handle_authorization_block(
        &self,
        call: &ToolCall,
        outcome: ToolAuthorizationOutcome,
        reason: Option<String>,
    ) -> Result<Option<ContentBlock>, RuntimeError> {
        self.emit_tool_authorization_finished(call, outcome, reason.clone())?;
        self.emit_tool_authorization_blocked(call, outcome, reason.clone())?;
        Ok(Some(
            self.blocked_authorization_result(call, outcome, reason),
        ))
    }

    async fn execute_one_tool(
        &mut self,
        agent: &mut Agent,
        call: ToolCall,
    ) -> Result<CompletedToolExecution, RuntimeError> {
        self.note_tool_started(agent, &call)?;
        if !agent.can_use_tool(&call.name) {
            let result = self.unavailable_tool_result(call.clone());
            agent.emit_event(AgentEvent::ToolExecutionFinished {
                result: result.clone(),
            });
            return Ok(CompletedToolExecution {
                result,
                task_succeeded: false,
                should_end_turn: false,
                terminated: false,
                tool_name: call.name.clone(),
            });
        }

        Ok(self.execute_registered_tool(agent, call).await)
    }

    async fn execute_parallel_batch(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        calls: Vec<ToolCall>,
    ) -> Result<Vec<CompletedToolExecution>, RuntimeError> {
        let len = calls.len();
        let mut results = (0..len).map(|_| None).collect::<Vec<_>>();
        let mut join_set = JoinSet::new();

        for (index, call) in calls.iter().cloned().enumerate() {
            if let Err(error) = self.note_tool_started(agent, &call) {
                join_set.abort_all();
                return Err(error);
            }

            let Some((tool, descriptor)) = self.registered_tool(&call.name) else {
                let result = ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: "Tool not found".into(),
                    is_error: true,
                };
                agent.emit_event(AgentEvent::ToolExecutionFinished {
                    result: result.clone(),
                });
                results[index] = Some(CompletedToolExecution {
                    result,
                    task_succeeded: false,
                    should_end_turn: false,
                    terminated: false,
                    tool_name: call.name.clone(),
                });
                continue;
            };

            let ctx = self.parallel_tool_context(agent, &call);
            if let Some(result) = self.authorize_tool_call(&call, &tool, &ctx).await? {
                let execution = self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
                results[index] = Some(execution);
                continue;
            }

            // Pre-execution hook check
            match self.run_pre_hooks(&call)? {
                HookDecision::Allow => {}
                HookDecision::Deny(reason) => {
                    self.emit_tool_execution_blocked(&call, &reason);
                    let result = ContentBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        content: format!("Blocked by pre-execution hook: {reason}").into(),
                        is_error: true,
                    };
                    let execution = self.completed_execution(
                        agent,
                        &call,
                        &descriptor,
                        result,
                        RoundEffect::default(),
                        None,
                    );
                    results[index] = Some(execution);
                    continue;
                }
            }

            if let Err(error) = self.emit_tool_runtime_started(&call) {
                let result = self.blocked_tool_result(&call, error);
                let execution = self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
                results[index] = Some(execution);
                continue;
            }

            join_set.spawn(async move {
                let output = execute_tool_future(
                    &call.name,
                    descriptor.execution_timeout,
                    tool.execute_output(ctx, call.input.clone()),
                )
                .await;
                (index, call, descriptor, output)
            });
        }

        while !join_set.is_empty() {
            if let Err(error) = options.check_limits() {
                join_set.abort_all();
                return Err(error);
            }
            match tokio::time::timeout(PARALLEL_JOIN_POLL_INTERVAL, join_set.join_next()).await {
                Ok(Some(Ok((index, call, descriptor, output)))) => {
                    let (result, details, terminate) = Self::tool_output_block(&call, output);
                    // RUNTIME defense: a parallel-lane execution can never end
                    // the run — a `terminate: true` surfacing here is a tool
                    // misuse (or a static-coercion gap), never honored as
                    // termination, and never a silent race with the rest of
                    // the batch.
                    let (result, details) = if terminate {
                        eprintln!(
                            "warning: tool '{}' requested termination from a parallel \
                             execution; rejecting as a misuse error, run continues",
                            call.name
                        );
                        (parallel_termination_rejected(&call), None)
                    } else {
                        (result, details)
                    };
                    results[index] = Some(self.completed_execution(
                        agent,
                        &call,
                        &descriptor,
                        result,
                        RoundEffect::default(),
                        details,
                    ));
                }
                Ok(Some(Err(error))) => {
                    join_set.abort_all();
                    return Err(RuntimeError::Store(format!(
                        "parallel tool task failed: {error}"
                    )));
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }

        if let Err(error) = options.check_limits() {
            join_set.abort_all();
            return Err(error);
        }

        let mut ordered = Vec::with_capacity(len);
        for result in results {
            ordered.push(result.ok_or_else(|| {
                RuntimeError::Store("parallel tool batch lost a result".to_string())
            })?);
        }

        Ok(ordered)
    }

    async fn execute_registered_tool(
        &mut self,
        agent: &mut Agent,
        call: ToolCall,
    ) -> CompletedToolExecution {
        let Some((tool, descriptor)) = self.registered_tool(&call.name) else {
            let result = ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: "Tool not found".into(),
                is_error: true,
            };
            agent.emit_event(AgentEvent::ToolExecutionFinished {
                result: result.clone(),
            });
            return CompletedToolExecution {
                result,
                task_succeeded: false,
                should_end_turn: false,
                terminated: false,
                tool_name: call.name.clone(),
            };
        };

        let authorization_ctx = self.parallel_tool_context(agent, &call);
        match self
            .authorize_tool_call(&call, &tool, &authorization_ctx)
            .await
        {
            Ok(Some(result)) => {
                return self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
            }
            Ok(None) => {}
            Err(error) => {
                let result = self.blocked_tool_result(&call, error);
                return self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
            }
        }

        // Pre-execution hook check
        match self.run_pre_hooks(&call) {
            Ok(HookDecision::Allow) => {}
            Ok(HookDecision::Deny(reason)) => {
                self.emit_tool_execution_blocked(&call, &reason);
                let result = ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: format!("Blocked by pre-execution hook: {reason}").into(),
                    is_error: true,
                };
                return self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
            }
            Err(error) => {
                let result = self.blocked_tool_result(&call, error);
                return self.completed_execution(
                    agent,
                    &call,
                    &descriptor,
                    result,
                    RoundEffect::default(),
                    None,
                );
            }
        }

        if let Err(error) = self.emit_tool_runtime_started(&call) {
            let result = self.blocked_tool_result(&call, error);
            return self.completed_execution(
                agent,
                &call,
                &descriptor,
                result,
                RoundEffect::default(),
                None,
            );
        }

        let working_directory = authorization_ctx.working_directory.clone();
        let runtime = authorization_ctx.runtime.clone();
        let event_tx = agent.event_sender();
        let (result, details, terminate) = Self::tool_output_block(
            &call,
            execute_tool_future(
                &call.name,
                descriptor.execution_timeout,
                tool.execute_mut_output(
                    ToolContext {
                        agent_id: self.agent_id.clone(),
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        working_directory,
                        runtime,
                        agent,
                        event_tx,
                    },
                    call.input.clone(),
                ),
            )
            .await,
        );
        let effect = RoundEffect {
            should_end_turn: agent.take_idle_requested() || terminate,
            terminated: terminate,
        };
        self.completed_execution(agent, &call, &descriptor, result, effect, details)
    }
}

impl ToolCallSchedule {
    fn new(runtime: &ToolRuntime, agent: &Agent, calls: Vec<ToolCall>) -> Self {
        let mut batches = Vec::new();
        let mut pending_parallel = Vec::new();

        for call in calls {
            match runtime.call_execution_category_for_agent(&call, Some(agent)) {
                ToolExecutionCategory::ReadOnlyParallel => pending_parallel.push(call),
                ToolExecutionCategory::ExclusiveLocalMutation
                | ToolExecutionCategory::ExclusivePersistentMutation
                | ToolExecutionCategory::BackgroundJob
                | ToolExecutionCategory::Delegation => {
                    if !pending_parallel.is_empty() {
                        batches.push(ToolCallBatch::Parallel(std::mem::take(
                            &mut pending_parallel,
                        )));
                    }
                    batches.push(ToolCallBatch::Exclusive(call));
                }
            }
        }

        if !pending_parallel.is_empty() {
            batches.push(ToolCallBatch::Parallel(pending_parallel));
        }

        Self { batches }
    }
}

impl ToolCallBatch {
    fn execution_count(&self) -> usize {
        match self {
            ToolCallBatch::Exclusive(_) => 1,
            ToolCallBatch::Parallel(calls) => calls.len(),
        }
    }

    /// Unwraps this batch into its constituent calls, in original call order.
    /// Used to build not-executed results for batches skipped by termination.
    fn into_calls(self) -> Vec<ToolCall> {
        match self {
            ToolCallBatch::Exclusive(call) => vec![call],
            ToolCallBatch::Parallel(calls) => calls,
        }
    }
}

/// Builds the is_error result for a call that was never executed because an
/// earlier call in the same round terminated the run.
fn not_executed_result(call: &ToolCall, terminated_by: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: call.id.clone(),
        content: format!("not executed: run terminated by '{terminated_by}'").into(),
        is_error: true,
    }
}

/// Builds the is_error result for a parallel-lane call that requested
/// termination — RUNTIME defense: never honored, always surfaced as misuse.
fn parallel_termination_rejected(call: &ToolCall) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: call.id.clone(),
        content: format!(
            "not honored: tool '{}' requested termination from a parallel execution; \
             termination is only honored from an exclusive execution",
            call.name
        )
        .into(),
        is_error: true,
    }
}

async fn execute_tool_future<F, T>(
    tool_name: &str,
    execution_timeout: Option<Duration>,
    future: F,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    match execution_timeout {
        Some(timeout) => match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "Tool '{tool_name}' timed out after {}",
                format_duration(timeout)
            )),
        },
        None => future.await,
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 && duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else if duration.as_millis() > 0 {
        format!("{}ms", duration.as_millis())
    } else if duration.as_micros() > 0 {
        format!("{}us", duration.as_micros())
    } else {
        format!("{}ns", duration.as_nanos())
    }
}
