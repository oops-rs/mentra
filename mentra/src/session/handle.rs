use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde::de::DeserializeOwned;
use tokio::sync::broadcast;

use crate::{
    AgentTranscript, ContentBlock, Message, Role,
    agent::{
        Agent, AgentEvent, AgentEventTapGuard, DisposableSubagentTemplate, FinalOutput,
        TerminalOutputDecision, TerminalOutputReservation, TerminalOutputSpec,
    },
    error::RuntimeError,
    runtime::{PermissionRuleContext, RunOptions, RuntimeStore, is_transient_runtime_error},
    session::{
        event::{
            EventSeq, PermissionOutcome, PermissionRuleScope, SessionEvent, TaskKind,
            TaskLifecycleStatus,
        },
        mapping::{ToolNameIndex, map_agent_event},
        permission::{
            ClaimedPendingPermission, PendingPermissionEntry, PendingPermissionStore,
            PermissionDecision, PermissionRuleAddress, RememberedRule, RuleKey, RuleStore,
            SessionToolAuthorizer,
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
    context: PermissionRuleContext,
    store: Arc<dyn RuntimeStore>,
    event_tx: broadcast::Sender<SessionEvent>,
    pending_permissions: PendingPermissionStore,
}

impl SessionPermissionHandle {
    pub(crate) fn new(
        agent_id: String,
        project_id: Option<String>,
        store: Arc<dyn RuntimeStore>,
        event_tx: broadcast::Sender<SessionEvent>,
        pending_permissions: PendingPermissionStore,
    ) -> Self {
        Self {
            context: PermissionRuleContext {
                session_id: agent_id,
                project_id,
            },
            store,
            event_tx,
            pending_permissions,
        }
    }

    /// The stable agent/project namespace used by this live session.
    pub fn context(&self) -> &PermissionRuleContext {
        &self.context
    }

    /// Atomically inserts or replaces one rule in its effective namespace.
    pub fn remember_rule(&self, rule: RememberedRule) -> Result<(), RuntimeError> {
        self.store.upsert_rule(&self.context, &rule)
    }

    /// Atomically revokes one exact rule address from its effective namespace.
    pub fn revoke_rule(&self, address: &PermissionRuleAddress) -> Result<bool, RuntimeError> {
        self.store.revoke_rule(&self.context, address)
    }

    /// Atomically clears one effective scope and returns stored rows removed.
    pub fn clear_scope(&self, scope: PermissionRuleScope) -> Result<usize, RuntimeError> {
        self.store.clear_scope(&self.context, scope)
    }

    /// Loads the rules currently applicable to this session in stable order.
    pub fn remembered_rules(&self) -> Result<Vec<RememberedRule>, RuntimeError> {
        self.store.load_applicable_rules(&self.context)
    }

    pub(crate) fn matching_rule(
        &self,
        tool_name: &str,
        input_json: Option<&str>,
    ) -> Result<Option<RememberedRule>, RuntimeError> {
        let rule_store = RuleStore::new();
        for rule in self.remembered_rules()? {
            rule_store.add_rule(rule);
        }
        Ok(rule_store.matching_rule(tool_name, input_json))
    }

    pub(crate) fn event_tx(&self) -> &broadcast::Sender<SessionEvent> {
        &self.event_tx
    }

    pub(crate) fn pending_permissions(&self) -> &PendingPermissionStore {
        &self.pending_permissions
    }

    pub fn resolve_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeError> {
        if let Some(scope) = decision.remember_as {
            self.context.validate_scope(scope)?;
        }

        let claim = self.pending_permissions.claim(request_id).ok_or_else(|| {
            RuntimeError::OperationDenied(format!(
                "no pending permission with request_id '{request_id}'"
            ))
        })?;
        let ClaimedPendingPermission {
            generation,
            lifecycle,
            entry,
        } = claim;
        let mut active = lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !*active || entry.sender.is_closed() {
            *active = false;
            return Err(RuntimeError::OperationDenied(format!(
                "no pending permission with request_id '{request_id}'"
            )));
        }

        let outcome = if decision.allow {
            PermissionOutcome::Allowed
        } else {
            PermissionOutcome::Denied
        };

        if let Some(scope) = decision.remember_as {
            let rule = RememberedRule {
                key: RuleKey {
                    tool_name: entry.tool_name.clone(),
                    pattern: None,
                },
                allow: decision.allow,
                scope,
                // Only a refusal has anything left to say: this call's reason
                // reaches the model as its tool result, and every later call
                // the rule answers has nothing else to read.
                reason: if decision.allow {
                    None
                } else {
                    decision.reason.clone()
                },
            };
            if let Err(error) = self.store.upsert_rule(&self.context, &rule) {
                let restored = self.pending_permissions.restore(
                    request_id.to_owned(),
                    ClaimedPendingPermission {
                        generation,
                        lifecycle: lifecycle.clone(),
                        entry,
                    },
                );
                if !restored {
                    *active = false;
                }
                return Err(error);
            }
        }

        let PendingPermissionEntry {
            tool_call_id,
            tool_name,
            sender,
        } = entry;
        let _ = self.event_tx.send(SessionEvent::PermissionResolved {
            request_id: request_id.to_owned(),
            tool_call_id,
            tool_name,
            outcome,
            rule_scope: decision.remember_as,
        });

        sender.send(decision).map_err(|_| {
            RuntimeError::OperationDenied(format!(
                "no pending permission with request_id '{request_id}'"
            ))
        })?;
        *active = false;
        Ok(())
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
            PendingPermissionStore::new(),
            None,
        )
    }

    pub(crate) fn new_with_parts(
        id: SessionId,
        metadata: SessionMetadata,
        mut agent: Agent,
        event_tx: broadcast::Sender<SessionEvent>,
        pending_permissions: PendingPermissionStore,
        project_id: Option<String>,
    ) -> Self {
        let runtime = agent.runtime_handle();
        let permission_handle = SessionPermissionHandle::new(
            agent.id().to_owned(),
            project_id,
            runtime.store(),
            event_tx.clone(),
            pending_permissions.clone(),
        );
        let authorizer = SessionToolAuthorizer::new(
            runtime.execution.tool_authorizer.clone(),
            permission_handle.clone(),
        );
        agent.replace_tool_authorizer(Arc::new(authorizer));
        Self {
            id,
            metadata,
            agent,
            event_tx,
            next_seq: 0,
            tool_names: Arc::new(StdMutex::new(ToolNameIndex::default())),
            permission_handle,
        }
    }

    /// Returns the session identifier.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Returns the session metadata (title, model, status, turn count, timestamps).
    pub fn metadata(&self) -> &SessionMetadata {
        &self.metadata
    }

    /// Returns the current configuration of this session's underlying agent.
    ///
    /// For a newly resumed session, this is the configuration loaded from the
    /// persisted agent record. Later configuration changes are reflected here,
    /// so this is not an immutable historical snapshot.
    ///
    /// The configuration can contain system prompts, metadata, local paths,
    /// and provider request headers. Callers should not log or otherwise expose
    /// it wholesale.
    pub fn config(&self) -> &crate::agent::AgentConfig {
        self.agent.config()
    }

    /// Returns this live session's ephemeral tool audience, if any.
    pub fn tool_audience(&self) -> Option<&crate::tool::ToolAudience> {
        self.agent.tool_audience()
    }

    /// Replaces the runtime's tool authorizer for this live session.
    ///
    /// The replacement is scoped to this session's agent and descendants
    /// spawned after this call. Sibling sessions and agents created directly
    /// from the runtime keep the runtime's authorizer. The session's own
    /// permission wrapper remains outermost, so a [`crate::tool::ToolAuthorizer`]
    /// that returns [`crate::tool::ToolAuthorizationOutcome::Prompt`] still
    /// consults remembered answers, then emits [`SessionEvent::PermissionRequested`]
    /// and waits for [`resolve_permission`](Self::resolve_permission) when none
    /// matches. `Allow` and `Deny` remain authoritative and are never overridden
    /// by an answer remembered under an earlier policy.
    ///
    /// The attachment is live-only and is not persisted with the agent. Attach
    /// the current authorizer again after resuming a session. A stateful
    /// authorizer may change its own policy between calls without replacing the
    /// session attachment.
    ///
    /// Consume and decorate the session before its first turn or before taking
    /// a disposable-subagent template from it. Already-spawned descendants and
    /// previously captured templates retain the authorizer they inherited when
    /// they were created.
    pub fn with_tool_authorizer<A>(mut self, authorizer: A) -> Self
    where
        A: crate::tool::ToolAuthorizer + 'static,
    {
        let authorizer =
            SessionToolAuthorizer::new(Some(Arc::new(authorizer)), self.permission_handle.clone());
        self.agent.replace_tool_authorizer(Arc::new(authorizer));
        self
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

    /// Returns how many tokens this session's model accepts, when known.
    ///
    /// Reads the agent directly rather than making a host mirror the
    /// `ModelInfo` it last handed over: a mirror desyncs the moment anything
    /// calls [`set_model`](Self::set_model), and this is the value the
    /// compaction threshold is actually computed from.
    pub fn context_window(&self) -> Option<usize> {
        self.agent.context_window()
    }

    /// Returns the reasoning options this session's turns are sent with.
    ///
    /// The reader for [`set_reasoning`](Self::set_reasoning): a picker that
    /// cannot show which effort a session was opened with has to keep its own
    /// copy and hope the two never diverge.
    pub fn reasoning(&self) -> Option<&crate::provider::ReasoningOptions> {
        self.agent.reasoning()
    }

    /// Renames the session and persists the new name.
    ///
    /// A session's name is fixed at creation otherwise, so a host that mints
    /// one before it knows what the conversation is about is stuck with
    /// whatever placeholder it guessed.
    pub fn set_name(&mut self, name: impl Into<String>) -> Result<(), RuntimeError> {
        let name = name.into();
        self.metadata.title = name.clone();
        self.metadata.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.agent.set_name(name)
    }

    /// Compacts this session's transcript now, without waiting for a threshold.
    ///
    /// The model can already ask for this through the `compact` intrinsic; the
    /// person whose session it is could not. `instructions` says what to keep
    /// — "hold on to the migration plan, drop the log spelunking" — and is
    /// added to the standing continuity requirements rather than replacing
    /// them.
    ///
    /// Returns `None` when there was nothing to compact.
    ///
    /// Unbounded: a compaction started here runs to completion. Use
    /// [`compact_with_bounds`](Self::compact_with_bounds) to be able to take
    /// it back.
    pub async fn compact(
        &mut self,
        instructions: Option<&str>,
    ) -> Result<Option<crate::agent::CompactionDetails>, RuntimeError> {
        self.compact_with_bounds(instructions, crate::compaction::CompactionBounds::default())
            .await
    }

    /// Compacts this session's transcript now, under bounds the caller can
    /// trip.
    ///
    /// [`compact`](Self::compact) with a way to stop: a summarization is a
    /// full provider round trip over a long transcript, and a host driving one
    /// from a UI needs the same cancel it has over a turn. Reaching a bound
    /// fails with [`RuntimeError::Cancelled`] or
    /// [`RuntimeError::DeadlineExceeded`] and leaves the transcript exactly as
    /// it was — an abandoned compaction changes nothing.
    ///
    /// `instructions` behave as they do for [`compact`](Self::compact).
    pub async fn compact_with_bounds(
        &mut self,
        instructions: Option<&str>,
        bounds: crate::compaction::CompactionBounds,
    ) -> Result<Option<crate::agent::CompactionDetails>, RuntimeError> {
        // The last turn stays whole, as it does for the intrinsic: compacting
        // the exchange a caller just had is the one thing they did not ask for.
        let preserve_from = self.agent.history().len().saturating_sub(1);
        // A compaction outside a turn still has something to say. The event
        // forwarder is otherwise installed only between `begin_turn` and
        // `finish_turn`, so the agent's `ContextCompacted` had no tap and a
        // host watching the stream saw a transcript shrink with no event
        // explaining it. This is not a turn — no status change, no counter —
        // just the forwarding.
        let (event_tap, forwarded_seq) = self.install_agent_event_forwarder();
        let result = self
            .agent
            .compact_history_with_instructions(
                preserve_from,
                crate::agent::CompactionTrigger::Manual,
                instructions,
                &bounds,
                None,
            )
            .await;
        drop(event_tap);
        self.sync_forwarded_seq(&forwarded_seq);
        self.touch_updated_at();
        result
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

    /// Registers a lossless in-process observer for this session's agent events.
    ///
    /// The callback runs synchronously for every [`AgentEvent`] in occurrence
    /// order, before the event is offered to the agent's bounded broadcast
    /// channel. It therefore does not lag or drop events when a broadcast
    /// receiver falls behind, and it sees complete provider-neutral tool call
    /// and result payloads that the UI-oriented [`SessionEvent`] stream may
    /// summarize.
    ///
    /// The callback executes inline on the operation emitting the event. It
    /// must return promptly and must not block or panic: blocking stalls that
    /// operation, and a panic propagates through it because taps are not an
    /// unwind boundary. It must not re-enter an event-emitting operation on
    /// this session or drop an event-tap guard from inside a callback.
    ///
    /// Keep the returned [`AgentEventTapGuard`] alive for as long as observation
    /// is required. Dropping it unregisters the callback and waits for any
    /// invocation already in flight; registration does not replay events that
    /// happened earlier. Do not drop it while holding a lock or other resource
    /// that an in-flight callback needs.
    pub fn register_agent_event_tap(
        &self,
        tap: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> AgentEventTapGuard {
        self.agent.register_event_tap(tap)
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
        self.emit(user_message_event(&content));

        let turn = self.begin_turn();
        let result = self.agent.run(content, options).await;
        self.finish_turn(turn, result)
    }

    /// Submits a user turn that must end in a typed value, and returns that
    /// value together with the tool-result message that carried it.
    ///
    /// The session-level counterpart to [`Agent::run_to_output`], as
    /// [`append_turn_with_options`](Self::append_turn_with_options) is to
    /// [`Agent::run`]. A host that drives a conversation through a `Session` —
    /// for the event stream and the permission handle — needs a typed final
    /// answer without dropping to the agent and losing both.
    ///
    /// Whether the turn may work on its way to that answer or only shape what
    /// the conversation already holds is the spec's to say, through
    /// [`TerminalOutputSpec::with_tools`]. A working typed turn is where a
    /// `Session` earns the most: its tool calls go to the same event stream
    /// and its permission requests to the same handle as any other turn's, so
    /// a host gets one turn that reads, asks, and answers in a declared shape
    /// without giving up either.
    ///
    /// The turn announces itself on the stream exactly as every other turn
    /// does: a [`SessionEvent::UserMessage`] going in, whatever the agent
    /// emits while it runs, and on success one
    /// [`SessionEvent::AssistantMessageCompleted`] carrying the text of the
    /// turn's final assistant message. For a typed turn that is whatever prose
    /// the model wrote alongside the terminal tool call, which is often
    /// nothing. The value itself is deliberately not put there: it already
    /// reaches the stream as the terminal tool's
    /// [`ToolQueued`](SessionEvent::ToolQueued) input and
    /// [`ToolCompleted`](SessionEvent::ToolCompleted) summary, and a client
    /// that reads `AssistantMessageCompleted` as "what the assistant said"
    /// would render a tool payload as prose. Failure reports the same way any
    /// turn does — [`SessionEvent::Error`] and [`SessionStatus::Failed`].
    ///
    /// One asymmetry with a plain turn is worth knowing: a value that does not
    /// deserialize into `T` fails *after* the agent committed the exchange, so
    /// the transcript holds the terminal call and its result even though this
    /// returns `Err`. The turn counter still does not move, as for any failed
    /// turn.
    pub async fn append_turn_to_output<T: DeserializeOwned>(
        &mut self,
        content: Vec<ContentBlock>,
        options: RunOptions,
        spec: TerminalOutputSpec,
    ) -> Result<FinalOutput<T>, RuntimeError> {
        self.emit(user_message_event(&content));

        let turn = self.begin_turn();
        let result = self.agent.run_to_output::<T>(content, options, spec).await;
        self.finish_turn(turn, result)
    }

    /// Submits a multipart turn whose reserved output is validated before the
    /// generated tool may terminate it.
    pub async fn append_turn_to_reserved_output<T, V>(
        &mut self,
        content: Vec<ContentBlock>,
        options: RunOptions,
        reservation: TerminalOutputReservation,
        validator: V,
    ) -> Result<FinalOutput<T>, RuntimeError>
    where
        T: DeserializeOwned,
        V: Fn(&serde_json::Value) -> TerminalOutputDecision + Send + Sync + 'static,
    {
        self.emit(user_message_event(&content));

        let turn = self.begin_turn();
        let result = self
            .agent
            .run_to_reserved_output::<T, V>(content, options, reservation, validator)
            .await;
        self.finish_turn(turn, result)
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
        let turn = self.begin_turn();
        let result = self.agent.resume_with_options(options).await;
        self.finish_turn(turn, result)
    }

    /// Opens a turn: marks the session active and starts forwarding agent
    /// events onto the session stream.
    ///
    /// Every turn opens here and closes in [`finish_turn`](Self::finish_turn) —
    /// started by a prompt, resumed after a failure, or run to a typed output —
    /// so no turn can report itself differently from the others.
    fn begin_turn(&mut self) -> TurnGuard {
        self.update_status(SessionStatus::Active);
        let (event_tap, forwarded_seq) = self.install_agent_event_forwarder();
        TurnGuard {
            event_tap,
            forwarded_seq,
        }
    }

    /// Closes a turn opened by [`begin_turn`](Self::begin_turn): stops the
    /// forwarder, emits the terminal event, and settles the status, the turn
    /// counter, and `updated_at`.
    fn finish_turn<O: TurnOutcome>(
        &mut self,
        turn: TurnGuard,
        result: Result<O, RuntimeError>,
    ) -> Result<O, RuntimeError> {
        let TurnGuard {
            event_tap,
            forwarded_seq,
        } = turn;
        drop(event_tap);
        self.sync_forwarded_seq(&forwarded_seq);

        match result {
            Ok(outcome) => {
                let text = outcome.completion_text(self.agent.history());
                self.emit(SessionEvent::AssistantMessageCompleted { text });
                self.metadata.turn_count += 1;
                self.update_status(SessionStatus::Idle);
                self.touch_updated_at();
                Ok(outcome)
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
    /// If `remember_as` is set, the one rule is atomically persisted before a
    /// [`SessionEvent::PermissionResolved`] event is emitted and the decision is
    /// sent to the waiting caller. Validation or persistence failure leaves the
    /// pending request available for a corrected retry.
    pub fn resolve_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeError> {
        self.permission_handle
            .resolve_permission(request_id, decision)
    }

    /// Loads all remembered permission rules currently applicable to this
    /// session from its runtime store.
    pub fn remembered_rules(&self) -> Result<Vec<RememberedRule>, RuntimeError> {
        self.permission_handle.remembered_rules()
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
    ///
    /// The subagent runs on [`RunOptions::default`]: this is a host-initiated
    /// spawn with no session turn necessarily in flight, so there is no parent
    /// run whose bounds it could inherit. A host that wants the subagent to
    /// share a turn's cancellation and token accounting passes that turn's
    /// [`RunOptions::child`] to
    /// [`spawn_subagent_with_options`](Self::spawn_subagent_with_options)
    /// instead. This is the opposite of the model-facing `task` intrinsic,
    /// which always inherits because it can only run *inside* a parent run.
    pub async fn spawn_subagent(
        &mut self,
        name: &str,
        prompt: &str,
    ) -> Result<SubagentHandle, RuntimeError> {
        self.spawn_subagent_with_options(name, prompt, RunOptions::default())
            .await
    }

    /// Spawns a disposable subagent that runs on caller-supplied `options`.
    ///
    /// Pass a turn's [`RunOptions::child`] to put the subagent under that
    /// turn's cancellation, stop, deadline, and shared token accounting. The
    /// subagent is detached, so those bounds are the only thing tying its
    /// lifetime to the turn's.
    pub async fn spawn_subagent_with_options(
        &mut self,
        name: &str,
        prompt: &str,
        options: RunOptions,
    ) -> Result<SubagentHandle, RuntimeError> {
        // Delegates through the template-taking path on an un-overridden
        // template rather than duplicating it: this is what makes "spawns
        // byte-identically to spawn_subagent_from with a plain template" true
        // by construction instead of by two implementations staying in sync.
        let template = self.disposable_subagent_template();
        self.spawn_subagent_from_with_options(name, prompt, template, options)
            .await
    }

    /// A template cloned from this session's agent's own config exactly as it
    /// stands at this call — the same snapshot
    /// [`spawn_subagent`](Self::spawn_subagent) takes when it is called — for
    /// a host that wants to override the child's tool profile, model, or
    /// system prompt before spawning it via
    /// [`spawn_subagent_from`](Self::spawn_subagent_from) or
    /// [`spawn_subagent_from_with_options`](Self::spawn_subagent_from_with_options).
    /// A later change to the agent (e.g. `set_model`) does not reach back
    /// into a template already taken.
    pub fn disposable_subagent_template(&self) -> DisposableSubagentTemplate {
        self.agent.disposable_subagent_template()
    }

    /// Forwards to [`spawn_subagent_from_with_options`](Self::spawn_subagent_from_with_options)
    /// on [`RunOptions::default`], after verifying the template's source.
    pub async fn spawn_subagent_from(
        &mut self,
        name: &str,
        prompt: &str,
        template: DisposableSubagentTemplate,
    ) -> Result<SubagentHandle, RuntimeError> {
        self.spawn_subagent_from_with_options(name, prompt, template, RunOptions::default())
            .await
    }

    /// Spawns through the agent's source-verifying template path, then detaches
    /// the result the same way
    /// [`spawn_subagent_with_options`](Self::spawn_subagent_with_options) does.
    pub async fn spawn_subagent_from_with_options(
        &mut self,
        name: &str,
        prompt: &str,
        template: DisposableSubagentTemplate,
        options: RunOptions,
    ) -> Result<SubagentHandle, RuntimeError> {
        let subagent = self.agent.spawn_subagent_from(template).await?;
        self.detach_subagent(name, prompt, subagent, options)
    }

    /// Registers, announces, and detaches an already-spawned subagent — the
    /// shared tail of every `spawn_subagent*` variant, which differ only in
    /// how the [`Agent`] they hand here was built.
    fn detach_subagent(
        &mut self,
        name: &str,
        prompt: &str,
        mut subagent: Agent,
        options: RunOptions,
    ) -> Result<SubagentHandle, RuntimeError> {
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
                .run(vec![ContentBlock::Text { text: prompt_text }], options)
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

/// The bookkeeping a turn holds open while it runs: the agent-event tap that
/// forwards onto the session stream, and the sequence counter the tap advances
/// behind it.
#[must_use = "a turn opened with `begin_turn` must be closed with `finish_turn`"]
struct TurnGuard {
    event_tap: AgentEventTapGuard,
    forwarded_seq: Arc<StdMutex<EventSeq>>,
}

/// What a successful turn puts in its terminal
/// [`SessionEvent::AssistantMessageCompleted`].
///
/// Both shapes a turn can return resolve to one rule — the text of the turn's
/// final assistant message — so the stream reads the same whether the turn
/// returned that message itself or a typed value extracted from the tool
/// result that followed it.
trait TurnOutcome {
    fn completion_text(&self, history: &[Message]) -> String;
}

impl TurnOutcome for Message {
    /// A prompted or resumed turn returns the final assistant message itself.
    fn completion_text(&self, _history: &[Message]) -> String {
        self.text()
    }
}

impl<T> TurnOutcome for FinalOutput<T> {
    /// A typed turn ends on a tool-result message, so its final assistant
    /// message is the one that carried the terminal call — the last assistant
    /// message in the committed history.
    fn completion_text(&self, history: &[Message]) -> String {
        history
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text)
            .unwrap_or_default()
    }
}

/// Builds the event announcing a submitted user turn.
///
/// The image count rides along because a turn can be images alone: a client
/// rendering only `text` would show a blank message where a screenshot was.
fn user_message_event(content: &[ContentBlock]) -> SessionEvent {
    SessionEvent::UserMessage {
        text: extract_user_text(content),
        image_count: content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count(),
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
