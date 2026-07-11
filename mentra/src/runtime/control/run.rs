use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::SystemTime;

use crate::runtime::error::RuntimeError;

const DEFAULT_PROVIDER_RETRY_BUDGET: usize = 5;

#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

pub type CancellationFlag = CancellationToken;

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct RunOptions {
    pub cancellation: Option<CancellationToken>,
    /// A **graceful** stop signal, distinct from [`cancellation`](Self::cancellation).
    ///
    /// When this token is tripped (via [`CancellationToken::cancel`]) the run ends
    /// **successfully** at the next round boundary — the committed transcript is
    /// kept (the run resolves like the model self-terminating with no further tool
    /// calls), rather than failing and rolling the run back the way `cancellation`
    /// does. Use it to stop gathering once enough work is done while preserving the
    /// gathered context for a follow-up turn on the same agent. `None` (the default)
    /// never stops the run.
    pub stop: Option<CancellationToken>,
    pub deadline: Option<SystemTime>,
    pub retry_budget: usize,
    pub tool_budget: Option<usize>,
    pub model_budget: Option<usize>,
    /// A per-run [`RoundStrategy`](crate::agent::RoundStrategy) invoked at each
    /// round boundary (after a committed tool round and after a committed
    /// tool-free assistant message). It is owned by this single `Agent::run`
    /// invocation, never by a shared [`Runtime`](crate::Runtime), so one run's
    /// steering and stop state cannot leak into another run. `None` (the default)
    /// reproduces mentra's built-in round loop exactly.
    pub round_strategy: Option<Arc<dyn crate::agent::RoundStrategy>>,
    /// A **soft** aggregate token bound on this run's reported usage, distinct
    /// from [`model_budget`](Self::model_budget) (which caps the number of
    /// provider *requests*, not tokens).
    ///
    /// Token usage is only known once a round's response has streamed in full
    /// (the same point where `TurnRunner` emits
    /// `AgentEvent::UsageReport`), so this can never be a hard ceiling: a single
    /// round is always allowed to finish even if it pushes cumulative usage from
    /// under the bound to well past it. Once a round has completed, the bound is
    /// checked at the same round-boundary point where [`stop`](Self::stop) is
    /// checked: if cumulative reported `input_tokens + output_tokens` (summed
    /// across every round this run, and any [`child`](Self::child) run sharing
    /// this handle, has completed) has reached or exceeded the bound, the run
    /// ends **gracefully** there, exactly as `stop` does — the committed
    /// transcript is kept, not rolled back. Cache-read and cache-creation tokens
    /// are not counted. `None` (the default) never stops the run. This is never
    /// an expense bound: mentra has no injected price source and makes no
    /// monetary claim.
    pub token_budget: Option<u64>,
    /// Shared cumulative `input_tokens + output_tokens` counter backing
    /// [`token_budget`](Self::token_budget) and read back through
    /// [`reported_tokens`](Self::reported_tokens). Held behind an `Arc` so a
    /// [`child`](Self::child) run reports into the same aggregate as its parent —
    /// that is the intended way to share it. This field is `pub` only so
    /// `RunOptions { .., ..RunOptions::default() }` construction keeps working;
    /// leave it at its default (a fresh, zeroed counter) unless you are
    /// deliberately aliasing a specific run's accounting.
    pub token_usage: Arc<AtomicU64>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            cancellation: None,
            stop: None,
            deadline: None,
            retry_budget: DEFAULT_PROVIDER_RETRY_BUDGET,
            tool_budget: None,
            model_budget: None,
            round_strategy: None,
            token_budget: None,
            token_usage: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl RunOptions {
    /// Attaches a per-run [`RoundStrategy`](crate::agent::RoundStrategy) to these
    /// options, returning the updated value.
    pub fn with_round_strategy(mut self, strategy: Arc<dyn crate::agent::RoundStrategy>) -> Self {
        self.round_strategy = Some(strategy);
        self
    }

    /// Derives [`RunOptions`] for work spawned during this run — a subagent or a
    /// delegated run — sharing this run's aggregate safety bounds: the same
    /// [`cancellation`](Self::cancellation) and [`stop`](Self::stop) tokens (so
    /// cancelling or gracefully stopping the parent also ends the child), the same
    /// [`deadline`](Self::deadline), and the same [`token_budget`](Self::token_budget)
    /// bound backed by the *same* accounting handle — a child's reported usage
    /// adds to the parent's running total, so parent and child together trip one
    /// shared bound rather than each getting an independent one. Every other
    /// field (`retry_budget`, `tool_budget`, `model_budget`, `round_strategy`)
    /// resets to [`RunOptions::default`]: those express per-run policy a child
    /// sets independently, not an aggregate safety bound.
    ///
    /// mentra does not itself spawn a child run from a parent's `Agent::run` call
    /// — a host drives both. Call this when threading `RunOptions` into a
    /// subagent's or delegated run's own `Agent::run`/`resume` call.
    pub fn child(&self) -> RunOptions {
        RunOptions {
            cancellation: self.cancellation.clone(),
            stop: self.stop.clone(),
            deadline: self.deadline,
            token_budget: self.token_budget,
            token_usage: Arc::clone(&self.token_usage),
            ..RunOptions::default()
        }
    }

    /// Cumulative `input_tokens + output_tokens` reported so far against
    /// [`token_budget`](Self::token_budget), aggregated across this run and any
    /// [`child`](Self::child) run sharing this handle.
    pub fn reported_tokens(&self) -> u64 {
        self.token_usage.load(Ordering::SeqCst)
    }

    pub(crate) fn record_tokens(&self, tokens: u64) {
        self.token_usage.fetch_add(tokens, Ordering::SeqCst);
    }

    /// Whether cumulative reported usage has reached or exceeded
    /// [`token_budget`](Self::token_budget). `false` when no bound is set.
    pub(crate) fn token_budget_exceeded(&self) -> bool {
        self.token_budget
            .is_some_and(|budget| self.reported_tokens() >= budget)
    }

    pub(crate) fn check_limits(&self) -> Result<(), RuntimeError> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(RuntimeError::Cancelled);
        }

        if self
            .deadline
            .is_some_and(|deadline| SystemTime::now() >= deadline)
        {
            return Err(RuntimeError::DeadlineExceeded);
        }

        Ok(())
    }

    /// Whether a graceful stop has been requested via [`stop`](Self::stop). The
    /// runner checks this at each round boundary, where the transcript is at a
    /// consistent point, and ends the run successfully when it is set.
    pub(crate) fn stop_requested(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    pub(crate) fn tool_budget(&self) -> usize {
        self.tool_budget.unwrap_or(usize::MAX)
    }

    pub(crate) fn model_budget(&self) -> usize {
        self.model_budget.unwrap_or(usize::MAX)
    }
}
