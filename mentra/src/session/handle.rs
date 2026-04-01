use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tokio::sync::broadcast;

use crate::{
    AgentTranscript, ContentBlock, Message,
    agent::{Agent, AgentEvent},
    error::RuntimeError,
    runtime::{PermissionRuleStore, is_transient_runtime_error},
    session::{
        event::{EventSeq, PermissionOutcome, SessionEvent},
        mapping::map_agent_event,
        permission::{
            PendingPermissionStore, PermissionDecision, RememberedRule, RuleKey, RuleStore,
        },
        types::{SessionId, SessionMetadata, SessionStatus},
    },
};

/// Type alias for the receiver end of the session event broadcast channel.
pub type SessionEventReceiver = broadcast::Receiver<SessionEvent>;

#[derive(Clone)]
pub struct SessionPermissionHandle {
    session_id: SessionId,
    event_tx: broadcast::Sender<SessionEvent>,
    rule_store: RuleStore,
    permission_store: Arc<StdMutex<Option<Arc<dyn PermissionRuleStore>>>>,
    pending_permissions: PendingPermissionStore,
}

impl SessionPermissionHandle {
    fn new(
        session_id: SessionId,
        event_tx: broadcast::Sender<SessionEvent>,
        rule_store: RuleStore,
        permission_store: Arc<StdMutex<Option<Arc<dyn PermissionRuleStore>>>>,
        pending_permissions: PendingPermissionStore,
    ) -> Self {
        Self {
            session_id,
            event_tx,
            rule_store,
            permission_store,
            pending_permissions,
        }
    }

    fn set_permission_store(&self, store: Arc<dyn PermissionRuleStore>) {
        let mut slot = self
            .permission_store
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(store);
    }

    fn load_persisted_rules(&self, session_id: &SessionId) -> Result<usize, RuntimeError> {
        let store = self
            .permission_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(store) = store else {
            return Ok(0);
        };
        let rules = store.load_rules(session_id.as_str())?;
        let count = rules.len();
        for rule in rules {
            self.rule_store.add_rule(rule);
        }
        Ok(count)
    }

    pub fn resolve_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeError> {
        let entry = self.pending_permissions.remove(request_id).ok_or_else(|| {
            RuntimeError::OperationDenied(format!(
                "no pending permission with request_id '{request_id}'"
            ))
        })?;

        let outcome = if decision.allow {
            PermissionOutcome::Allowed
        } else {
            PermissionOutcome::Denied
        };

        if let Some(scope) = decision.remember_as {
            self.rule_store.add_rule(RememberedRule {
                key: RuleKey {
                    tool_name: entry.tool_name.clone(),
                    pattern: None,
                },
                allow: decision.allow,
                scope,
            });

            let store = self
                .permission_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(store) = store {
                let all_rules = self.rule_store.rules();
                store.save_rules(self.session_id.as_str(), &all_rules)?;
            }
        }

        let _ = self.event_tx.send(SessionEvent::PermissionResolved {
            request_id: request_id.to_owned(),
            tool_call_id: entry.tool_call_id,
            tool_name: entry.tool_name,
            outcome,
            rule_scope: decision.remember_as,
        });

        let _ = entry.sender.send(decision);
        Ok(())
    }

    pub(crate) fn remembered_rules(&self) -> Vec<RememberedRule> {
        self.rule_store.rules()
    }

    pub(crate) fn rule_store(&self) -> &RuleStore {
        &self.rule_store
    }
}

/// A `Session` wraps an [`Agent`] with session-level metadata and a broadcast
/// event channel that emits [`SessionEvent`] values for UI consumption.
pub struct Session {
    id: SessionId,
    metadata: SessionMetadata,
    agent: Agent,
    event_tx: broadcast::Sender<SessionEvent>,
    next_seq: EventSeq,
    #[allow(dead_code)]
    pub(crate) pending_permissions: PendingPermissionStore,
    permission_handle: SessionPermissionHandle,
}

impl Session {
    /// Creates a new session wrapping the given agent.
    #[allow(dead_code)]
    pub(crate) fn new(id: SessionId, metadata: SessionMetadata, agent: Agent) -> Self {
        let (event_tx, _) = broadcast::channel(512);
        Self::new_with_parts(
            id,
            metadata,
            agent,
            event_tx,
            RuleStore::new(),
            PendingPermissionStore::new(),
        )
    }

    pub(crate) fn new_with_parts(
        id: SessionId,
        metadata: SessionMetadata,
        agent: Agent,
        event_tx: broadcast::Sender<SessionEvent>,
        rule_store: RuleStore,
        pending_permissions: PendingPermissionStore,
    ) -> Self {
        let permission_store = Arc::new(StdMutex::new(None));
        let permission_handle = SessionPermissionHandle::new(
            id.clone(),
            event_tx.clone(),
            rule_store.clone(),
            permission_store.clone(),
            pending_permissions.clone(),
        );
        Self {
            id,
            metadata,
            agent,
            event_tx,
            next_seq: 0,
            pending_permissions,
            permission_handle,
        }
    }

    /// Attaches a persistent permission rule store to this session.
    ///
    /// When set, remembered rules are saved to the store on each decision and
    /// can be loaded on session resume via [`load_persisted_rules`](Self::load_persisted_rules).
    pub fn set_permission_store(&mut self, store: Arc<dyn PermissionRuleStore>) {
        self.permission_handle.set_permission_store(store);
    }

    /// Loads persisted permission rules from the attached store into the
    /// in-memory [`RuleStore`].
    ///
    /// This is typically called during session resume to restore rules that were
    /// persisted in a prior session run. Returns the number of rules loaded.
    pub fn load_persisted_rules(&mut self) -> Result<usize, RuntimeError> {
        self.permission_handle.load_persisted_rules(&self.id)
    }

    /// Returns the session identifier.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Returns the session metadata (title, model, status, turn count, timestamps).
    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    /// Returns the underlying agent identifier.
    pub fn agent_id(&self) -> &str {
        self.agent.id()
    }

    /// Returns the session display name (same as the agent name).
    pub fn name(&self) -> &str {
        self.agent.name()
    }

    /// Subscribes to the session event stream.
    pub fn subscribe(&self) -> SessionEventReceiver {
        self.event_tx.subscribe()
    }

    pub fn permission_handle(&self) -> SessionPermissionHandle {
        self.permission_handle.clone()
    }

    /// Submits a user turn, runs the agent, emits session events, and returns
    /// the assistant response message.
    pub async fn append_turn(
        &mut self,
        content: Vec<ContentBlock>,
    ) -> Result<Message, RuntimeError> {
        let user_text = extract_user_text(&content);
        self.emit(SessionEvent::UserMessage { text: user_text });
        self.update_status(SessionStatus::Active);

        let mut agent_rx = self.agent.subscribe_events();

        let result = self.agent.send(content).await;

        // Drain agent events and map to session events.
        self.drain_agent_events(&mut agent_rx);

        match result {
            Ok(message) => {
                self.emit(SessionEvent::AssistantMessageCompleted {
                    text: message.text(),
                });
                self.metadata.turn_count += 1;
                self.update_status(SessionStatus::Idle);
                self.touch_updated_at();
                Ok(message)
            }
            Err(error) => {
                let recoverable = is_transient_runtime_error(&error);
                self.emit(SessionEvent::Error {
                    message: error.to_string(),
                    recoverable,
                });
                self.update_status(SessionStatus::Failed(error.to_string()));
                self.touch_updated_at();
                Err(error)
            }
        }
    }

    /// Returns the agent's canonical transcript for UI reconstruction.
    pub fn replay(&self) -> &AgentTranscript {
        self.agent.transcript()
    }

    /// Resumes the agent from an interrupted or failed state, emitting session
    /// events as the turn runs.
    pub async fn resume_turn(&mut self) -> Result<Message, RuntimeError> {
        self.update_status(SessionStatus::Active);

        let mut agent_rx = self.agent.subscribe_events();

        let result = self.agent.resume().await;

        self.drain_agent_events(&mut agent_rx);

        match result {
            Ok(message) => {
                self.emit(SessionEvent::AssistantMessageCompleted {
                    text: message.text(),
                });
                self.metadata.turn_count += 1;
                self.update_status(SessionStatus::Idle);
                self.touch_updated_at();
                Ok(message)
            }
            Err(error) => {
                let recoverable = is_transient_runtime_error(&error);
                self.emit(SessionEvent::Error {
                    message: error.to_string(),
                    recoverable,
                });
                self.update_status(SessionStatus::Failed(error.to_string()));
                self.touch_updated_at();
                Err(error)
            }
        }
    }

    /// Returns the committed message history.
    pub fn history(&self) -> &[Message] {
        self.agent.history()
    }

    /// Emits the initial `SessionStarted` event. Used by `Runtime::create_session`.
    pub(crate) fn emit_started(&mut self, event: SessionEvent) {
        self.emit(event);
    }

    /// Resolves a pending permission request with the given decision.
    ///
    /// If `remember_as` is set on the decision, the rule is stored in the
    /// session's [`RuleStore`]. A [`SessionEvent::PermissionResolved`] event is
    /// emitted and the decision is sent back to the waiting caller via oneshot.
    pub fn resolve_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeError> {
        self.permission_handle
            .resolve_permission(request_id, decision)
    }

    /// Returns all remembered permission rules for this session.
    pub fn remembered_rules(&self) -> Vec<RememberedRule> {
        self.permission_handle.remembered_rules()
    }

    /// Returns a reference to the session's rule store.
    pub fn rule_store(&self) -> &RuleStore {
        self.permission_handle.rule_store()
    }

    // -- internal helpers --

    fn emit(&mut self, event: SessionEvent) {
        // Ignore send errors — there may be no active subscribers.
        let _ = self.event_tx.send(event);
        self.next_seq += 1;
    }

    fn drain_agent_events(&mut self, rx: &mut broadcast::Receiver<AgentEvent>) {
        while let Ok(agent_event) = rx.try_recv() {
            let mapped = map_agent_event(&agent_event, &mut self.next_seq);
            for (_seq, session_event) in mapped {
                let _ = self.event_tx.send(session_event);
            }
        }
    }

    fn update_status(&mut self, status: SessionStatus) {
        self.metadata.status = status;
    }

    fn touch_updated_at(&mut self) {
        self.metadata.updated_at = unix_now();
    }
}

fn extract_user_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
