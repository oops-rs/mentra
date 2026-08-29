use std::{
    future::Future,
    time::{Duration, SystemTime},
};

use crate::{
    error::RuntimeError,
    runtime::{CancellationToken, RunOptions},
};

/// How often a bounded compaction re-reads its cancellation token while a
/// provider call is in flight.
///
/// [`CancellationToken`] is a polled flag rather than a signal — every other
/// bound check in mentra is a poll at a boundary — so a compaction that wants
/// to notice a cancel *between* boundaries has to look. 25 ms is short enough
/// that a cancel is indistinguishable from immediate to a person and long
/// enough that the poll costs nothing next to a summarization round trip.
const BOUND_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The run bounds a compaction is expected to honor.
///
/// Compaction is a full provider round trip — up to
/// [`summary_max_input_chars`](crate::agent::CompactionConfig::summary_max_input_chars)
/// of transcript, retried — and it happens exactly when a session has grown
/// long, which is exactly when someone is most likely to cancel. Without these
/// it ran to completion regardless, and a cancelled run only reported
/// [`RuntimeError::Cancelled`] once the summarizer had answered.
///
/// [`Default`] is "no bounds", which is what a compaction outside any run
/// gets, and reproduces the pre-0.24 behavior exactly.
///
/// A custom [`CompactionEngine`](super::CompactionEngine) is a supported
/// extension point and is **expected to honor these**: check
/// [`check`](Self::check) before doing work, and wrap any await that can
/// outlive a cancel in [`guard`](Self::guard). An engine that ignores them
/// leaves its host unable to cancel a compaction at all.
#[derive(Debug, Clone, Default)]
pub struct CompactionBounds {
    /// Trips when the run this compaction belongs to is cancelled. A
    /// compaction that sees it tripped fails with [`RuntimeError::Cancelled`]
    /// and leaves the transcript exactly as it found it.
    pub cancellation: Option<CancellationToken>,
    /// The wall-clock instant past which this compaction must not still be
    /// working. Reaching it fails with [`RuntimeError::DeadlineExceeded`],
    /// again without touching the transcript.
    pub deadline: Option<SystemTime>,
}

impl CompactionBounds {
    /// The bounds of an in-progress run, for a compaction happening inside it.
    ///
    /// Takes only [`cancellation`](RunOptions::cancellation) and
    /// [`deadline`](RunOptions::deadline) — the two bounds that say a run must
    /// stop *now*. The graceful [`stop`](RunOptions::stop) is deliberately not
    /// among them: it ends a run at the next round boundary with its
    /// transcript committed, and abandoning a compaction half-way is not that.
    pub fn from_run_options(options: &RunOptions) -> Self {
        Self {
            cancellation: options.cancellation.clone(),
            deadline: options.deadline,
        }
    }

    /// Whether either bound is set. `false` means every
    /// [`guard`](Self::guard) is a plain await.
    pub fn is_bounded(&self) -> bool {
        self.cancellation.is_some() || self.deadline.is_some()
    }

    /// Fails if a bound has already been reached.
    ///
    /// The same two errors the rest of the runtime reports for these bounds,
    /// so a caller's existing error handling is unchanged in kind: a cancelled
    /// compaction is [`RuntimeError::Cancelled`], not a compaction-specific
    /// failure that a degrade-gracefully path would swallow.
    pub fn check(&self) -> Result<(), RuntimeError> {
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

    /// Awaits `future`, abandoning it if a bound is reached first.
    ///
    /// The bounds are checked before `future` is polled at all, so a
    /// compaction already past its deadline never issues the request. While it
    /// runs they are re-checked every 25 ms, and at the deadline itself
    /// however far off it is. `future` is dropped where it stands when a bound
    /// wins — for a provider call that means the request is abandoned, which
    /// is the point.
    pub async fn guard<F>(&self, future: F) -> Result<F::Output, RuntimeError>
    where
        F: Future,
    {
        self.check()?;
        if !self.is_bounded() {
            return Ok(future.await);
        }

        tokio::pin!(future);
        loop {
            tokio::select! {
                // The work wins a tie: an answer that already arrived is not
                // worth discarding over a bound reached in the same instant.
                biased;
                output = &mut future => return Ok(output),
                () = tokio::time::sleep(self.poll_delay()) => self.check()?,
            }
        }
    }

    /// Waits `duration`, cutting the wait short if a bound is reached.
    ///
    /// The retry delay between compaction attempts, so a cancelled run does
    /// not sit out a sleep it will only wake from to give up.
    pub(crate) async fn sleep(&self, duration: Duration) -> Result<(), RuntimeError> {
        self.guard(tokio::time::sleep(duration)).await
    }

    /// How long to wait before the next bound check: the poll interval, or
    /// less when the deadline lands sooner (zero when it has already passed).
    fn poll_delay(&self) -> Duration {
        match self.deadline {
            Some(deadline) => deadline
                .duration_since(SystemTime::now())
                .unwrap_or(Duration::ZERO)
                .min(BOUND_POLL_INTERVAL),
            None => BOUND_POLL_INTERVAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bounds_bind_nothing() {
        let bounds = CompactionBounds::default();

        assert!(!bounds.is_bounded());
        assert!(bounds.check().is_ok());
    }

    #[test]
    fn run_bounds_carry_cancellation_and_deadline_but_not_a_graceful_stop() {
        let cancellation = CancellationToken::default();
        let stop = CancellationToken::default();
        let deadline = SystemTime::now() + Duration::from_secs(30);
        let options = RunOptions {
            cancellation: Some(cancellation.clone()),
            stop: Some(stop.clone()),
            deadline: Some(deadline),
            ..RunOptions::default()
        };

        let bounds = CompactionBounds::from_run_options(&options);

        assert_eq!(bounds.deadline, Some(deadline));
        assert!(bounds.check().is_ok());
        stop.cancel();
        assert!(
            bounds.check().is_ok(),
            "a graceful stop ends a run at a boundary; it does not abandon work in flight"
        );
        cancellation.cancel();
        assert!(matches!(bounds.check(), Err(RuntimeError::Cancelled)));
    }

    // Virtual time: the claim is "the guard gives up without waiting for the
    // work", and a wall-clock bound measures the test machine's load as much
    // as the code. Under `start_paused` the clock only advances when the
    // runtime is idle, so the elapsed value below is exactly what the guard's
    // own sleeps asked for.
    #[tokio::test(start_paused = true)]
    async fn a_guarded_future_is_abandoned_when_the_token_trips() {
        let cancellation = CancellationToken::default();
        let bounds = CompactionBounds {
            cancellation: Some(cancellation.clone()),
            deadline: None,
        };

        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancellation.cancel();
        });
        let started = tokio::time::Instant::now();
        let guarded: Result<(), RuntimeError> = bounds.guard(std::future::pending()).await;
        canceller.await.expect("canceller task");

        assert!(matches!(guarded, Err(RuntimeError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the guard notices the cancel within a poll interval or two of it"
        );
    }

    #[tokio::test]
    async fn a_guarded_future_is_never_polled_past_the_deadline() {
        let bounds = CompactionBounds {
            cancellation: None,
            deadline: Some(SystemTime::now() - Duration::from_secs(1)),
        };
        let mut polled = false;

        let guarded = bounds
            .guard(async {
                polled = true;
            })
            .await;

        assert!(matches!(guarded, Err(RuntimeError::DeadlineExceeded)));
        assert!(!polled, "an expired bound must not start the work");
    }

    #[tokio::test(start_paused = true)]
    async fn a_guarded_sleep_ends_early_on_cancellation() {
        let cancellation = CancellationToken::default();
        let bounds = CompactionBounds {
            cancellation: Some(cancellation.clone()),
            deadline: None,
        };
        cancellation.cancel();

        let started = tokio::time::Instant::now();
        let slept = bounds.sleep(Duration::from_secs(30)).await;

        assert!(matches!(slept, Err(RuntimeError::Cancelled)));
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "an already-cancelled sleep waits for nothing at all"
        );
    }
}
