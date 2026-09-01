//! Tool orchestration pipeline for scheduling, authorization, execution, and result ordering.

use std::{collections::BTreeMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use tokio::task::JoinSet;

use crate::{
    ContentBlock,
    agent::{Agent, AgentEvent, AgentStatus},
    error::RuntimeError,
    runtime::control::{HookDecision, PostExecutionContext, PreExecutionContext, ResultDecision},
    runtime::{RunOptions, RuntimeHookEvent},
    tool::{
        ExecutableTool, ParallelToolContext, ResolvedTool, RuntimeToolDescriptor,
        ToolAuthorizationOutcome, ToolAuthorizationRequest, ToolCall, ToolCapability, ToolContext,
        ToolExecutionCategory,
    },
};

use super::{
    paging::{READ_TOOL_RESULT_TOOL, ToolResultPager},
    truncation::{SpillBehavior, ToolOutputLimiter},
};

const PARALLEL_JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct ToolExecutionOutcome {
    pub(crate) results: Vec<ContentBlock>,
    pub(crate) successful_task: bool,
    pub(crate) end_turn: bool,
    /// Per-call opaque metadata collected from this round's executions,
    /// keyed by `tool_use_id` — the runner attaches this to the appended
    /// transcript item so it survives persistence and replay, never
    /// projected to a provider (ADR-0001 §4).
    pub(crate) details: BTreeMap<String, serde_json::Value>,
}

pub(crate) struct ToolRuntime {
    runtime: crate::runtime::handle::RuntimeHandle,
    agent_id: String,
    tool_calls: usize,
    working_directory: Option<PathBuf>,
    output_limiter: ToolOutputLimiter,
    /// `Some` only when this agent enables tool-result paging; `None` leaves
    /// every result exactly as the limiter produced it.
    pager: Option<ToolResultPager>,
}

#[derive(Clone)]
enum ToolCallBatch {
    Exclusive(Box<ScheduledToolCall>),
    Parallel(Vec<ScheduledToolCall>),
}

struct ToolCallSchedule {
    batches: Vec<ToolCallBatch>,
}

#[derive(Clone)]
struct ScheduledToolCall {
    call: ToolCall,
    tool: ScheduledTool,
    execution_category: ToolExecutionCategory,
}

#[derive(Clone)]
enum ScheduledTool {
    Resolved(Box<ResolvedTool>),
    Unavailable,
    Missing,
}

/// What the pre-execution hooks left of a call.
enum HookOutcome {
    /// Run it; `modified` records whether a hook rewrote the input, because a
    /// shape error in a rewritten input is the hook's to answer for.
    Proceed { modified: bool },
    /// Do not run it, and tell the model why.
    Refused(String),
}

/// Whether a scheduled call made it through hooks, schema check, and
/// authorization.
///
/// Both payloads are boxed: each is hundreds of bytes and they differ enough
/// that an unboxed enum pays the larger one's size on every admission.
enum Admission {
    /// Execute with this context.
    Run(Box<ParallelToolContext>),
    /// Already answered; nothing runs.
    Refused(Box<CompletedToolExecution>),
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
    /// The input the tool actually ran with, as JSON — after any pre-execution
    /// hook rewrote it, so a post-execution hook judges what happened rather
    /// than what was asked for.
    input_json: String,
    /// This execution's opaque `ToolOutput::details`, if any — collected by
    /// [`ToolRuntime::execute_calls`] into [`ToolExecutionOutcome::details`].
    details: Option<serde_json::Value>,
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
        let runtime = agent.runtime_handle();
        let policy = &runtime.execution.policy;
        let spill = if !policy.spill_full_tool_output {
            SpillBehavior::Disabled("spill-to-file is disabled by runtime policy")
        } else if !runtime.persistence.store.allows_disk_artifacts() {
            SpillBehavior::Disabled("the runtime store forbids durable artifacts")
        } else {
            SpillBehavior::Enabled(agent.config().compaction.transcript_dir.join("tool-output"))
        };
        let output_limiter = ToolOutputLimiter::new(
            policy.max_tool_result_bytes,
            policy.max_tool_result_lines,
            spill,
        );
        Self {
            runtime,
            agent_id: agent.id().to_string(),
            tool_calls: 0,
            working_directory: None,
            output_limiter,
            pager: agent.config().tool_result_paging.map(ToolResultPager::new),
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
        let mut details = BTreeMap::new();

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
                ToolCallBatch::Exclusive(call) => {
                    vec![self.execute_one_tool(agent, options, *call).await?]
                }
                ToolCallBatch::Parallel(calls) => {
                    self.execute_parallel_batch(agent, options, calls).await?
                }
            };

            let mut terminator = None;
            for execution in executions {
                successful_task |= execution.task_succeeded;
                end_turn |= execution.should_end_turn;
                let reviewed = self
                    .run_post_hooks(
                        &execution.tool_name,
                        &execution.input_json,
                        execution.result,
                    )
                    .await?;
                let result = self.page_result(agent, &execution.tool_name, reviewed);
                if execution.terminated {
                    terminator.get_or_insert(execution.tool_name);
                }
                if let (Some(value), ContentBlock::ToolResult { tool_use_id, .. }) =
                    (execution.details, &result)
                {
                    details.insert(tool_use_id.clone(), value);
                }
                results.push(result);
            }

            // A terminating call ends the round as the value of its own
            // execution; calls already scheduled for later batches in this
            // round are never executed. Each still gets an explicit
            // is_error result so the transcript always has one result block
            // per tool_use — never a silent drop.
            if let Some(terminator) = terminator {
                for remaining_batch in batches {
                    for call in remaining_batch.into_calls() {
                        let result = not_executed_result(&call, &terminator);
                        results.push(self.page_result(agent, &call.name, result));
                    }
                }
                break;
            }
        }

        Ok(ToolExecutionOutcome {
            results,
            successful_task,
            end_turn,
            details,
        })
    }

    /// Replaces an oversized text result with its first window, retaining the
    /// full text on the agent for `read_tool_result` to serve.
    ///
    /// This is the single point where a result becomes the *model's* view of
    /// itself: every `AgentEvent::ToolExecutionFinished` has already been
    /// emitted with the complete block by the time a result reaches here, so
    /// consumers reconstructing evidence from the event stream observe no
    /// change at all. Applied to every block that joins the round's committed
    /// message — including the fixed not-executed and not-found results — so
    /// no path into the transcript bypasses the bound.
    fn page_result(&self, agent: &Agent, tool_name: &str, result: ContentBlock) -> ContentBlock {
        let Some(pager) = self.pager else {
            return result;
        };
        // A window returned by `read_tool_result` is bounded by construction;
        // paging it again would nest a trailer inside a trailer.
        if tool_name == READ_TOOL_RESULT_TOOL {
            return result;
        }
        let ContentBlock::ToolResult {
            tool_use_id,
            content: mentra_provider::ToolResultContent::Text(text),
            is_error,
        } = result
        else {
            return result;
        };

        let Some(page) = pager.first_page(&tool_use_id, &text) else {
            return ContentBlock::ToolResult {
                tool_use_id,
                content: mentra_provider::ToolResultContent::Text(text),
                is_error,
            };
        };
        agent.record_paged_tool_result(&tool_use_id, &text);
        ContentBlock::ToolResult {
            tool_use_id,
            content: mentra_provider::ToolResultContent::Text(page),
            is_error,
        }
    }

    fn schedule_call(&self, agent: &Agent, call: ToolCall) -> ScheduledToolCall {
        let tool = match agent.resolve_tool(&call.name) {
            crate::tool::ToolResolution::Visible(tool) => tool,
            crate::tool::ToolResolution::Hidden => {
                return ScheduledToolCall {
                    call,
                    tool: ScheduledTool::Unavailable,
                    execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
                };
            }
            crate::tool::ToolResolution::Missing => {
                return ScheduledToolCall {
                    call,
                    tool: ScheduledTool::Missing,
                    execution_category: ToolExecutionCategory::ExclusiveLocalMutation,
                };
            }
        };

        let (declared, scheduled) =
            Self::execution_categories_for_snapshot(&call, &tool.handler, tool.descriptor());
        if scheduled != declared {
            eprintln!(
                "warning: tool '{}' is marked terminal but declared a parallel \
                 execution category; coercing to exclusive scheduling",
                call.name
            );
        }

        ScheduledToolCall {
            call,
            tool: ScheduledTool::Resolved(tool),
            execution_category: scheduled,
        }
    }

    fn execution_categories_for_snapshot(
        call: &ToolCall,
        tool: &Arc<dyn ExecutableTool>,
        descriptor: &RuntimeToolDescriptor,
    ) -> (ToolExecutionCategory, ToolExecutionCategory) {
        let declared = tool.execution_category(&call.input);
        let scheduled = if descriptor.terminal && declared.allows_parallel() {
            ToolExecutionCategory::ExclusiveLocalMutation
        } else {
            declared
        };
        (declared, scheduled)
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

    async fn run_pre_hooks(&mut self, call: &ToolCall) -> Result<HookDecision, RuntimeError> {
        let context = PreExecutionContext {
            agent_id: self.agent_id.clone(),
            tool_name: call.name.clone(),
            tool_call_id: call.id.clone(),
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
            working_directory: self.working_directory(),
        };
        self.runtime.pre_hooks().run(&context).await
    }

    /// Offers a finished result to the post-execution hooks and applies what
    /// they decided.
    ///
    /// Runs before [`page_result`](Self::page_result), so a hook sees the whole
    /// result rather than its first window, and whatever it returns is still
    /// bounded by the pager afterwards — a hook cannot enlarge a result past
    /// the limit the runtime set.
    ///
    /// Only genuine executions reach here. A call that was never run — blocked
    /// by a terminating sibling, or refused before it started — has no output
    /// for a *post-execution* hook to judge.
    async fn run_post_hooks(
        &mut self,
        tool_name: &str,
        input_json: &str,
        result: ContentBlock,
    ) -> Result<ContentBlock, RuntimeError> {
        if self.runtime.post_hooks().is_empty() {
            return Ok(result);
        }

        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = result
        else {
            return Ok(result);
        };

        let context = PostExecutionContext {
            agent_id: self.agent_id.clone(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_use_id.clone(),
            input_json: input_json.to_string(),
            working_directory: self.working_directory(),
            content,
            is_error,
        };

        Ok(match self.runtime.post_hooks().run(&context).await? {
            ResultDecision::Keep => ContentBlock::ToolResult {
                tool_use_id,
                content: context.content,
                is_error: context.is_error,
            },
            ResultDecision::Replace { content, is_error } => ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        })
    }

    /// Runs the pre-execution hooks and applies whatever they decided.
    ///
    /// `Ok(HookOutcome::Proceed { .. })` means proceed — with `call.input`
    /// rewritten by a hook when `modified` is set. `Ok(HookOutcome::Refused)`
    /// means the call must not run and the reason is what the model should be
    /// told.
    ///
    /// Shared by the serial and parallel paths so the two cannot disagree
    /// about what a hook's answer means.
    async fn apply_pre_hooks(&mut self, call: &mut ToolCall) -> Result<HookOutcome, RuntimeError> {
        match self.run_pre_hooks(call).await? {
            HookDecision::Allow => Ok(HookOutcome::Proceed { modified: false }),
            HookDecision::Deny(reason) => Ok(HookOutcome::Refused(reason)),
            HookDecision::Modify { input_json, .. } => {
                // A hook that rewrites the input but hands back something that
                // is not JSON has failed at its own job. Refusing is the safe
                // reading: running the *original* would silently ignore a hook
                // that believed it had intervened.
                match serde_json::from_str(&input_json) {
                    Ok(input) => {
                        call.input = input;
                        Ok(HookOutcome::Proceed { modified: true })
                    }
                    Err(error) => Ok(HookOutcome::Refused(format!(
                        "pre-execution hook returned invalid JSON for '{}': {error}",
                        call.name
                    ))),
                }
            }
        }
    }

    /// Everything that stands between a scheduled call and its execution, in
    /// the one order both lanes share: pre-execution hooks, then the schema
    /// check, then the [`ToolAuthorizer`](crate::tool::ToolAuthorizer).
    ///
    /// Hooks run first so that the input the authorizer is asked about is the
    /// input the tool will run with. A remembered permission rule is matched
    /// against the serialized input, and a rule that matched a call the hook
    /// then rewrote would describe an execution that never happened. The
    /// schema check sits between them because a hook can rewrite into the
    /// wrong shape as easily as a model can produce it, and a call that does
    /// not fit its own schema should be corrected, not put to a person.
    ///
    /// Returning `Err` means a hook or the authorizer itself failed; each lane
    /// decides what a failure costs (the parallel lane aborts the batch, the
    /// serial lane answers the one call), which is why this does not.
    async fn admit_call(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        call: &mut ToolCall,
        tool: &Arc<dyn ExecutableTool>,
        descriptor: &RuntimeToolDescriptor,
        execution_category: ToolExecutionCategory,
    ) -> Result<Admission, RuntimeError> {
        let modified = match self.apply_pre_hooks(call).await? {
            HookOutcome::Proceed { modified } => modified,
            HookOutcome::Refused(reason) => {
                return Ok(Admission::Refused(Box::new(
                    self.hook_blocked_execution(agent, call, descriptor, &reason),
                )));
            }
        };

        let authorization_category = if modified {
            let (_, rewritten_category) =
                Self::execution_categories_for_snapshot(call, tool, descriptor);
            if execution_category.allows_parallel() && !rewritten_category.allows_parallel() {
                let reason = format!(
                    "pre-execution hook changed '{}' from a parallel call into {:?}; refusing to \
                     run mutating work in the parallel lane",
                    call.name, rewritten_category
                );
                return Ok(Admission::Refused(Box::new(
                    self.hook_blocked_execution(agent, call, descriptor, &reason),
                )));
            }
            rewritten_category
        } else {
            execution_category
        };

        if let Some(error) = self.schema_violation(call, descriptor) {
            // The model is told what to fix when the shape is its own. When a
            // hook produced the shape, blaming the model would send it
            // correcting an input it never wrote; the host component is the
            // one that failed, and the record says so.
            return Ok(Admission::Refused(Box::new(if modified {
                let reason = format!(
                    "pre-execution hook rewrote '{}' into input that does not fit its schema: \
                     {error}",
                    call.name
                );
                self.hook_blocked_execution(agent, call, descriptor, &reason)
            } else {
                self.schema_violation_execution(agent, call, error)
            })));
        }

        let ctx = self.parallel_tool_context(agent, options, call);
        if let Some(result) = self
            .authorize_tool_call(call, tool, &ctx, authorization_category)
            .await?
        {
            let execution = self.completed_execution(
                agent,
                call,
                descriptor,
                result,
                RoundEffect::default(),
                None,
            );
            return Ok(Admission::Refused(Box::new(execution)));
        }

        Ok(Admission::Run(Box::new(ctx)))
    }

    fn hook_blocked_execution(
        &self,
        agent: &Agent,
        call: &ToolCall,
        descriptor: &RuntimeToolDescriptor,
        reason: &str,
    ) -> CompletedToolExecution {
        self.emit_tool_execution_blocked(call, reason);
        let result = ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: format!("Blocked by pre-execution hook: {reason}").into(),
            is_error: true,
        };
        self.completed_execution(
            agent,
            call,
            descriptor,
            result,
            RoundEffect::default(),
            None,
        )
    }

    /// The answer to a call whose shape the model itself got wrong: an error
    /// result naming the field, and nothing else — a call that never ran has
    /// no runtime-level finish to report.
    fn schema_violation_execution(
        &self,
        agent: &Agent,
        call: &ToolCall,
        error: String,
    ) -> CompletedToolExecution {
        let result = ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: format!("Invalid input for '{}': {error}", call.name).into(),
            is_error: true,
        };
        agent.emit_event(AgentEvent::ToolExecutionFinished {
            result: result.clone(),
        });
        CompletedToolExecution {
            result,
            task_succeeded: false,
            should_end_turn: false,
            terminated: false,
            tool_name: call.name.clone(),
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
            details: None,
        }
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

    /// Checks a call against the schema its tool published, returning what
    /// is wrong when it does not fit.
    ///
    /// A tool advertises an `input_schema` to the model and nothing compared a
    /// call against it, so a missing required field or a string where a number
    /// belonged reached the tool's own code — where it became a confusing
    /// deserialization error, or worse, was read loosely and did the wrong
    /// thing quietly. Answering here gives whoever produced the input the one
    /// thing they can act on: which field, and what was expected.
    fn schema_violation(
        &self,
        call: &ToolCall,
        descriptor: &RuntimeToolDescriptor,
    ) -> Option<String> {
        // A terminal tool's result *is* the turn's value, and it validates that
        // value itself with a message about the requested type. Rejecting the
        // call here instead would replace a turn that fails cleanly with one
        // that asks the model to try again -- forever, for a model that keeps
        // producing the same wrong shape.
        if descriptor.terminal {
            return None;
        }

        crate::tool::schema::validate_tool_input(&descriptor.provider.input_schema, &call.input)
            .err()
            .map(|error| error.to_string())
    }

    fn unavailable_tool_result(&self, call: ToolCall) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Tool '{}' is not available for this agent", call.name).into(),
            is_error: true,
        }
    }

    fn unavailable_tool_execution(&self, agent: &Agent, call: ToolCall) -> CompletedToolExecution {
        let result = self.unavailable_tool_result(call.clone());
        agent.emit_event(AgentEvent::ToolExecutionFinished {
            result: result.clone(),
        });
        CompletedToolExecution {
            result,
            task_succeeded: false,
            should_end_turn: false,
            terminated: false,
            tool_name: call.name,
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
            details: None,
        }
    }

    fn missing_tool_execution(&self, agent: &Agent, call: ToolCall) -> CompletedToolExecution {
        let result = ContentBlock::ToolResult {
            tool_use_id: call.id.clone(),
            content: "Tool not found".into(),
            is_error: true,
        };
        agent.emit_event(AgentEvent::ToolExecutionFinished {
            result: result.clone(),
        });
        CompletedToolExecution {
            result,
            task_succeeded: false,
            should_end_turn: false,
            terminated: false,
            tool_name: call.name,
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
            details: None,
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
    async fn tool_output_block(
        &self,
        call: &ToolCall,
        output: Result<crate::tool::ToolOutput, String>,
    ) -> (ContentBlock, Option<serde_json::Value>, bool) {
        match output {
            Ok(output) => (
                ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: self.output_limiter.apply(output.content).await,
                    is_error: false,
                },
                output.details,
                output.terminate,
            ),
            Err(content) => (
                ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: self
                        .output_limiter
                        .apply(mentra_provider::ToolResultContent::Text(content))
                        .await,
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
        self.emit_tool_runtime_finished(call, &result, details.clone());
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
            input_json: serde_json::to_string(&call.input).unwrap_or_default(),
            details,
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

    fn parallel_tool_context(
        &mut self,
        agent: &Agent,
        options: &RunOptions,
        call: &ToolCall,
    ) -> ParallelToolContext {
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
            run_options: options.clone(),
        }
    }

    async fn authorize_tool_call(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn ExecutableTool>,
        ctx: &ParallelToolContext,
        execution_category: ToolExecutionCategory,
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

        // A preview reports whichever category its builder chose, and every
        // builder in the tree copies the tool's *static* declaration. The
        // scheduler does not: it asks the tool with this call's input and then
        // applies the terminal coercion. Answering the authorizer with the
        // scheduler's answer is what makes the classification describe the call
        // that will actually run, for every tool at once rather than for
        // whichever preview builders remember to do it.
        let preview = crate::tool::ToolAuthorizationPreview {
            execution_category,
            ..preview
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
        options: &RunOptions,
        scheduled: ScheduledToolCall,
    ) -> Result<CompletedToolExecution, RuntimeError> {
        let ScheduledToolCall {
            call,
            tool,
            execution_category,
        } = scheduled;
        self.note_tool_started(agent, &call)?;
        match tool {
            ScheduledTool::Unavailable => Ok(self.unavailable_tool_execution(agent, call)),
            ScheduledTool::Missing => Ok(self.missing_tool_execution(agent, call)),
            ScheduledTool::Resolved(tool) => Ok(self
                .execute_registered_tool(agent, options, call, *tool, execution_category)
                .await),
        }
    }

    async fn execute_parallel_batch(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        calls: Vec<ScheduledToolCall>,
    ) -> Result<Vec<CompletedToolExecution>, RuntimeError> {
        let len = calls.len();
        let mut results = (0..len).map(|_| None).collect::<Vec<_>>();
        let mut join_set = JoinSet::new();

        for (index, scheduled) in calls.into_iter().enumerate() {
            let ScheduledToolCall {
                mut call,
                tool,
                execution_category,
            } = scheduled;
            if let Err(error) = self.note_tool_started(agent, &call) {
                join_set.abort_all();
                return Err(error);
            }

            let resolved = match tool {
                ScheduledTool::Resolved(tool) => *tool,
                ScheduledTool::Unavailable => {
                    results[index] = Some(self.unavailable_tool_execution(agent, call));
                    continue;
                }
                ScheduledTool::Missing => {
                    results[index] = Some(self.missing_tool_execution(agent, call));
                    continue;
                }
            };
            let descriptor = resolved.descriptor().clone();
            let tool = resolved.handler;

            let ctx = match self
                .admit_call(
                    agent,
                    options,
                    &mut call,
                    &tool,
                    &descriptor,
                    execution_category,
                )
                .await?
            {
                Admission::Run(ctx) => *ctx,
                Admission::Refused(execution) => {
                    results[index] = Some(*execution);
                    continue;
                }
            };

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
                    let (result, details, terminate) = self.tool_output_block(&call, output).await;
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
        options: &RunOptions,
        mut call: ToolCall,
        resolved: ResolvedTool,
        execution_category: ToolExecutionCategory,
    ) -> CompletedToolExecution {
        let descriptor = resolved.descriptor().clone();
        let tool = resolved.handler;

        let authorization_ctx = match self
            .admit_call(
                agent,
                options,
                &mut call,
                &tool,
                &descriptor,
                execution_category,
            )
            .await
        {
            Ok(Admission::Run(ctx)) => *ctx,
            Ok(Admission::Refused(execution)) => return *execution,
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
        };

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
        let (result, details, terminate) = self
            .tool_output_block(
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
                            run_options: options.clone(),
                        },
                        call.input.clone(),
                    ),
                )
                .await,
            )
            .await;
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
            let scheduled = runtime.schedule_call(agent, call);
            match scheduled.execution_category {
                ToolExecutionCategory::ReadOnlyParallel => pending_parallel.push(scheduled),
                ToolExecutionCategory::ExclusiveLocalMutation
                | ToolExecutionCategory::ExclusivePersistentMutation
                | ToolExecutionCategory::BackgroundJob
                | ToolExecutionCategory::Delegation => {
                    if !pending_parallel.is_empty() {
                        batches.push(ToolCallBatch::Parallel(std::mem::take(
                            &mut pending_parallel,
                        )));
                    }
                    batches.push(ToolCallBatch::Exclusive(Box::new(scheduled)));
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
            ToolCallBatch::Exclusive(call) => vec![call.call],
            ToolCallBatch::Parallel(calls) => {
                calls.into_iter().map(|scheduled| scheduled.call).collect()
            }
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
