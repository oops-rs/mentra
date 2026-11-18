use std::{
    future::{Future, pending, poll_fn},
    pin::pin,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use serde_json::Value;

use crate::{
    ContentBlock,
    agent::{Agent, AgentStatus},
    error::RuntimeError,
    runtime::{RunOptions, next_id},
    tool::{ReviewedToolResult, ToolCall},
    transcript::{ChildToolEvent, TranscriptItem},
};

use super::{
    PARALLEL_JOIN_POLL_INTERVAL, RoundEffect, ScheduledToolCall, ToolCallBatch, ToolRuntime,
};

pub(in crate::tool) struct ActiveToolCall<'a> {
    pub(in crate::tool) call: &'a ToolCall,
    pub(in crate::tool) parent: Option<&'a Self>,
}

impl ActiveToolCall<'_> {
    fn contains(&self, name: &str) -> bool {
        let mut active = Some(self);
        while let Some(call) = active {
            if call.call.name == name {
                return true;
            }
            active = call.parent;
        }
        false
    }
}

/// The live dispatcher borrowed by an exclusive tool invocation.
pub(crate) struct ChildDispatch<'a> {
    runtime: &'a mut ToolRuntime,
    active: &'a ActiveToolCall<'a>,
    control: &'a CompositionControl,
}

impl<'a> ChildDispatch<'a> {
    pub(in crate::tool) fn new(
        runtime: &'a mut ToolRuntime,
        active: &'a ActiveToolCall<'a>,
        control: &'a CompositionControl,
    ) -> Self {
        Self {
            runtime,
            active,
            control,
        }
    }

    pub(in crate::tool) fn activate(&self) {
        self.control.active.store(true, Ordering::Relaxed);
    }

    pub(in crate::tool) async fn invoke(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        name: &str,
        input: Value,
    ) -> ReviewedToolResult {
        self.activate();
        if self.control.is_halting() {
            return pending().await;
        }

        let exit = match self.invoke_inner(agent, options, name, input).await {
            Ok((result, RoundEffect::Continue)) => return result,
            Ok((result, effect)) => CompositionExit::EndTurn { result, effect },
            Err(exit) => exit,
        };
        self.control.halt(exit).await
    }

    async fn invoke_inner(
        &mut self,
        agent: &mut Agent,
        options: &RunOptions,
        name: &str,
        input: Value,
    ) -> Result<(ReviewedToolResult, RoundEffect), CompositionExit> {
        if let Some(exit) = CompositionExit::run_limit(options) {
            return Err(exit);
        }
        self.runtime.reserve_tool_calls(options, 1)?;
        let call = ToolCall {
            id: next_id("child"),
            name: name.to_owned(),
            input,
        };
        let parent_id = &self.active.call.id;
        agent.start_child_tool_call(TranscriptItem::child_tool_event(
            parent_id,
            &call,
            ChildToolEvent::Started,
        ))?;
        if let Some(exit) = CompositionExit::run_limit(options) {
            return Err(exit);
        }

        let scheduled = if self.active.contains(name) {
            ScheduledToolCall::refused(call.clone(), format!("Recursive tool call to '{name}'"))
        } else {
            self.runtime.schedule_call(agent, call.clone())
        };
        let batch = if scheduled.execution_category.allows_parallel() {
            ToolCallBatch::Parallel(vec![scheduled])
        } else {
            ToolCallBatch::Exclusive(Box::new(scheduled))
        };
        let execution = {
            let scope = ChildAgentScope::new(agent, self.active.call);
            let mut executions = self
                .runtime
                .execute_batch(scope.agent, options, batch, Some(self.active))
                .await?;
            self.runtime
                .review_execution(scope.agent, executions.pop().expect("one scheduled child"))
                .await?
        };
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = execution.result
        else {
            unreachable!("the dispatcher returns a tool result for each call");
        };

        agent.append_transcript_item(TranscriptItem::child_tool_event(
            parent_id,
            &call,
            ChildToolEvent::Finished { is_error },
        ))?;
        self.runtime.child_task_succeeded |= execution.task_succeeded;
        options.check_limits()?;
        if execution.effect != RoundEffect::Terminate && options.stop_requested() {
            return Err(CompositionExit::Stop);
        }
        Ok((ReviewedToolResult { content, is_error }, execution.effect))
    }
}

/// An idle request belongs to the invocation that made it. Also restore the
/// parent's executing status when a tool drops a child future before it returns.
struct ChildAgentScope<'a> {
    agent: &'a mut Agent,
    parent: &'a ToolCall,
    parent_idle: bool,
}

impl<'a> ChildAgentScope<'a> {
    fn new(agent: &'a mut Agent, parent: &'a ToolCall) -> Self {
        let parent_idle = agent.take_idle_requested();
        Self {
            agent,
            parent,
            parent_idle,
        }
    }
}

impl Drop for ChildAgentScope<'_> {
    fn drop(&mut self) {
        self.agent.take_idle_requested();
        if self.parent_idle {
            self.agent.request_idle();
        }
        self.agent.set_status(AgentStatus::ExecutingTool {
            id: self.parent.id.clone(),
            name: self.parent.name.clone(),
        });
    }
}

pub(super) enum CompositionExit {
    Abort(RuntimeError),
    Stop,
    EndTurn {
        result: ReviewedToolResult,
        effect: RoundEffect,
    },
}

impl From<RuntimeError> for CompositionExit {
    fn from(error: RuntimeError) -> Self {
        Self::Abort(error)
    }
}

impl CompositionExit {
    fn run_limit(options: &RunOptions) -> Option<Self> {
        if let Err(error) = options.check_limits() {
            Some(Self::Abort(error))
        } else if options.stop_requested() {
            Some(Self::Stop)
        } else {
            None
        }
    }
}

#[derive(Default)]
pub(in crate::tool) struct CompositionControl {
    active: AtomicBool,
    exit: Mutex<Option<CompositionExit>>,
}

impl CompositionControl {
    fn is_halting(&self) -> bool {
        self.exit
            .lock()
            .expect("composition exit poisoned")
            .is_some()
    }

    async fn halt(&self, exit: CompositionExit) -> ReviewedToolResult {
        self.exit
            .lock()
            .expect("composition exit poisoned")
            .get_or_insert(exit);
        pending().await
    }

    /// Observe a child exit in the same poll in which it suspends the
    /// evaluator. Dropping the evaluation here keeps turn control out of the
    /// result channel even if the caller polls a child alongside other work.
    pub(super) async fn run<F: Future>(
        &self,
        options: &RunOptions,
        evaluation: F,
    ) -> Result<F::Output, CompositionExit> {
        let mut evaluation = pin!(evaluation);
        let mut limits = pin!(wait_for_run_limit(options));
        poll_fn(|cx| {
            if self.active.load(Ordering::Relaxed)
                && let Some(exit) = CompositionExit::run_limit(options)
            {
                return Poll::Ready(Err(exit));
            }

            let output = evaluation.as_mut().poll(cx);
            if self.active.load(Ordering::Relaxed) {
                if let Some(exit) = self.exit.lock().expect("composition exit poisoned").take() {
                    return Poll::Ready(Err(exit));
                }
                if let Poll::Ready(exit) = limits.as_mut().poll(cx) {
                    return Poll::Ready(Err(exit));
                }
            }
            output.map(Ok)
        })
        .await
    }
}

async fn wait_for_run_limit(options: &RunOptions) -> CompositionExit {
    if options.cancellation.is_none() && options.stop.is_none() && options.deadline.is_none() {
        return pending().await;
    }
    loop {
        if let Some(exit) = CompositionExit::run_limit(options) {
            return exit;
        }
        tokio::time::sleep(PARALLEL_JOIN_POLL_INTERVAL).await;
    }
}

/// Admission may await permission or run a hook that requests a stop. Check
/// again before entering the handler, with the composing evaluation still
/// responsible for ending the run and closing its parent call.
pub(super) async fn check_before_child_handler(
    options: &RunOptions,
    is_child: bool,
) -> Result<(), RuntimeError> {
    if is_child {
        options.check_limits()?;
        if options.stop_requested() {
            return pending().await;
        }
    }
    Ok(())
}
