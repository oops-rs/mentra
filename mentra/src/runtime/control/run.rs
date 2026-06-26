use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
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
        }
    }
}

impl RunOptions {
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
