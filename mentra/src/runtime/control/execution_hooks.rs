use std::{
    fmt,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    error::RuntimeError,
    tool::{ToolAudience, ToolResultContent},
};

use super::hooks::{
    LiveHookRegistration, LiveHookRegistry, SharedHookRegistrationConflict,
    SharedLiveHookRegistration,
};
use super::{PostExecutionContext, PreExecutionContext};

static NEXT_EXECUTION_HOOK_BATCH_ID: AtomicU64 = AtomicU64::new(1);

fn next_batch_id() -> u64 {
    NEXT_EXECUTION_HOOK_BATCH_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("execution hook batch identifiers exhausted")
}

/// What one ordered mixed participant decided before a tool runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeDecision {
    Continue,
    Deny(String),
    Modify {
        input_json: String,
        attribution: Option<String>,
    },
}

/// What one ordered mixed participant decided after a tool runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterDecision {
    Continue,
    Deny(String),
    Replace {
        content: ToolResultContent,
        /// `None` preserves the verdict left by the tool or an earlier participant.
        is_error: Option<bool>,
        attribution: Option<String>,
    },
}

/// One named participant in an ordered mixed execution-hook chain.
///
/// Both methods default to no opinion, so event-specific adapters implement
/// only the seam they serve. Expected participant failures and fail-open/closed
/// policy belong inside adapters; an `Err` here retains Mentra's existing hook
/// behavior and propagates as a [`RuntimeError`].
#[async_trait]
pub trait ExecutionHookParticipant: Send + Sync {
    fn name(&self) -> &str;

    async fn before(&self, _context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        Ok(BeforeDecision::Continue)
    }

    async fn after(&self, _context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        Ok(AfterDecision::Continue)
    }
}

type ExecutionHookBatch = Vec<Arc<dyn ExecutionHookParticipant>>;
type LiveExecutionHookRegistry = LiveHookRegistry<ExecutionHookBatch>;

#[async_trait]
impl<T: ExecutionHookParticipant + ?Sized> ExecutionHookParticipant for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn before(&self, context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        (**self).before(context).await
    }

    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        (**self).after(context).await
    }
}

#[async_trait]
impl<T: ExecutionHookParticipant + ?Sized> ExecutionHookParticipant for Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn before(&self, context: &PreExecutionContext) -> Result<BeforeDecision, RuntimeError> {
        (**self).before(context).await
    }

    async fn after(&self, context: &PostExecutionContext) -> Result<AfterDecision, RuntimeError> {
        (**self).after(context).await
    }
}

/// Keeps one atomic live batch of mixed execution hooks registered.
///
/// Dropping the guard removes the exact batch. A tool call that already holds
/// a snapshot keeps its participants through both seams. The guard does not
/// keep its runtime alive.
#[must_use = "dropping the guard immediately unregisters the mixed execution hooks"]
pub struct ExecutionHookRegistration {
    inner: LiveHookRegistration<ExecutionHookBatch>,
}

impl fmt::Debug for ExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionHookRegistration")
            .field("audience", &self.inner.audience)
            .field("active", &self.inner.active)
            .finish_non_exhaustive()
    }
}

impl ExecutionHookRegistration {
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }

    pub fn unregister(mut self) -> bool {
        self.inner.unregister()
    }
}

/// Keeps one caller-keyed mixed-hook batch registered while any holder lives.
///
/// Re-registering the same key shares one batch only when its audience and
/// ordered [`Arc`] participant identities are unchanged. Clones and repeated
/// successful registrations are holders; the last drop removes the batch.
/// Keys are local to the mixed execution-hook chain.
#[derive(Clone)]
#[must_use = "dropping the last holder unregisters the shared mixed execution hooks"]
pub struct SharedExecutionHookRegistration {
    inner: Arc<SharedLiveHookRegistration<ExecutionHookBatch>>,
}

impl fmt::Debug for SharedExecutionHookRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedExecutionHookRegistration")
            .field("key", &self.inner.key)
            .field("audience", &self.inner.audience)
            .finish_non_exhaustive()
    }
}

impl SharedExecutionHookRegistration {
    /// Returns the caller-supplied identity key for this shared batch.
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// Returns the audience this batch is scoped to, or `None` when it is global.
    pub fn audience(&self) -> Option<&ToolAudience> {
        self.inner.audience.as_ref()
    }
}

/// Builder-time and live storage for the ordered mixed participant chain.
///
/// Unlike legacy post hooks, both seams walk this chain forward. Builder-time
/// participants are always before live batches; matching global and audience
/// batches retain their one insertion order.
#[derive(Clone)]
pub struct ExecutionHooks {
    permanent: Vec<Arc<dyn ExecutionHookParticipant>>,
    live: Arc<RwLock<LiveExecutionHookRegistry>>,
}

impl Default for ExecutionHooks {
    fn default() -> Self {
        Self {
            permanent: Vec::new(),
            live: Arc::new(RwLock::new(LiveHookRegistry::new())),
        }
    }
}

impl ExecutionHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_participant<H>(mut self, participant: H) -> Self
    where
        H: ExecutionHookParticipant + 'static,
    {
        self.permanent.push(Arc::new(participant));
        self
    }

    pub fn extend<I>(mut self, participants: I) -> Self
    where
        I: IntoIterator<Item = Arc<dyn ExecutionHookParticipant>>,
    {
        self.permanent.extend(participants);
        self
    }

    pub fn snapshot(&self, audience: Option<&ToolAudience>) -> ExecutionHookSnapshot {
        let registry = self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut participants = Vec::with_capacity(
            self.permanent.len() + registry.matching(audience).map(Vec::len).sum::<usize>(),
        );
        participants.extend(self.permanent.iter().cloned());
        for batch in registry.matching(audience) {
            participants.extend(batch.iter().cloned());
        }
        ExecutionHookSnapshot { participants }
    }

    pub(crate) fn register_live(
        &self,
        audience: Option<ToolAudience>,
        participants: ExecutionHookBatch,
    ) -> ExecutionHookRegistration {
        let id = next_batch_id();
        let guard_audience = audience.clone();
        self.live
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, audience, participants);
        ExecutionHookRegistration {
            inner: LiveHookRegistration::new(Arc::downgrade(&self.live), id, guard_audience),
        }
    }

    pub(crate) fn register_live_shared(
        &self,
        key: String,
        audience: Option<ToolAudience>,
        participants: ExecutionHookBatch,
    ) -> Result<SharedExecutionHookRegistration, SharedHookRegistrationConflict> {
        let id = next_batch_id();
        let inner = LiveHookRegistry::register_shared(
            &self.live,
            id,
            key,
            audience,
            participants,
            |left, right| {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| Arc::ptr_eq(left, right))
            },
        )?;
        Ok(SharedExecutionHookRegistration { inner })
    }
}

/// One immutable participant snapshot retained across both sides of a tool call.
#[derive(Clone, Default)]
pub struct ExecutionHookSnapshot {
    participants: Vec<Arc<dyn ExecutionHookParticipant>>,
}

impl ExecutionHookSnapshot {
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    pub async fn before(
        &self,
        context: &PreExecutionContext,
    ) -> Result<BeforeDecision, RuntimeError> {
        let mut current = context.clone();
        let mut attributions = Vec::new();

        for participant in &self.participants {
            match participant.before(&current).await? {
                BeforeDecision::Continue => {}
                BeforeDecision::Deny(reason) => {
                    return Ok(BeforeDecision::Deny(named_denial(
                        participant.name(),
                        reason,
                    )));
                }
                BeforeDecision::Modify {
                    input_json,
                    attribution,
                } => {
                    current.input_json = input_json;
                    attributions.push(named_attribution(participant.name(), attribution));
                }
            }
        }

        Ok(if attributions.is_empty() {
            BeforeDecision::Continue
        } else {
            BeforeDecision::Modify {
                input_json: current.input_json,
                attribution: Some(attributions.join("; ")),
            }
        })
    }

    pub async fn after(
        &self,
        context: &PostExecutionContext,
    ) -> Result<AfterDecision, RuntimeError> {
        let mut current = context.clone();
        let mut attributions = Vec::new();

        for participant in &self.participants {
            match participant.after(&current).await? {
                AfterDecision::Continue => {}
                AfterDecision::Deny(reason) => {
                    return Ok(AfterDecision::Deny(named_denial(
                        participant.name(),
                        reason,
                    )));
                }
                AfterDecision::Replace {
                    content,
                    is_error,
                    attribution,
                } => {
                    current.content = content;
                    if let Some(is_error) = is_error {
                        current.is_error = is_error;
                    }
                    attributions.push(named_attribution(participant.name(), attribution));
                }
            }
        }

        Ok(if attributions.is_empty() {
            AfterDecision::Continue
        } else {
            AfterDecision::Replace {
                content: current.content,
                is_error: Some(current.is_error),
                attribution: Some(attributions.join("; ")),
            }
        })
    }
}

fn named_denial(name: &str, reason: String) -> String {
    format!("denied by execution hook '{name}': {reason}")
}

fn named_attribution(name: &str, attribution: Option<String>) -> String {
    match attribution {
        Some(attribution) => format!("execution hook '{name}': {attribution}"),
        None => format!("execution hook '{name}'"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::AssertUnwindSafe,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use super::*;

    struct Rewrites {
        name: &'static str,
        suffix: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ExecutionHookParticipant for Rewrites {
        fn name(&self) -> &str {
            self.name
        }

        async fn before(
            &self,
            context: &PreExecutionContext,
        ) -> Result<BeforeDecision, RuntimeError> {
            self.log.lock().expect("log").push(self.name);
            Ok(BeforeDecision::Modify {
                input_json: format!("{}{}", context.input_json, self.suffix),
                attribution: Some(self.suffix.to_string()),
            })
        }

        async fn after(
            &self,
            context: &PostExecutionContext,
        ) -> Result<AfterDecision, RuntimeError> {
            self.log.lock().expect("log").push(self.name);
            Ok(AfterDecision::Replace {
                content: ToolResultContent::text(format!(
                    "{}{}",
                    context.content.to_display_string(),
                    self.suffix
                )),
                is_error: None,
                attribution: Some(self.suffix.to_string()),
            })
        }
    }

    fn pre() -> PreExecutionContext {
        PreExecutionContext {
            agent_id: "agent".into(),
            tool_name: "tool".into(),
            tool_call_id: "call".into(),
            input_json: "start".into(),
            working_directory: PathBuf::from("/repo"),
        }
    }

    fn post(is_error: bool) -> PostExecutionContext {
        PostExecutionContext {
            agent_id: "agent".into(),
            tool_name: "tool".into(),
            tool_call_id: "call".into(),
            input_json: "input".into(),
            working_directory: PathBuf::from("/repo"),
            content: ToolResultContent::text("out"),
            is_error,
        }
    }

    #[tokio::test]
    async fn mixed_chain_runs_forward_both_ways_and_aggregates_attribution() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = ExecutionHooks::new()
            .with_participant(Rewrites {
                name: "host",
                suffix: "-host",
                log: Arc::clone(&log),
            })
            .with_participant(Rewrites {
                name: "workspace",
                suffix: "-workspace",
                log: Arc::clone(&log),
            });
        let snapshot = chain.snapshot(None);

        assert_eq!(
            snapshot.before(&pre()).await.expect("before"),
            BeforeDecision::Modify {
                input_json: "start-host-workspace".into(),
                attribution: Some(
                    "execution hook 'host': -host; execution hook 'workspace': -workspace".into()
                ),
            }
        );
        log.lock().expect("log").clear();
        assert_eq!(
            snapshot.after(&post(true)).await.expect("after"),
            AfterDecision::Replace {
                content: ToolResultContent::text("out-host-workspace"),
                is_error: Some(true),
                attribution: Some(
                    "execution hook 'host': -host; execution hook 'workspace': -workspace".into()
                ),
            }
        );
        assert_eq!(*log.lock().expect("log"), ["host", "workspace"]);
    }

    struct Counts {
        name: &'static str,
        before: Arc<AtomicUsize>,
        after: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ExecutionHookParticipant for Counts {
        fn name(&self) -> &str {
            self.name
        }

        async fn before(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<BeforeDecision, RuntimeError> {
            self.before.fetch_add(1, Ordering::SeqCst);
            Ok(BeforeDecision::Continue)
        }

        async fn after(
            &self,
            _context: &PostExecutionContext,
        ) -> Result<AfterDecision, RuntimeError> {
            self.after.fetch_add(1, Ordering::SeqCst);
            Ok(AfterDecision::Continue)
        }
    }

    #[tokio::test]
    async fn one_atomic_batch_is_audience_scoped_duplicate_preserving_and_exactly_removed() {
        let hooks = ExecutionHooks::new();
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let participant = Arc::new(Counts {
            name: "same",
            before: Arc::clone(&before),
            after: Arc::clone(&after),
        });
        let participant: Arc<dyn ExecutionHookParticipant> = participant;
        let audience = ToolAudience::new("alpha");
        let guard = hooks.register_live(
            Some(audience.clone()),
            vec![Arc::clone(&participant), participant],
        );

        hooks
            .snapshot(Some(&audience))
            .before(&pre())
            .await
            .expect("alpha before");
        hooks
            .snapshot(Some(&audience))
            .after(&post(false))
            .await
            .expect("alpha after");
        hooks
            .snapshot(Some(&ToolAudience::new("beta")))
            .before(&pre())
            .await
            .expect("beta before");
        hooks.snapshot(None).before(&pre()).await.expect("global");
        assert_eq!(before.load(Ordering::SeqCst), 2);
        assert_eq!(after.load(Ordering::SeqCst), 2);

        assert!(guard.unregister());
        assert!(hooks.snapshot(Some(&audience)).is_empty());
    }

    #[tokio::test]
    async fn shared_mixed_batch_has_one_entry_and_lives_until_its_last_holder() {
        let hooks = ExecutionHooks::new();
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let participant: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
            name: "shared",
            before: Arc::clone(&before),
            after: Arc::clone(&after),
        });
        let audience = ToolAudience::new("alpha");

        let first = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(audience.clone()),
                vec![Arc::clone(&participant)],
            )
            .expect("first holder");
        let second = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(audience.clone()),
                vec![Arc::clone(&participant)],
            )
            .expect("second holder");
        let cloned = second.clone();

        let snapshot = hooks.snapshot(Some(&audience));
        snapshot.before(&pre()).await.expect("before");
        snapshot.after(&post(false)).await.expect("after");
        assert_eq!(before.load(Ordering::SeqCst), 1);
        assert_eq!(after.load(Ordering::SeqCst), 1);

        drop(first);
        drop(second);
        assert!(!hooks.snapshot(Some(&audience)).is_empty());
        drop(cloned);
        assert!(hooks.snapshot(Some(&audience)).is_empty());
    }

    #[test]
    fn shared_mixed_key_rejects_different_batch_order_and_audience() {
        let hooks = ExecutionHooks::new();
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let left: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
            name: "left",
            before: Arc::clone(&before),
            after: Arc::clone(&after),
        });
        let right: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
            name: "right",
            before,
            after,
        });
        let audience = ToolAudience::new("alpha");
        let guard = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(audience.clone()),
                vec![Arc::clone(&left), Arc::clone(&right)],
            )
            .expect("original batch");

        let order_conflict = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(audience),
                vec![Arc::clone(&right), Arc::clone(&left)],
            )
            .expect_err("ordered identity must not silently change");
        assert_eq!(order_conflict.key(), "workspace");

        let audience_conflict = hooks
            .register_live_shared(
                "workspace".to_string(),
                Some(ToolAudience::new("beta")),
                vec![left, right],
            )
            .expect_err("audience is part of registration identity");
        assert_eq!(audience_conflict.key(), "workspace");

        drop(guard);
        assert!(hooks.snapshot(Some(&ToolAudience::new("alpha"))).is_empty());
    }

    struct Denies {
        name: &'static str,
        after: bool,
    }

    #[async_trait]
    impl ExecutionHookParticipant for Denies {
        fn name(&self) -> &str {
            self.name
        }

        async fn before(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<BeforeDecision, RuntimeError> {
            Ok(if self.after {
                BeforeDecision::Continue
            } else {
                BeforeDecision::Deny("no".into())
            })
        }

        async fn after(
            &self,
            _context: &PostExecutionContext,
        ) -> Result<AfterDecision, RuntimeError> {
            Ok(if self.after {
                AfterDecision::Deny("no output".into())
            } else {
                AfterDecision::Continue
            })
        }
    }

    #[tokio::test]
    async fn named_denials_short_circuit_their_remaining_seam() {
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let chain = ExecutionHooks::new()
            .with_participant(Denies {
                name: "before-guard",
                after: false,
            })
            .with_participant(Counts {
                name: "never-before",
                before: Arc::clone(&before),
                after: Arc::clone(&after),
            });
        assert_eq!(
            chain.snapshot(None).before(&pre()).await.expect("before"),
            BeforeDecision::Deny("denied by execution hook 'before-guard': no".into())
        );
        assert_eq!(before.load(Ordering::SeqCst), 0);

        let chain = ExecutionHooks::new()
            .with_participant(Denies {
                name: "after-guard",
                after: true,
            })
            .with_participant(Counts {
                name: "never-after",
                before,
                after: Arc::clone(&after),
            });
        assert_eq!(
            chain
                .snapshot(None)
                .after(&post(false))
                .await
                .expect("after"),
            AfterDecision::Deny("denied by execution hook 'after-guard': no output".into())
        );
        assert_eq!(after.load(Ordering::SeqCst), 0);
    }

    struct BasisLikeFailureAdapter {
        name: &'static str,
        fail_open: bool,
        reported: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ExecutionHookParticipant for BasisLikeFailureAdapter {
        fn name(&self) -> &str {
            self.name
        }

        async fn before(
            &self,
            _context: &PreExecutionContext,
        ) -> Result<BeforeDecision, RuntimeError> {
            self.reported.fetch_add(1, Ordering::SeqCst);
            Ok(if self.fail_open {
                BeforeDecision::Continue
            } else {
                BeforeDecision::Deny("could not answer and denies on failure".into())
            })
        }
    }

    #[tokio::test]
    async fn basis_like_adapters_keep_failure_policy_and_reporting_outside_mentra() {
        let reported = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let chain = ExecutionHooks::new()
            .with_participant(BasisLikeFailureAdapter {
                name: "observer",
                fail_open: true,
                reported: Arc::clone(&reported),
            })
            .with_participant(BasisLikeFailureAdapter {
                name: "guard",
                fail_open: false,
                reported: Arc::clone(&reported),
            })
            .with_participant(Counts {
                name: "never",
                before: Arc::new(AtomicUsize::new(0)),
                after: Arc::clone(&after),
            });

        assert_eq!(
            chain.snapshot(None).before(&pre()).await.expect("before"),
            BeforeDecision::Deny(
                "denied by execution hook 'guard': could not answer and denies on failure".into()
            )
        );
        assert_eq!(reported.load(Ordering::SeqCst), 2);
        assert_eq!(after.load(Ordering::SeqCst), 0);
    }

    struct ReentrantDrop {
        hooks: ExecutionHooks,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for ReentrantDrop {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            let transient: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
                name: "transient",
                before: Arc::new(AtomicUsize::new(0)),
                after: Arc::new(AtomicUsize::new(0)),
            });
            drop(self.hooks.register_live(None, vec![transient]));
        }
    }

    #[async_trait]
    impl ExecutionHookParticipant for ReentrantDrop {
        fn name(&self) -> &str {
            "reentrant"
        }
    }

    #[test]
    fn batch_drop_is_poison_safe_and_destroys_captures_after_unlock() {
        let hooks = ExecutionHooks::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let participant: Arc<dyn ExecutionHookParticipant> = Arc::new(ReentrantDrop {
            hooks: hooks.clone(),
            dropped: Arc::clone(&dropped),
        });
        let guard = hooks.register_live(None, vec![participant]);
        let (done_tx, done_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(guard);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reentrant capture Drop must not deadlock");
        dropper.join().expect("dropper");
        assert!(dropped.load(Ordering::SeqCst));

        let participant: Arc<dyn ExecutionHookParticipant> = Arc::new(Counts {
            name: "poison",
            before: Arc::new(AtomicUsize::new(0)),
            after: Arc::new(AtomicUsize::new(0)),
        });
        let guard = hooks.register_live(None, vec![participant]);
        let registry = guard.inner.registry.upgrade().expect("registry");
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _locked = registry.write().expect("healthy registry");
            panic!("poison registry");
        }));
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| drop(guard))).is_ok());
        assert!(hooks.snapshot(None).is_empty());
    }
}
