use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::SystemTime;

use crate::runtime::error::RuntimeError;

const DEFAULT_PROVIDER_RETRY_BUDGET: usize = 5;

/// Why a run ended before its work was done, when a bound rather than the model
/// decided that.
///
/// The two graceful bounds — [`RunOptions::stop`] and
/// [`RunOptions::token_budget`] — deliberately end a run the way the model
/// finishing does: at a round boundary, transcript committed, `Ok`. That is the
/// right *behavior* and a silent *report*, because what the caller receives is
/// then identical for "the model was done" and "the runner refused to start
/// another round". A caller that has to tell those apart — a CLI owing a
/// distinct exit code, a supervisor deciding whether to prompt again — is
/// otherwise left either recomputing the comparison the runner already made or
/// reading prose, and the first answers a slightly different question (what is
/// true *now*, not what was true at the boundary) while the second is not a
/// contract at all. This records the runner's own decision at the moment it
/// made it.
///
/// Read it back through [`RunOptions::ended_early`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EarlyEnd {
    /// The run ended at a round boundary because [`RunOptions::stop`] was
    /// tripped.
    ///
    /// Reported in preference to [`TokenBudget`](Self::TokenBudget) when both
    /// were true at that boundary. A stop is an instruction the caller issued,
    /// and the runner would have ended there with no budget set at all; a
    /// crossed budget is an ambient bound that merely also held. Naming the
    /// bound would tell a caller its allowance ran out when what actually
    /// happened is that it asked to stop — and the runner's own control flow
    /// agrees, since it checks `stop` first and never reaches the budget check.
    StopRequested,
    /// The run ended at a round boundary because cumulative reported usage had
    /// reached or passed [`RunOptions::token_budget`].
    ///
    /// See that field for why this can only ever be noticed at a boundary, and
    /// for what the run keeps when it is.
    TokenBudget,
}

/// A shared flag a caller trips to stop a run.
///
/// `Debug` prints whether it has been tripped, so a host that embeds one in
/// its own options struct can still derive `Debug` on that struct.
#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
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
    /// Where a run records *why* it ended early, read back through
    /// [`ended_early`](Self::ended_early).
    ///
    /// The counterpart of [`token_usage`](Self::token_usage) for the decision
    /// rather than the count: the runner knows at the boundary it stops at
    /// which bound stopped it, and this is how that reaches a caller holding a
    /// clone of these options instead of being re-derived — or lost. Written at
    /// most once, first writer winning, because a run ends at exactly one
    /// boundary and because both conditions that produce an entry are sticky
    /// under a fixed bound: a tripped stop token stays tripped, and a crossed
    /// cumulative total stays crossed, so a later turn on this handle ends the
    /// same way and would record the same answer. Raising
    /// [`token_budget`](Self::token_budget) on a clone is the one way to make
    /// the entry stale — the deliberate aliasing `token_usage` warns about,
    /// seen from the reporting side. Like that field, this one is `pub` only so
    /// `RunOptions { .., ..RunOptions::default() }` construction keeps working;
    /// leave it at its default (a fresh, empty slot).
    pub early_end: Arc<OnceLock<EarlyEnd>>,
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
            early_end: Arc::new(OnceLock::new()),
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
    /// [`early_end`](Self::early_end) resets too, for a different reason — it
    /// records a decision rather than carrying a bound. A child that ends on the
    /// shared budget ended *its own* run at *its own* boundary; the parent then
    /// reaches its next boundary and records for itself, so keeping the slots
    /// apart loses nothing, while sharing one would let a child's ending be read
    /// as the parent's — including on a parent that went on to finish its work
    /// normally.
    ///
    /// mentra applies this itself on exactly one path: the `task` intrinsic's
    /// delegated subagent runs on the parent run's derived child options, so a
    /// model that delegates work cannot spend outside the bounds its own run was
    /// given. Every other subagent path is host-driven — call this when
    /// threading `RunOptions` into a subagent's or delegated run's own
    /// `Agent::run`/`resume` call, including through
    /// [`Session::spawn_subagent_with_options`](crate::Session::spawn_subagent_with_options)
    /// and, for a custom tool that spawns its own subagent,
    /// [`ToolContext::child_run_options`](crate::tool::ToolContext::child_run_options).
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

    /// Why the run ended early, or `None` when none did — the model finished, or
    /// the run failed outright and reported that as an error.
    ///
    /// Read from a clone: [`Agent::run`](crate::Agent::run) takes its options by
    /// value, so a caller that wants this keeps a `clone()` of what it passed
    /// in. The slot is shared behind an `Arc`, exactly as the counter behind
    /// [`reported_tokens`](Self::reported_tokens) is.
    pub fn ended_early(&self) -> Option<EarlyEnd> {
        self.early_end.get().copied()
    }

    /// Records why the runner is ending this run early, keeping the first answer
    /// if one is already there. See [`early_end`](Self::early_end) for why the
    /// first is the one that stays true.
    pub(crate) fn record_early_end(&self, end: EarlyEnd) {
        let _ = self.early_end.set(end);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancellation_token_shows_whether_it_was_tripped() {
        let token = CancellationToken::default();
        assert!(format!("{token:?}").contains("cancelled: false"));

        token.cancel();
        assert!(format!("{token:?}").contains("cancelled: true"));
    }

    #[test]
    fn a_clone_of_a_runs_options_reads_what_that_run_recorded() {
        // The mechanism an embedder depends on: `Agent::run` takes its options
        // by value, so the only way to hear back from a run is to have kept a
        // clone — which shares the slot, exactly as it shares the counter.
        let options = RunOptions {
            token_budget: Some(100),
            ..RunOptions::default()
        };
        let held = options.clone();

        options.record_early_end(EarlyEnd::TokenBudget);

        assert_eq!(held.ended_early(), Some(EarlyEnd::TokenBudget));
    }

    #[test]
    fn a_child_run_records_its_early_end_apart_from_its_parent() {
        // A delegated run that ends on the shared budget has ended its own run,
        // not its parent's. The parent reaches its own next boundary and records
        // there; until it does, claiming it ended early would be a guess — and a
        // wrong one for a parent that goes on to finish its work.
        let parent = RunOptions {
            token_budget: Some(100),
            ..RunOptions::default()
        };
        let child = parent.child();

        child.record_early_end(EarlyEnd::TokenBudget);

        assert_eq!(child.ended_early(), Some(EarlyEnd::TokenBudget));
        assert_eq!(parent.ended_early(), None);
        child.record_tokens(60);
        assert_eq!(
            parent.reported_tokens(),
            60,
            "what the two do share is the accounting, unchanged"
        );
    }

    #[test]
    fn the_first_early_end_recorded_is_the_one_that_stays() {
        // Both conditions are sticky under a fixed bound, so a handle reused for
        // a second turn ends the same way it ended the first. Keeping the first
        // answer makes that explicit rather than depending on it.
        let options = RunOptions::default();

        options.record_early_end(EarlyEnd::StopRequested);
        options.record_early_end(EarlyEnd::TokenBudget);

        assert_eq!(options.ended_early(), Some(EarlyEnd::StopRequested));
    }
}
