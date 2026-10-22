use crate::agent::{AgentEvent, AgentEventBus};

/// An owned progress channel for work started by a tool call.
///
/// Obtain one from [`ToolContext::progress`](super::ToolContext::progress) or
/// [`ParallelToolContext::progress`](super::ParallelToolContext::progress).
/// It is `Clone + Send + Sync + 'static`, so background work can retain it
/// after the tool returns without keeping the tool context or agent borrowed.
/// Each update retains the originating tool call's id and name.
///
/// Updates go immediately to the parent's transient event stream and event
/// taps, including after the run ends. They are not stored, added to model
/// context, or replayed on the next run, and do not keep a run alive. Use
/// [`Deferred`](crate::agent::Deferred) to deliver results to the model.
/// The caller controls the lifetime and cancellation of its background work.
#[derive(Clone)]
pub struct ToolProgress {
    events: AgentEventBus,
    tool_call_id: String,
    tool_name: String,
}

impl ToolProgress {
    pub(crate) fn new(events: AgentEventBus, tool_call_id: String, tool_name: String) -> Self {
        Self {
            events,
            tool_call_id,
            tool_name,
        }
    }

    /// Emits a [`ToolExecutionProgress`](AgentEvent::ToolExecutionProgress)
    /// event immediately, without waiting for a model round boundary.
    ///
    /// Event taps run synchronously on the emitting thread, with the same
    /// restrictions as [`SessionHandle::register_agent_event_tap`](crate::SessionHandle::register_agent_event_tap).
    pub fn emit(&self, progress: impl Into<String>) {
        self.events.send(AgentEvent::ToolExecutionProgress {
            id: self.tool_call_id.clone(),
            name: self.tool_name.clone(),
            progress: progress.into(),
        });
    }
}

impl std::fmt::Debug for ToolProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolProgress")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .finish_non_exhaustive()
    }
}
