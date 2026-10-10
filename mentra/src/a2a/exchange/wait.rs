//! Waiting on the remote agent while staying answerable to the run.

use std::convert::Infallible;
use std::time::Duration;

use tokio::time::Instant;

use crate::runtime::RunOptions;

/// How often a wait checks whether the run was cancelled or asked to stop.
///
/// A poll interval is seconds long, and the cancellation token is a flag
/// rather than a notification, so without this a cancelled run would wait out
/// the rest of the interval before noticing.
const INTERRUPT_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Why a wait ended before what it waited on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    /// The deadline passed.
    Elapsed,
    /// The run asked to stop at its next boundary.
    Stopping,
    /// The run was cancelled or passed its own deadline.
    Cancelled,
}

/// Runs `future` until it completes, `deadline` passes, or the run is
/// cancelled or asked to stop — whichever comes first.
pub(super) async fn race<F: Future>(
    future: F,
    deadline: Instant,
    options: &RunOptions,
) -> Result<F::Output, Wake> {
    tokio::pin!(future);
    loop {
        if options.check_limits().is_err() {
            return Err(Wake::Cancelled);
        }
        if options.stop_requested() {
            return Err(Wake::Stopping);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Wake::Elapsed);
        }
        let check = tokio::time::sleep((deadline - now).min(INTERRUPT_CHECK_INTERVAL));
        tokio::select! {
            output = &mut future => return Ok(output),
            () = check => {}
        }
    }
}

/// Waits until `deadline`, waking early if the run is cancelled, passes its
/// own deadline, or is asked to stop.
pub(super) async fn wait(deadline: Instant, options: &RunOptions) -> Wake {
    match race(std::future::pending::<Infallible>(), deadline, options).await {
        Ok(never) => match never {},
        Err(wake) => wake,
    }
}
