use crate::{agent::AgentEvent, error::RuntimeError};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamObserverSink,
    TeamProtocolRequestSummary, TeamProtocolStatus, TeamRegistration, TeamRequestFilter, TeamStore,
    TeammateActorHandle,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct TeamManager {
    inner: Arc<TeamManagerInner>,
}

struct TeamManagerInner {
    store: Arc<dyn TeamStore>,
    state: Mutex<TeamManagerState>,
}

#[derive(Default)]
struct TeamManagerState {
    teams: HashMap<String, TeamState>,
}

#[derive(Default)]
struct TeamState {
    team_dir: PathBuf,
    members: Vec<TeamMemberSummary>,
    requests: Vec<TeamProtocolRequestSummary>,
    known_agents: HashSet<String>,
    unread_counts: HashMap<String, usize>,
    observers: Vec<TeamObserver>,
    actors: HashMap<String, TeammateActorHandle>,
    pending_shutdowns: HashSet<String>,
}

#[derive(Clone)]
struct TeamObserver {
    agent_name: String,
    sink: Arc<dyn TeamObserverSink>,
}

#[derive(Clone)]
struct ObserverUpdate {
    observer: TeamObserver,
    unread_count: usize,
}

impl TeamManager {
    pub(crate) fn new(store: Arc<dyn TeamStore>) -> Self {
        Self {
            inner: Arc::new(TeamManagerInner {
                store,
                state: Default::default(),
            }),
        }
    }

    pub(crate) fn register_agent(
        &self,
        registration: TeamRegistration,
    ) -> Result<(), RuntimeError> {
        let TeamRegistration {
            agent_name,
            team_dir,
            observer,
        } = registration;
        let (members, requests, unread_count) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir.as_path())?;
            team.known_agents.insert(agent_name.clone());
            team.unread_counts.insert(
                agent_name.clone(),
                self.inner
                    .store
                    .unread_team_count(team_dir.as_path(), &agent_name)?,
            );
            team.observers
                .retain(|existing| existing.agent_name != agent_name);
            team.observers.push(TeamObserver {
                agent_name: agent_name.clone(),
                sink: observer.clone(),
            });
            (
                team.members.clone(),
                team.requests.clone(),
                team.unread_counts
                    .get(&agent_name)
                    .copied()
                    .unwrap_or_default(),
            )
        };

        observer.publish_snapshot(&members, &requests, unread_count);
        Ok(())
    }

    pub(crate) fn spawn_teammate(
        &self,
        team_dir: &Path,
        summary: TeamMemberSummary,
        actor: TeammateActorHandle,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        let (observer_updates, members, requests) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            if let Some(index) = team
                .members
                .iter()
                .position(|member| member.name == summary.name)
            {
                if team.actors.contains_key(&summary.name) {
                    return Err(RuntimeError::InvalidTeam(format!(
                        "Team member '{}' already exists",
                        summary.name
                    )));
                }

                team.members[index] = summary.clone();
            } else {
                team.members.push(summary.clone());
            }
            team.known_agents.insert(summary.name.clone());
            team.actors.insert(summary.name.clone(), actor);
            self.inner
                .store
                .upsert_team_member(&team.team_dir, &summary)?;
            (
                observer_updates(team),
                team.members.clone(),
                team.requests.clone(),
            )
        };

        self.publish_to_observers(
            observer_updates,
            members,
            requests,
            AgentEvent::TeammateSpawned {
                teammate: summary.clone(),
            },
        );
        Ok(summary)
    }

    pub(crate) fn wake_teammate(
        &self,
        team_dir: &Path,
        teammate_name: &str,
    ) -> Result<(), RuntimeError> {
        let wake_tx = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            team.actors
                .get(teammate_name)
                .map(|actor| actor.wake_tx.clone())
                .ok_or_else(|| {
                    RuntimeError::InvalidTeam(format!(
                        "No live teammate actor exists for '{teammate_name}'"
                    ))
                })?
        };

        let _ = wake_tx.send(());
        Ok(())
    }

    pub(crate) fn update_member_status(
        &self,
        team_dir: &Path,
        name: &str,
        status: TeamMemberStatus,
    ) -> Result<(), RuntimeError> {
        let (observer_updates, members, requests, teammate) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            let teammate = team
                .members
                .iter_mut()
                .find(|member| member.name == name)
                .ok_or_else(|| {
                    RuntimeError::InvalidTeam(format!("Unknown team member '{name}'"))
                })?;
            teammate.status = status;
            self.inner
                .store
                .upsert_team_member(&team.team_dir, teammate)?;
            let teammate = teammate.clone();
            (
                observer_updates(team),
                team.members.clone(),
                team.requests.clone(),
                teammate,
            )
        };

        self.publish_to_observers(
            observer_updates,
            members,
            requests,
            AgentEvent::TeammateUpdated { teammate },
        );
        Ok(())
    }

    pub(crate) fn send_message(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        content: String,
    ) -> Result<TeamDispatch, RuntimeError> {
        let (wake_tx, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            if !team.known_agents.contains(to)
                && !team.members.iter().any(|member| member.name == to)
            {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Unknown team recipient '{to}'"
                )));
            }

            self.inner.store.append_team_message(
                team_dir,
                to,
                &TeamMessage::message(sender.to_string(), content),
            )?;
            increment_unread_count(team, to);

            (
                team.actors.get(to).map(|actor| actor.wake_tx.clone()),
                inbox_notification(team, to),
            )
        };

        if let Some(wake_tx) = wake_tx {
            let _ = wake_tx.send(());
        }
        self.publish_inbox_notification(notification);

        Ok(TeamDispatch {
            teammate: to.to_string(),
        })
    }

    pub(crate) fn broadcast_message(
        &self,
        team_dir: &Path,
        sender: &str,
        content: String,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        let (recipients, wake_txs, notifications) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;

            let mut recipients = team.known_agents.iter().cloned().collect::<Vec<_>>();
            recipients.sort();
            recipients.retain(|name| name != sender);

            let mut wake_txs = Vec::new();
            for recipient in &recipients {
                self.inner.store.append_team_message(
                    team_dir,
                    recipient,
                    &TeamMessage::broadcast(sender.to_string(), content.clone()),
                )?;
                increment_unread_count(team, recipient);

                if let Some(wake_tx) = team
                    .actors
                    .get(recipient)
                    .map(|actor| actor.wake_tx.clone())
                {
                    wake_txs.push(wake_tx);
                }
            }

            let notifications = recipients
                .iter()
                .map(|recipient| inbox_notification(team, recipient))
                .collect::<Vec<_>>();

            (recipients, wake_txs, notifications)
        };

        for wake_tx in wake_txs {
            let _ = wake_tx.send(());
        }
        for notification in notifications {
            self.publish_inbox_notification(notification);
        }

        Ok(recipients
            .into_iter()
            .map(|teammate| TeamDispatch { teammate })
            .collect())
    }

    pub(crate) fn read_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        let (messages, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            let messages = self.inner.store.read_team_inbox(team_dir, agent_name)?;
            team.unread_counts.insert(agent_name.to_string(), 0);
            (messages, inbox_notification(team, agent_name))
        };
        self.publish_inbox_notification(notification);
        Ok(messages)
    }

    pub(crate) fn has_pending_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<bool, RuntimeError> {
        Ok(self.inner.store.unread_team_count(team_dir, agent_name)? > 0)
    }

    pub(crate) fn requeue_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
        _messages: Vec<TeamMessage>,
    ) -> Result<(), RuntimeError> {
        let notification = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            self.inner.store.requeue_team_inbox(team_dir, agent_name)?;
            team.unread_counts.insert(
                agent_name.to_string(),
                self.inner.store.unread_team_count(team_dir, agent_name)?,
            );
            inbox_notification(team, agent_name)
        };
        self.publish_inbox_notification(notification);
        Ok(())
    }

    pub(crate) fn acknowledge_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<(), RuntimeError> {
        let notification = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            self.inner.store.ack_team_inbox(team_dir, agent_name)?;
            team.unread_counts.insert(
                agent_name.to_string(),
                self.inner.store.unread_team_count(team_dir, agent_name)?,
            );
            inbox_notification(team, agent_name)
        };
        self.publish_inbox_notification(notification);
        Ok(())
    }

    pub(crate) fn create_request(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        protocol: String,
        content: String,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        let (observer_updates, members, requests, request, wake_tx, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            if !team.known_agents.contains(to)
                && !team.members.iter().any(|member| member.name == to)
            {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Unknown team recipient '{to}'"
                )));
            }

            let request = TeamProtocolRequestSummary {
                request_id: next_request_id(),
                protocol,
                from: sender.to_string(),
                to: to.to_string(),
                content,
                status: TeamProtocolStatus::Pending,
                created_at: unix_timestamp_secs(),
                resolved_at: None,
                resolution_reason: None,
            };

            self.inner.store.append_team_message(
                team_dir,
                to,
                &TeamMessage::request(sender.to_string(), &request),
            )?;
            increment_unread_count(team, to);

            team.requests.push(request.clone());
            self.inner
                .store
                .upsert_team_request(&team.team_dir, &request)?;
            (
                observer_updates(team),
                team.members.clone(),
                team.requests.clone(),
                request,
                team.actors.get(to).map(|actor| actor.wake_tx.clone()),
                inbox_notification(team, to),
            )
        };

        if let Some(wake_tx) = wake_tx {
            let _ = wake_tx.send(());
        }
        self.publish_inbox_notification(notification);

        self.publish_to_observers(
            observer_updates,
            members,
            requests,
            AgentEvent::TeamProtocolRequested {
                request: request.clone(),
            },
        );
        Ok(request)
    }

    pub(crate) fn resolve_request(
        &self,
        team_dir: &Path,
        responder: &str,
        request_id: &str,
        approve: bool,
        reason: Option<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        let (observer_updates, members, requests, request, wake_tx, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            let request = team
                .requests
                .iter_mut()
                .find(|request| request.request_id == request_id)
                .ok_or_else(|| {
                    RuntimeError::InvalidTeam(format!("Unknown team request '{request_id}'"))
                })?;

            if request.to != responder {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Agent '{responder}' cannot respond to request '{request_id}'"
                )));
            }

            if request.status != TeamProtocolStatus::Pending {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Team request '{request_id}' is already resolved"
                )));
            }

            request.status = if approve {
                TeamProtocolStatus::Approved
            } else {
                TeamProtocolStatus::Rejected
            };
            request.resolved_at = Some(unix_timestamp_secs());
            request.resolution_reason = reason.clone().filter(|value| !value.trim().is_empty());
            let request = request.clone();
            let response_body = request.resolution_reason.clone().unwrap_or_default();

            self.inner.store.append_team_message(
                team_dir,
                &request.from,
                &TeamMessage::response(responder.to_string(), &request, approve, response_body),
            )?;
            increment_unread_count(team, &request.from);

            if approve && request.protocol == "shutdown" {
                team.pending_shutdowns.insert(responder.to_string());
            }

            self.inner
                .store
                .upsert_team_request(&team.team_dir, &request)?;
            let wake_tx = team
                .actors
                .get(&request.from)
                .map(|actor| actor.wake_tx.clone());
            let notification = inbox_notification(team, &request.from);
            (
                observer_updates(team),
                team.members.clone(),
                team.requests.clone(),
                request,
                wake_tx,
                notification,
            )
        };

        if let Some(wake_tx) = wake_tx {
            let _ = wake_tx.send(());
        }
        self.publish_inbox_notification(notification);

        self.publish_to_observers(
            observer_updates,
            members,
            requests,
            AgentEvent::TeamProtocolResolved {
                request: request.clone(),
            },
        );
        Ok(request)
    }

    pub(crate) fn list_requests(
        &self,
        team_dir: &Path,
        agent_name: &str,
        filter: TeamRequestFilter,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        let mut requests = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
            team.requests
                .iter()
                .filter(|request| filter.matches(agent_name, request))
                .cloned()
                .collect::<Vec<_>>()
        };
        requests.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        Ok(requests)
    }

    pub(crate) fn take_shutdown_signal(
        &self,
        team_dir: &Path,
        teammate_name: &str,
    ) -> Result<bool, RuntimeError> {
        let mut state = self.inner.state.lock().expect("team manager poisoned");
        let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
        Ok(team.pending_shutdowns.remove(teammate_name))
    }

    pub(crate) fn unregister_teammate_actor(
        &self,
        team_dir: &Path,
        teammate_name: &str,
    ) -> Result<(), RuntimeError> {
        let mut state = self.inner.state.lock().expect("team manager poisoned");
        let team = ensure_team_state(&self.inner.store, &mut state, team_dir)?;
        team.actors.remove(teammate_name);
        Ok(())
    }

    fn publish_to_observers(
        &self,
        observer_updates: Vec<ObserverUpdate>,
        members: Vec<TeamMemberSummary>,
        requests: Vec<TeamProtocolRequestSummary>,
        event: AgentEvent,
    ) {
        for update in observer_updates {
            let observer = update.observer;
            observer
                .sink
                .publish_snapshot(&members, &requests, update.unread_count);
            observer.sink.publish_event(event.clone());
        }
    }

    fn publish_inbox_notification(&self, notification: Option<InboxNotification>) {
        let Some(notification) = notification else {
            return;
        };

        for observer in notification.observers {
            observer.sink.publish_snapshot(
                &notification.members,
                &notification.requests,
                notification.unread_count,
            );
            observer.sink.publish_event(AgentEvent::TeamInboxUpdated {
                unread_count: notification.unread_count,
            });
        }
    }
}

fn ensure_team_state<'a>(
    store: &Arc<dyn TeamStore>,
    state: &'a mut TeamManagerState,
    team_dir: &Path,
) -> Result<&'a mut TeamState, RuntimeError> {
    let key = team_key(team_dir);
    if !state.teams.contains_key(&key) {
        state.teams.insert(
            key.clone(),
            TeamState {
                team_dir: team_dir.to_path_buf(),
                members: store.load_team_members(team_dir)?,
                requests: store.load_team_requests(team_dir)?,
                known_agents: store.list_team_agent_names(team_dir)?.into_iter().collect(),
                ..Default::default()
            },
        );
    }

    Ok(state.teams.get_mut(&key).expect("team state missing"))
}

#[derive(Clone)]
struct InboxNotification {
    observers: Vec<TeamObserver>,
    members: Vec<TeamMemberSummary>,
    requests: Vec<TeamProtocolRequestSummary>,
    unread_count: usize,
}

fn team_key(team_dir: &Path) -> String {
    team_dir.to_string_lossy().into_owned()
}

fn increment_unread_count(team: &mut TeamState, agent_name: &str) {
    *team
        .unread_counts
        .entry(agent_name.to_string())
        .or_insert(0) += 1;
}

fn inbox_notification(team: &TeamState, agent_name: &str) -> Option<InboxNotification> {
    let unread_count = team
        .unread_counts
        .get(agent_name)
        .copied()
        .unwrap_or_default();
    let observers = team
        .observers
        .iter()
        .filter(|observer| observer.agent_name == agent_name)
        .cloned()
        .collect::<Vec<_>>();
    if observers.is_empty() {
        return None;
    }

    Some(InboxNotification {
        observers,
        members: team.members.clone(),
        requests: team.requests.clone(),
        unread_count,
    })
}

fn observer_updates(team: &TeamState) -> Vec<ObserverUpdate> {
    team.observers
        .iter()
        .cloned()
        .map(|observer| ObserverUpdate {
            unread_count: team
                .unread_counts
                .get(&observer.agent_name)
                .copied()
                .unwrap_or_default(),
            observer,
        })
        .collect()
}

fn next_request_id() -> String {
    let counter = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}", unix_timestamp_secs() ^ counter)
}

fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
