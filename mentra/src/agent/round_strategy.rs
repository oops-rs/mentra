//! Per-run round strategy: a host-supplied policy invoked at the two round
//! boundaries of [`Agent::run`](crate::Agent::run).
//!
//! A [`RoundStrategy`] rides on [`RunOptions`](crate::runtime::RunOptions) and so
//! belongs to exactly one run. Its state lives and dies with that run and can
//! never leak through a pooled [`Runtime`](crate::Runtime) into another run. Its
//! absence — the `None` default on `RunOptions` — reproduces mentra's built-in
//! round loop byte-for-byte.
//!
//! The runner invokes the strategy at two commit points: after a tool round's
//! results are committed to the transcript, and after a tool-free assistant
//! message is committed but before the run returns it. At each point the strategy
//! decides how the run proceeds via [`RoundDecision`].

use async_trait::async_trait;

use crate::{ContentBlock, Message, ModelInfo, ReasoningOptions};

/// Identifies which round boundary invoked a [`RoundStrategy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundBoundary {
    /// A tool round's results were just committed to the transcript. The run is
    /// about to advance to the next round.
    ToolResultsCommitted,
    /// A tool-free assistant message was just committed. The run is about to
    /// return it as the final message unless the strategy injects another round.
    AssistantMessageCommitted,
}

/// Provider-neutral summary of one committed tool result.
///
/// Exposes only what a host needs to reason about a completed tool round without
/// coupling to mentra's internal tool-result representation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundToolResult {
    /// The `tool_use_id` correlating this result with its originating tool call.
    pub tool_use_id: String,
    /// The name of the tool that produced the result.
    pub tool_name: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// How a [`RoundAdjustment`] changes reasoning settings for subsequent rounds.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReasoningChange {
    /// Set reasoning to the given options.
    Set(ReasoningOptions),
    /// Clear any configured reasoning, restoring the provider's default effort.
    Clear,
}

/// A model and/or reasoning override applied to the run's subsequent rounds.
///
/// Applying an adjustment reuses the same live-config mechanics as
/// [`Agent::set_model`](crate::Agent::set_model) and
/// [`Agent::set_reasoning`](crate::Agent::set_reasoning): the change takes effect
/// on the next model request and persists for the remainder of the run (and, under
/// a persisting store, on the agent record) exactly as those methods do. Build one
/// with [`RoundAdjustment::new`] and the `with_*` methods.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RoundAdjustment {
    pub(crate) model: Option<ModelInfo>,
    pub(crate) reasoning: Option<ReasoningChange>,
}

impl RoundAdjustment {
    /// An adjustment that changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch the model (and its provider) for subsequent rounds.
    pub fn with_model(mut self, model: ModelInfo) -> Self {
        self.model = Some(model);
        self
    }

    /// Change the reasoning settings for subsequent rounds.
    pub fn with_reasoning(mut self, reasoning: ReasoningChange) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Whether this adjustment would change nothing.
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.reasoning.is_none()
    }
}

/// The decision a [`RoundStrategy`] returns at a round boundary.
///
/// Construct one with [`RoundDecision::proceed`], [`RoundDecision::inject`], or
/// [`RoundDecision::stop`] for the common cases, or the variants directly to carry
/// a [`RoundAdjustment`].
#[non_exhaustive]
pub enum RoundDecision {
    /// Proceed with the default flow. At [`RoundBoundary::AssistantMessageCommitted`]
    /// this accepts the terminal message and lets the run return; at
    /// [`RoundBoundary::ToolResultsCommitted`] it advances to the next round. Any
    /// carried [`RoundAdjustment`] applies to subsequent rounds.
    Continue(RoundAdjustment),
    /// Do not end the run: append `content` as a corrective user turn and run
    /// another round. At [`RoundBoundary::AssistantMessageCommitted`] this prevents
    /// the run from returning. Any carried [`RoundAdjustment`] applies to that round.
    Inject {
        /// Corrective content appended to the transcript as a user message.
        content: Vec<ContentBlock>,
        /// Model/reasoning override applied before the injected round.
        adjust: RoundAdjustment,
    },
    /// End the run gracefully at this boundary, committing the transcript exactly
    /// as [`RunOptions::stop`](crate::runtime::RunOptions) does. A stop request
    /// asserts nothing about whether the run produced a valid answer.
    Stop,
}

impl RoundDecision {
    /// Continue with no model/reasoning change.
    pub fn proceed() -> Self {
        RoundDecision::Continue(RoundAdjustment::default())
    }

    /// Inject corrective context and run another round, with no adjustment.
    pub fn inject(content: impl Into<Vec<ContentBlock>>) -> Self {
        RoundDecision::Inject {
            content: content.into(),
            adjust: RoundAdjustment::default(),
        }
    }

    /// Request a graceful stop.
    pub fn stop() -> Self {
        RoundDecision::Stop
    }
}

/// Read-only view of a round boundary handed to a [`RoundStrategy`].
pub struct RoundContext<'a> {
    boundary: RoundBoundary,
    assistant_message: Option<&'a Message>,
    tool_results: &'a [RoundToolResult],
    rounds_completed: usize,
    model_requests: usize,
    transport_retries: usize,
}

impl<'a> RoundContext<'a> {
    pub(crate) fn new(
        boundary: RoundBoundary,
        assistant_message: Option<&'a Message>,
        tool_results: &'a [RoundToolResult],
        rounds_completed: usize,
        model_requests: usize,
        transport_retries: usize,
    ) -> Self {
        Self {
            boundary,
            assistant_message,
            tool_results,
            rounds_completed,
            model_requests,
            transport_retries,
        }
    }

    /// Which boundary invoked the strategy.
    pub fn boundary(&self) -> RoundBoundary {
        self.boundary
    }

    /// The assistant message just committed. Present only at
    /// [`RoundBoundary::AssistantMessageCommitted`], and absent even there if the
    /// terminal turn carried no content.
    pub fn assistant_message(&self) -> Option<&Message> {
        self.assistant_message
    }

    /// Summaries of the tool results just committed. Present only at
    /// [`RoundBoundary::ToolResultsCommitted`]; empty otherwise.
    pub fn tool_results(&self) -> &[RoundToolResult] {
        self.tool_results
    }

    /// The number of rounds the run has entered so far, including this one.
    pub fn rounds_completed(&self) -> usize {
        self.rounds_completed
    }

    /// The number of provider requests the run has issued so far, including
    /// transient transport retries. This mirrors mentra's request counter, which
    /// is not a pure logical-round counter.
    pub fn model_requests(&self) -> usize {
        self.model_requests
    }

    /// The number of transient transport-connection retries the run has made so
    /// far — the subset of [`model_requests`](Self::model_requests) that were
    /// *not* the round's successful attempt. Kept distinct from
    /// [`rounds_completed`](Self::rounds_completed), which counts only completed
    /// logical rounds: a round that needed retries before its connection opened
    /// still counts as exactly one completed round.
    pub fn transport_retries(&self) -> usize {
        self.transport_retries
    }
}

/// A host-supplied policy invoked at each round boundary of a single
/// [`Agent::run`](crate::Agent::run) invocation.
///
/// The strategy is carried on [`RunOptions`](crate::runtime::RunOptions) and so is
/// bound to exactly one run: its state lives and dies with that run and can never
/// leak through a pooled [`Runtime`](crate::Runtime) into another run. Its absence
/// — the `None` default — reproduces mentra's built-in round loop exactly.
///
/// At each boundary the strategy may [continue](RoundDecision::Continue),
/// [inject](RoundDecision::Inject) corrective context, switch the next round's
/// model or reasoning via a [`RoundAdjustment`], or request a graceful
/// [stop](RoundDecision::Stop). Every decision still passes through the run's
/// existing budget, cancellation, and deadline checks; an injected round is a
/// normal round in every respect.
#[async_trait]
pub trait RoundStrategy: Send + Sync {
    /// Decide how the run should proceed at the given boundary.
    async fn on_round(&self, ctx: RoundContext<'_>) -> RoundDecision;
}
