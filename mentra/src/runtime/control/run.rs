use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

use crate::runtime::error::RuntimeError;

const DEFAULT_PROVIDER_RETRY_BUDGET: usize = 5;
const DEFAULT_PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);
const DEFAULT_PROVIDER_RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

/// How long a run waits between provider retries.
///
/// *How many* retries it gets stays on [`RunOptions::retry_budget`]; see that
/// field for why the count did not move in here. This is the other half: what
/// each of those attempts waits for.
///
/// The defaults reproduce mentra's historical schedule exactly — 500 ms,
/// doubling, capped at 5 s — which is shaped for a blip: a connection reset, a
/// tunnel restart, a 502 from a proxy that is already coming back. A rate limit
/// is a different failure. It lasts as long as the window it belongs to, which
/// is routinely a minute, and the whole default budget elapses in about twelve
/// and a half seconds — five attempts into a limit that was never going to lift
/// in that time, and then a lost turn. A host that knows it is behind a metered
/// gateway can say so here instead of living with a schedule chosen for a
/// different failure.
///
/// ```rust
/// use std::time::Duration;
/// use mentra::runtime::{ProviderRetry, RunOptions};
///
/// // Wait out a minute-long rate-limit window rather than a blip.
/// let options = RunOptions {
///     retry_budget: 8,
///     ..RunOptions::default()
/// }
/// .with_provider_retry(ProviderRetry {
///     base_delay: Duration::from_secs(1),
///     max_delay: Duration::from_secs(30),
///     ..ProviderRetry::default()
/// });
/// # let _ = options;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRetry {
    /// The wait before the second attempt, doubled before each attempt after
    /// it.
    pub base_delay: Duration,
    /// The ceiling the doubling stops at. Reached and then held, so a long
    /// budget spends its tail attempts at a steady interval rather than an
    /// ever-growing one.
    pub max_delay: Duration,
    /// The longest wait a *server* may impose through `Retry-After`.
    ///
    /// A server that answers `Retry-After: 3600` is not describing a rate
    /// limit any run should sit through, and honoring it unconditionally hands
    /// a remote party control of how long this process blocks. The header is
    /// clamped to this before it is considered. It never shortens
    /// [`max_delay`](Self::max_delay): a schedule the host chose is the host's
    /// business, and this bounds only what the other end asked for.
    pub retry_after_cap: Duration,
}

impl Default for ProviderRetry {
    fn default() -> Self {
        Self {
            base_delay: DEFAULT_PROVIDER_RETRY_BASE_DELAY,
            max_delay: DEFAULT_PROVIDER_RETRY_MAX_DELAY,
            retry_after_cap: DEFAULT_PROVIDER_RETRY_AFTER_CAP,
        }
    }
}

impl ProviderRetry {
    /// The wait this schedule prescribes before the attempt that follows
    /// `attempt` (one-based), before anything the provider said is considered.
    pub fn scheduled_delay(&self, attempt: usize) -> Duration {
        // Clamped to `u32`'s width, not `usize`'s: the factor is a `u32`, and
        // shifting one by 32 or more is a panic in debug and nonsense in
        // release. Unreachable at the default budget of five, reachable the
        // moment a host raises it — which is the point of this type.
        let shift = attempt.saturating_sub(1).min(u32::BITS as usize - 1) as u32;
        let factor = 1u32 << shift;
        self.base_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }

    /// The wait actually taken before the attempt that follows `attempt`, given
    /// what the provider asked for in `retry_after`.
    ///
    /// The longer of the two wins, because they answer different questions: the
    /// schedule is the host's floor on how hard it is willing to hammer a
    /// provider, and `Retry-After` is the provider's floor on when it will
    /// answer again. Waiting the shorter of them satisfies neither. The
    /// server's number is clamped to
    /// [`retry_after_cap`](Self::retry_after_cap) first.
    pub fn delay_for(&self, attempt: usize, retry_after: Option<Duration>) -> Duration {
        let scheduled = self.scheduled_delay(attempt);
        match retry_after {
            Some(requested) => scheduled.max(requested.min(self.retry_after_cap)),
            None => scheduled,
        }
    }
}

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
    /// How many times one provider request may be re-attempted after a
    /// transient failure, before the run gives up and reports the error.
    ///
    /// The count stayed here rather than moving into [`ProviderRetry`] beside
    /// the schedule it belongs with, because
    /// `RunOptions { retry_budget: 3, ..default() }` is how every host that has
    /// ever changed this wrote it, and there is no spelling of a moved public
    /// field that keeps those compiling. One number, one home;
    /// [`provider_retry`](Self::provider_retry) holds the rest.
    ///
    /// **Retries are model requests.** Each attempt increments the same counter
    /// [`model_budget`](Self::model_budget) bounds, so a run with both set can
    /// exhaust its model budget on retries and end in
    /// [`ModelBudgetExceeded`](crate::error::RuntimeError::ModelBudgetExceeded)
    /// without the model ever having answered. That is deliberate:
    /// `model_budget` bounds how many times this run may reach for the
    /// provider, and an attempt that failed still reached. A host raising this
    /// budget to sit out a rate limit should raise `model_budget` with it, or
    /// leave `model_budget` at `None`, where no such interaction exists.
    pub retry_budget: usize,
    /// How long each of those retries waits. See [`ProviderRetry`]; the default
    /// schedule is mentra's historical one, unchanged.
    pub provider_retry: ProviderRetry,
    pub tool_budget: Option<usize>,
    /// A bound on how many provider requests this run may make, counting failed
    /// attempts — see [`retry_budget`](Self::retry_budget). `None` (the
    /// default) never bounds the run.
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
            provider_retry: ProviderRetry::default(),
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

    /// Sets the provider retry schedule on these options, returning the updated
    /// value. Leaves [`retry_budget`](Self::retry_budget), which counts the
    /// attempts this schedule spaces, alone.
    pub fn with_provider_retry(mut self, provider_retry: ProviderRetry) -> Self {
        self.provider_retry = provider_retry;
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
    /// field (`retry_budget`, `provider_retry`, `tool_budget`, `model_budget`,
    /// `round_strategy`) resets to [`RunOptions::default`]: those express per-run policy a child
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

    /// What the runner waited before each retry before this type existed:
    /// 500 ms doubling to a 5 s ceiling. Pinned so a future edit to the
    /// defaults has to be a deliberate one.
    #[test]
    fn the_default_schedule_is_the_one_mentra_has_always_used() {
        let retry = ProviderRetry::default();

        let delays: Vec<Duration> = (1..=8)
            .map(|attempt| retry.scheduled_delay(attempt))
            .collect();

        assert_eq!(
            delays,
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ]
        );
    }

    #[test]
    fn a_host_schedule_doubles_from_its_own_base_to_its_own_ceiling() {
        let retry = ProviderRetry {
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
            ..ProviderRetry::default()
        };

        let delays: Vec<Duration> = (1..=5)
            .map(|attempt| retry.scheduled_delay(attempt))
            .collect();

        assert_eq!(
            delays,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(10),
                Duration::from_secs(10),
            ]
        );
    }

    #[test]
    fn a_long_budget_does_not_overflow_the_doubling() {
        // The factor is a `u32`; the old code clamped the shift to `usize`'s
        // width, so a host generous enough to allow a 64th attempt got a panic
        // instead of a delay. Unreachable at a budget of five, reachable the
        // moment raising the budget is the supported thing to do.
        let retry = ProviderRetry::default();

        assert_eq!(retry.scheduled_delay(usize::MAX), retry.max_delay);
        assert_eq!(retry.scheduled_delay(64), retry.max_delay);
    }

    #[test]
    fn a_server_that_names_a_longer_wait_gets_it() {
        let retry = ProviderRetry::default();

        // Attempt 1's own schedule is 500 ms; the limit lasts a minute.
        assert_eq!(
            retry.delay_for(1, Some(Duration::from_secs(45))),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn a_server_that_names_a_shorter_wait_does_not_shorten_the_schedule() {
        // `Retry-After` is the provider's floor on when it will answer, not a
        // licence to hammer it sooner than the host chose to.
        let retry = ProviderRetry {
            base_delay: Duration::from_secs(5),
            ..ProviderRetry::default()
        };

        assert_eq!(
            retry.delay_for(1, Some(Duration::from_secs(1))),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn a_server_cannot_park_the_run_for_an_hour() {
        let retry = ProviderRetry::default();

        assert_eq!(
            retry.delay_for(1, Some(Duration::from_secs(3600))),
            retry.retry_after_cap,
            "the header is clamped before it is considered"
        );
        assert_eq!(retry.retry_after_cap, Duration::from_secs(60));
    }

    #[test]
    fn the_cap_bounds_the_server_and_never_the_host() {
        // A host that chose to wait five minutes between attempts keeps that
        // schedule; the cap exists to bound a remote party, not the caller.
        let retry = ProviderRetry {
            base_delay: Duration::from_secs(300),
            max_delay: Duration::from_secs(300),
            retry_after_cap: Duration::from_secs(60),
        };

        assert_eq!(retry.delay_for(1, None), Duration::from_secs(300));
        assert_eq!(
            retry.delay_for(1, Some(Duration::from_secs(3600))),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn a_silent_provider_leaves_the_schedule_alone() {
        let retry = ProviderRetry::default();

        assert_eq!(retry.delay_for(3, None), retry.scheduled_delay(3));
    }

    #[test]
    fn a_default_run_carries_the_default_schedule() {
        assert_eq!(
            RunOptions::default().provider_retry,
            ProviderRetry::default()
        );
        assert_eq!(RunOptions::default().retry_budget, 5);
    }

    #[test]
    fn a_child_run_starts_from_the_default_schedule() {
        // Same reasoning as `retry_budget`: how patiently a delegated run
        // treats its own provider is its own policy, not an aggregate bound
        // inherited from the parent.
        let parent = RunOptions::default().with_provider_retry(ProviderRetry {
            base_delay: Duration::from_secs(30),
            ..ProviderRetry::default()
        });

        assert_eq!(parent.child().provider_retry, ProviderRetry::default());
    }

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
