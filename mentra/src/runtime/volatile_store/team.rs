use std::{collections::HashMap, path::Path};

use crate::{
    runtime::RuntimeError,
    team::{TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary, TeamStore},
};

use super::{DeliveryState, VolatileRuntimeStore, path_key};

struct TeamInboxEntry {
    team_dir: String,
    recipient: String,
    message: TeamMessage,
    delivery_state: DeliveryState,
}

/// Team roster, inbox, and protocol-request state namespaced by the
/// caller-supplied `team_dir` path, mirroring the default store's
/// `team_members` / `team_inbox` / `team_requests` tables.
#[derive(Default)]
pub(super) struct TeamState {
    members: HashMap<(String, String), TeamMemberSummary>,
    inbox: Vec<TeamInboxEntry>,
    requests: HashMap<String, (String, TeamProtocolRequestSummary)>,
}

impl TeamStore for VolatileRuntimeStore {
    fn unread_team_count(&self, team_dir: &Path, agent_name: &str) -> Result<usize, RuntimeError> {
        let state = self.lock();
        let key = path_key(team_dir);
        Ok(state
            .team
            .inbox
            .iter()
            .filter(|entry| {
                entry.team_dir == key
                    && entry.recipient == agent_name
                    && entry.delivery_state == DeliveryState::Pending
            })
            .count())
    }

    fn load_team_members(&self, team_dir: &Path) -> Result<Vec<TeamMemberSummary>, RuntimeError> {
        let state = self.lock();
        let key = path_key(team_dir);
        let mut members: Vec<_> = state
            .team
            .members
            .iter()
            .filter(|((dir, _), _)| dir == &key)
            .map(|(_, summary)| summary.clone())
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(members)
    }

    fn upsert_team_member(
        &self,
        team_dir: &Path,
        summary: &TeamMemberSummary,
    ) -> Result<(), RuntimeError> {
        self.lock()
            .team
            .members
            .insert((path_key(team_dir), summary.name.clone()), summary.clone());
        Ok(())
    }

    fn read_team_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        let mut state = self.lock();
        let key = path_key(team_dir);
        let mut out = Vec::new();
        for entry in state.team.inbox.iter_mut() {
            if entry.team_dir == key
                && entry.recipient == agent_name
                && entry.delivery_state == DeliveryState::Pending
            {
                entry.delivery_state = DeliveryState::Inflight;
                out.push(entry.message.clone());
            }
        }
        Ok(out)
    }

    fn ack_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        let key = path_key(team_dir);
        for entry in state.team.inbox.iter_mut() {
            if entry.team_dir == key
                && entry.recipient == agent_name
                && entry.delivery_state == DeliveryState::Inflight
            {
                entry.delivery_state = DeliveryState::Acked;
            }
        }
        Ok(())
    }

    fn requeue_team_inbox(&self, team_dir: &Path, agent_name: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        let key = path_key(team_dir);
        for entry in state.team.inbox.iter_mut() {
            if entry.team_dir == key
                && entry.recipient == agent_name
                && entry.delivery_state == DeliveryState::Inflight
            {
                entry.delivery_state = DeliveryState::Pending;
            }
        }
        Ok(())
    }

    fn append_team_message(
        &self,
        team_dir: &Path,
        recipient: &str,
        message: &TeamMessage,
    ) -> Result<(), RuntimeError> {
        self.lock().team.inbox.push(TeamInboxEntry {
            team_dir: path_key(team_dir),
            recipient: recipient.to_string(),
            message: message.clone(),
            delivery_state: DeliveryState::Pending,
        });
        Ok(())
    }

    fn load_team_requests(
        &self,
        team_dir: &Path,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        let state = self.lock();
        let key = path_key(team_dir);
        let mut requests: Vec<_> = state
            .team
            .requests
            .values()
            .filter(|(dir, _)| dir == &key)
            .map(|(_, request)| request.clone())
            .collect();
        requests.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.request_id.cmp(&b.request_id))
        });
        Ok(requests)
    }

    fn upsert_team_request(
        &self,
        team_dir: &Path,
        request: &TeamProtocolRequestSummary,
    ) -> Result<(), RuntimeError> {
        self.lock().team.requests.insert(
            request.request_id.clone(),
            (path_key(team_dir), request.clone()),
        );
        Ok(())
    }

    fn list_team_agent_names(&self, team_dir: &Path) -> Result<Vec<String>, RuntimeError> {
        let state = self.lock();
        let key = path_key(team_dir);
        let mut names: Vec<_> = state
            .agents
            .values()
            .filter(|record| path_key(&record.config.team.team_dir) == key)
            .map(|record| record.name.clone())
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::team::{TeamMessage, TeamMessageKind, TeamStore};

    use super::super::VolatileRuntimeStore;

    fn message(from: &str, content: &str) -> TeamMessage {
        TeamMessage {
            kind: TeamMessageKind::Message,
            sender: from.to_string(),
            content: content.to_string(),
            timestamp: 0,
            request_id: None,
            protocol: None,
            approve: None,
        }
    }

    #[test]
    fn team_inbox_round_trips_through_read_ack() {
        let store = VolatileRuntimeStore::new();
        let team_dir = PathBuf::from("/tmp/does-not-exist/team");

        store
            .append_team_message(&team_dir, "primary", &message("lead", "hello"))
            .expect("append message");
        assert_eq!(
            store
                .unread_team_count(&team_dir, "primary")
                .expect("unread count"),
            1
        );

        let read = store
            .read_team_inbox(&team_dir, "primary")
            .expect("read inbox");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].content, "hello");

        // Read moves messages to in-flight; a second read sees nothing new
        // until ack/requeue resolves the in-flight batch.
        assert!(
            store
                .read_team_inbox(&team_dir, "primary")
                .expect("second read")
                .is_empty()
        );

        store
            .ack_team_inbox(&team_dir, "primary")
            .expect("ack inbox");
        assert_eq!(
            store
                .unread_team_count(&team_dir, "primary")
                .expect("unread count after ack"),
            0
        );
    }

    #[test]
    fn requeue_returns_inflight_messages_to_pending() {
        let store = VolatileRuntimeStore::new();
        let team_dir = PathBuf::from("/tmp/does-not-exist/team-2");

        store
            .append_team_message(&team_dir, "primary", &message("lead", "retry me"))
            .expect("append message");
        store
            .read_team_inbox(&team_dir, "primary")
            .expect("read inbox");
        store
            .requeue_team_inbox(&team_dir, "primary")
            .expect("requeue inbox");

        assert_eq!(
            store
                .unread_team_count(&team_dir, "primary")
                .expect("unread count after requeue"),
            1
        );
    }
}
