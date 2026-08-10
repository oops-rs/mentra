use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tokio::sync::broadcast;

use crate::{
    AgentTranscript, ContentBlock, Message,
    agent::{Agent, AgentEvent, AgentEventTapGuard},
    error::RuntimeError,
    runtime::{PermissionRuleStore, RunOptions, is_transient_runtime_error},
    session::{
        event::{EventSeq, PermissionOutcome, SessionEvent, TaskKind, TaskLifecycleStatus},
        mapping::{ToolNameIndex, map_agent_event},
        permission::{
            PendingPermissionStore, PermissionDecision, RememberedRule, RuleKey, RuleStore,
        },
        types::{SessionId, SessionMetadata, SessionStatus},
    },
    transcript::{EntryId, TranscriptItem},
};

/// Type alias for the receiver end of the session event broadcast channel.
pub type SessionEventReceiver = broadcast::Receiver<SessionEvent>;

/// Handle returned from `Session::spawn_subagent` for tracking spawned work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentHandle {
    /// Unique identifier for the spawned task.
    pub task_id: String,
    /// The subagent's internal agent identifier.
    pub agent_id: String,
}

#[derive(Clone)]
pub struct SessionPermissionHandle {
    session_id: SessionId,
    project_id: Option<String>,
    event_tx: broadcast::Sender<SessionEvent>,
    rule_store: RuleStore,
    permission_store: Arc<StdMutex<Option<Arc<dyn PermissionRuleStore>>>>,
    pending_permissions: PendingPermissionStore,
}

impl SessionPermissionHandle {
    fn new(
        session_id: SessionId,
        project_id: Option<String>,
        event_tx: broadcast::Sender<SessionEvent>,
        rule_store: RuleStore,
        permission_store: Arc<StdMutex<Option<Arc<dyn PermissionRuleStore>>>>,
        pending_permissions: PendingPermissionStore,
    ) -> Self {
        Self {
            session_id,
            project_id,
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
        let rules = store.load_rules(session_id.as_str(), self.project_id.as_deref())?;
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
                store.save_rules(
                    self.session_id.as_str(),
                    self.project_id.as_deref(),
                    &all_rules,
                )?;
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
    /// Shared with the per-turn event tap so a tool call queued in one turn
    /// still resolves its name when the result arrives in another.
    tool_names: Arc<StdMutex<ToolNameIndex>>,
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
            None,
        )
    }

    pub(crate) fn new_with_parts(
        id: SessionId,
        metadata: SessionMetadata,
        agent: Agent,
        event_tx: broadcast::Sender<SessionEvent>,
        rule_store: RuleStore,
        pending_permissions: PendingPermissionStore,
        project_id: Option<String>,
    ) -> Self {
        let permission_store = Arc::new(StdMutex::new(None));
        let permission_handle = SessionPermissionHandle::new(
            id.clone(),
            project_id,
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
            tool_names: Arc::new(StdMutex::new(ToolNameIndex::default())),
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

    /// Updates the live session model and persists the new setting so future
    /// resumes observe the same model.
    pub fn set_model(&mut self, model: crate::ModelInfo) -> Result<(), RuntimeError> {
        self.agent.set_model(model.clone())?;
        self.metadata.model = model.id;
        self.metadata.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(())
    }

    /// Updates the reasoning options for the live session's agent and persists the
    /// new setting (mirrors [`set_model`](Self::set_model)). Lets a caller run
    /// per-phase tiering — e.g. a low effort while gathering, then a higher effort
    /// for a final synthesis turn — on the same session.
    pub fn set_reasoning(
        &mut self,
        reasoning: Option<crate::provider::ReasoningOptions>,
    ) -> Result<(), RuntimeError> {
        self.agent.set_reasoning(reasoning)
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
        self.append_turn_with_options(content, RunOptions::default())
            .await
    }

    /// Submits a user turn with explicit execution limits and cancellation
    /// settings.
    ///
    /// The session-level counterpart to [`Agent::run`]. A host that drives a
    /// conversation through a `Session` — for the event stream and the
    /// permission handle — needs the same control over a turn that
    /// [`Agent::run`] gives, without dropping to the agent and losing both.
    /// Cancelling through [`RunOptions::cancellation`] fails the turn and rolls
    /// it back; [`RunOptions::stop`] ends it gracefully at the next round
    /// boundary, keeping the committed transcript.
    pub async fn append_turn_with_options(
        &mut self,
        content: Vec<ContentBlock>,
        options: RunOptions,
    ) -> Result<Message, RuntimeError> {
        let user_text = extract_user_text(&content);
        self.emit(SessionEvent::UserMessage { text: user_text });

        self.run_turn(|agent| Box::pin(agent.run(content, options)))
            .await
    }

    /// Returns the agent's canonical transcript for UI reconstruction.
    pub fn replay(&self) -> &AgentTranscript {
        self.agent.transcript()
    }

    /// The entry the next turn will continue from.
    pub fn leaf(&self) -> Option<&EntryId> {
        self.agent.leaf()
    }

    /// Returns to an earlier entry so the next turn takes a different path.
    ///
    /// This is how "undo that exchange and try something else" works without
    /// starting a new session: the entries after `entry` leave the active
    /// path but stay in the transcript, so the abandoned line of work is
    /// still addressable through [`children`](Self::children). Emits
    /// [`SessionEvent::Branched`] with the number of entries that moved.
    pub fn branch_from(&mut self, entry: &EntryId) -> Result<usize, RuntimeError> {
        let moved = self.agent.branch_from(entry)?;
        self.emit(SessionEvent::Branched {
            entry_id: entry.to_string(),
            abandoned_entries: moved,
        });
        self.touch_updated_at();
        Ok(moved)
    }

    /// The entries recorded as continuing from `entry`, in creation order.
    /// More than one means the conversation branched there.
    pub fn children(&self, entry: &EntryId) -> Vec<&TranscriptItem> {
        self.agent.children(entry)
    }

    /// Resumes the agent from an interrupted or failed state, emitting session
    /// events as the turn runs.
    pub async fn resume_turn(&mut self) -> Result<Message, RuntimeError> {
        self.resume_turn_with_options(RunOptions::default()).await
    }

    /// Resumes an interrupted or failed turn with explicit execution limits and
    /// cancellation settings.
    pub async fn resume_turn_with_options(
        &mut self,
        options: RunOptions,
    ) -> Result<Message, RuntimeError> {
        self.run_turn(|agent| Box::pin(agent.resume_with_options(options)))
            .await
    }

    /// Runs one turn on the agent, wrapped in the bookkeeping every turn needs:
    /// status transitions, the event tap that forwards agent events onto the
    /// session stream, the turn counter, and the terminal event.
    ///
    /// Both entry points share this so a turn started by a prompt and a turn
    /// resumed after a failure can never report themselves differently.
    async fn run_turn<F>(&mut self, run: F) -> Result<Message, RuntimeError>
    where
        F: for<'a> FnOnce(
            &'a mut Agent,
        ) -> Pin<
            Box<dyn Future<Output = Result<Message, RuntimeError>> + Send + 'a>,
        >,
    {
        self.update_status(SessionStatus::Active);

        let (event_tap, forwarded_seq) = self.install_agent_event_forwarder();
        let result = run(&mut self.agent).await;
        drop(event_tap);
        self.sync_forwarded_seq(&forwarded_seq);

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

    /// Returns summaries of all teammates registered with this session's agent.
    pub fn list_teammates(&self) -> Vec<crate::team::TeamMemberSummary> {
        self.agent.watch_snapshot().borrow().teammates.clone()
    }

    /// Returns summaries of all active or recently completed subagents.
    pub fn active_subagents(&self) -> Vec<crate::agent::SpawnedAgentSummary> {
        self.agent.watch_snapshot().borrow().subagents.clone()
    }

    /// Spawns a disposable subagent in the background and returns a handle for tracking it.
    ///
    /// The subagent is registered with the parent agent, a `SubagentSpawned` event is emitted,
    /// and the subagent runs its prompt in a detached `tokio::spawn`. When it completes, a
    /// `SessionEvent::TaskUpdated` event is broadcast with the final status.
    pub async fn spawn_subagent(
        &mut self,
        name: &str,
        prompt: &str,
    ) -> Result<SubagentHandle, RuntimeError> {
        let mut subagent = self.agent.spawn_subagent()?;
        let agent_id = subagent.id().to_string();
        let summary = self.agent.register_subagent(&subagent);

        self.agent.emit_event(AgentEvent::SubagentSpawned {
            agent: summary.clone(),
        });

        let handle = SubagentHandle {
            task_id: agent_id.clone(),
            agent_id: agent_id.clone(),
        };

        let event_tx = self.event_tx.clone();
        let task_name = name.to_string();
        let prompt_text = prompt.to_string();

        tokio::spawn(async move {
            let result = subagent
                .send(vec![ContentBlock::Text { text: prompt_text }])
                .await;

            let (status, detail) = match &result {
                Ok(msg) => (TaskLifecycleStatus::Finished, Some(msg.text())),
                Err(e) => (TaskLifecycleStatus::Failed, Some(e.to_string())),
            };

            let _ = event_tx.send(SessionEvent::TaskUpdated {
                task_id: agent_id,
                kind: TaskKind::Subagent,
                status,
                title: task_name,
                detail,
            });
        });

        Ok(handle)
    }

    // -- internal helpers --

    fn emit(&mut self, event: SessionEvent) {
        // Ignore send errors — there may be no active subscribers.
        let _ = self.event_tx.send(event);
        self.next_seq += 1;
    }

    fn install_agent_event_forwarder(&mut self) -> (AgentEventTapGuard, Arc<StdMutex<EventSeq>>) {
        let event_tx = self.event_tx.clone();
        let next_seq = Arc::new(StdMutex::new(self.next_seq));
        let next_seq_for_tap = Arc::clone(&next_seq);
        let tool_names = Arc::clone(&self.tool_names);
        let event_tap = self.agent.register_event_tap(move |agent_event| {
            let mut seq = next_seq_for_tap
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut names = tool_names.lock().unwrap_or_else(|error| error.into_inner());
            let mapped = map_agent_event(agent_event, &mut seq, &mut names);
            for (_seq, session_event) in mapped {
                let _ = event_tx.send(session_event);
            }
        });
        (event_tap, next_seq)
    }

    fn sync_forwarded_seq(&mut self, next_seq: &Arc<StdMutex<EventSeq>>) {
        self.next_seq = *next_seq.lock().unwrap_or_else(|error| error.into_inner());
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
