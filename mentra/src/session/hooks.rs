use tokio::sync::broadcast;

use crate::{
    runtime::{AuditStore, RuntimeError, RuntimeHook, RuntimeHookEvent},
    session::event::{NoticeSeverity, SessionEvent},
};

/// A [`RuntimeHook`] that forwards memory ingest events into the session event
/// broadcast channel as [`SessionEvent::MemoryUpdated`] or
/// [`SessionEvent::Notice`] events.
///
/// This hook is the bridge between the low-level runtime hook system and the
/// session-level event stream consumed by UI layers.
///
/// It is registered per session, on the session's own [`RuntimeHandle`] clone
/// — never on the runtime's shared hook list, where it would forward every
/// agent's memory activity into the one channel it holds.
///
/// [`RuntimeHandle`]: crate::runtime::RuntimeHandle
pub(crate) struct SessionHookBridge {
    tx: broadcast::Sender<SessionEvent>,
}

impl SessionHookBridge {
    /// Creates a new bridge that sends session events into the given sender.
    pub(crate) fn new(tx: broadcast::Sender<SessionEvent>) -> Self {
        Self { tx }
    }
}

impl RuntimeHook for SessionHookBridge {
    fn on_event(
        &self,
        _store: &dyn AuditStore,
        event: &RuntimeHookEvent,
    ) -> Result<(), RuntimeError> {
        match event {
            RuntimeHookEvent::MemoryIngestFinished {
                success: true,
                stored_records,
                agent_id,
                ..
            } => {
                // Ignore send errors — there may be no active subscribers.
                let _ = self.tx.send(SessionEvent::MemoryUpdated {
                    agent_id: agent_id.clone(),
                    stored_records: *stored_records,
                });
            }
            RuntimeHookEvent::MemoryIngestFinished {
                success: false,
                agent_id,
                error,
                ..
            } => {
                let message = error.as_deref().unwrap_or("memory ingest failed");
                let _ = self.tx.send(SessionEvent::Notice {
                    severity: NoticeSeverity::Warning,
                    message: format!("agent '{agent_id}': {message}"),
                });
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "tests use unwrap to fail immediately on invalid fixtures"
)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;
    use crate::runtime::RuntimeHookEvent;
    use crate::session::event::SessionEvent;

    struct NoopAuditStore;

    impl crate::runtime::AuditStore for NoopAuditStore {
        fn record_audit_event(
            &self,
            _scope: &str,
            _event_type: &str,
            _payload: serde_json::Value,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    fn make_bridge() -> (SessionHookBridge, broadcast::Receiver<SessionEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (SessionHookBridge::new(tx), rx)
    }

    #[test]
    fn memory_ingest_finished_success_emits_memory_updated() {
        let (bridge, mut rx) = make_bridge();
        let store = NoopAuditStore;

        let event = RuntimeHookEvent::MemoryIngestFinished {
            agent_id: "agent-1".to_string(),
            source_revision: 42,
            success: true,
            stored_records: 7,
            error: None,
        };

        bridge.on_event(&store, &event).unwrap();

        let received = rx.try_recv().unwrap();
        assert!(
            matches!(
                &received,
                SessionEvent::MemoryUpdated { agent_id, stored_records }
                if agent_id == "agent-1" && *stored_records == 7
            ),
            "Expected MemoryUpdated, got: {received:?}"
        );
    }

    #[test]
    fn memory_ingest_finished_failure_emits_notice_warning() {
        let (bridge, mut rx) = make_bridge();
        let store = NoopAuditStore;

        let event = RuntimeHookEvent::MemoryIngestFinished {
            agent_id: "agent-2".to_string(),
            source_revision: 1,
            success: false,
            stored_records: 0,
            error: Some("disk full".to_string()),
        };

        bridge.on_event(&store, &event).unwrap();

        let received = rx.try_recv().unwrap();
        assert!(
            matches!(
                &received,
                SessionEvent::Notice { severity: NoticeSeverity::Warning, message }
                if message.contains("agent-2") && message.contains("disk full")
            ),
            "Expected Warning Notice, got: {received:?}"
        );
    }

    #[test]
    fn memory_ingest_failure_without_error_uses_fallback_message() {
        let (bridge, mut rx) = make_bridge();
        let store = NoopAuditStore;

        let event = RuntimeHookEvent::MemoryIngestFinished {
            agent_id: "agent-3".to_string(),
            source_revision: 1,
            success: false,
            stored_records: 0,
            error: None,
        };

        bridge.on_event(&store, &event).unwrap();

        let received = rx.try_recv().unwrap();
        assert!(
            matches!(
                &received,
                SessionEvent::Notice { severity: NoticeSeverity::Warning, message }
                if message.contains("memory ingest failed")
            ),
            "Expected fallback message, got: {received:?}"
        );
    }

    #[test]
    fn non_memory_events_are_silently_ignored() {
        let (bridge, mut rx) = make_bridge();
        let store = NoopAuditStore;

        let event = RuntimeHookEvent::RunAborted {
            agent_id: "agent-1".to_string(),
            reason: "timeout".to_string(),
        };

        bridge.on_event(&store, &event).unwrap();

        assert!(
            rx.try_recv().is_err(),
            "Expected no event emitted for non-memory hook events"
        );
    }

    #[test]
    fn memory_updated_event_serialization_roundtrip() {
        let event = SessionEvent::MemoryUpdated {
            agent_id: "agent-1".to_string(),
            stored_records: 12,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "memory_updated");
        assert_eq!(json["agent_id"], "agent-1");
        assert_eq!(json["stored_records"], 12);

        let deserialized: SessionEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event, deserialized);
    }
}
