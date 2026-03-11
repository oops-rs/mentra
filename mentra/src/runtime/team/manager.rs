use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{broadcast, mpsc, watch};

use crate::runtime::{AgentEvent, AgentSnapshot, error::RuntimeError};

use super::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary,
    TeamProtocolStatus, TeamRequestFilter,
    store::{
        append_message, ensure_team_dirs, has_pending_messages as store_has_pending_messages,
        inbox_path, load_team_state, persist_team_state, read_and_drain_messages,
        requeue_messages as store_requeue_messages, unread_message_count,
    },
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub(crate) struct TeamManager {
    inner: Arc<TeamManagerInner>,
}

#[derive(Default)]
struct TeamManagerInner {
    state: Mutex<TeamManagerState>,
}

#[derive(Default)]
struct TeamManagerState {
    teams: HashMap<String, TeamState>,
}

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
    events: broadcast::Sender<AgentEvent>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
}

struct TeammateActorHandle {
    wake_tx: mpsc::UnboundedSender<()>,
    _task: std::thread::JoinHandle<()>,
}

impl TeamManager {
    pub(crate) fn register_agent(
        &self,
        agent_name: &str,
        team_dir: &Path,
        events: broadcast::Sender<AgentEvent>,
        snapshot_tx: watch::Sender<AgentSnapshot>,
        snapshot: Arc<Mutex<AgentSnapshot>>,
    ) -> Result<(), RuntimeError> {
        let (members, requests, unread_count) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            team.known_agents.insert(agent_name.to_string());
            team.unread_counts.insert(
                agent_name.to_string(),
                unread_message_count(team_dir, agent_name)?,
            );
            team.observers
                .retain(|observer| observer.agent_name != agent_name);
            team.observers.push(TeamObserver {
                agent_name: agent_name.to_string(),
                events,
                snapshot_tx: snapshot_tx.clone(),
                snapshot: Arc::clone(&snapshot),
            });
            (
                team.members.clone(),
                team.requests.clone(),
                team.unread_counts
                    .get(agent_name)
                    .copied()
                    .unwrap_or_default(),
            )
        };

        Self::publish_snapshot(Arc::clone(&snapshot), &members, &requests, unread_count);
        let snapshot = snapshot.lock().expect("agent snapshot poisoned").clone();
        snapshot_tx.send_replace(snapshot);
        Ok(())
    }

    pub(crate) fn spawn_teammate(
        &self,
        team_dir: &Path,
        summary: TeamMemberSummary,
        wake_tx: mpsc::UnboundedSender<()>,
        task: std::thread::JoinHandle<()>,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        let (observers, members, requests) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
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
            team.actors.insert(
                summary.name.clone(),
                TeammateActorHandle {
                    wake_tx,
                    _task: task,
                },
            );
            persist_team_state(&team.team_dir, &team.members, &team.requests)?;
            (
                team.observers.clone(),
                team.members.clone(),
                team.requests.clone(),
            )
        };

        self.publish_to_observers(
            observers,
            members,
            requests,
            AgentEvent::TeammateSpawned {
                teammate: summary.clone(),
            },
        );
        Ok(summary)
    }

    pub(crate) fn update_member_status(
        &self,
        team_dir: &Path,
        name: &str,
        status: TeamMemberStatus,
    ) -> Result<(), RuntimeError> {
        let (observers, members, requests, teammate) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            let teammate = team
                .members
                .iter_mut()
                .find(|member| member.name == name)
                .ok_or_else(|| {
                    RuntimeError::InvalidTeam(format!("Unknown team member '{name}'"))
                })?;
            teammate.status = status;
            let teammate = teammate.clone();
            persist_team_state(&team.team_dir, &team.members, &team.requests)?;
            (
                team.observers.clone(),
                team.members.clone(),
                team.requests.clone(),
                teammate,
            )
        };

        self.publish_to_observers(
            observers,
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
            let team = ensure_team_state(&mut state, team_dir)?;
            if !team.known_agents.contains(to)
                && !team.members.iter().any(|member| member.name == to)
            {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Unknown team recipient '{to}'"
                )));
            }

            append_message(
                inbox_path(team_dir, to).as_path(),
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
            let team = ensure_team_state(&mut state, team_dir)?;

            let mut recipients = team.known_agents.iter().cloned().collect::<Vec<_>>();
            recipients.sort();
            recipients.retain(|name| name != sender);

            let mut wake_txs = Vec::new();
            for recipient in &recipients {
                append_message(
                    inbox_path(team_dir, recipient).as_path(),
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
            let team = ensure_team_state(&mut state, team_dir)?;
            let messages = read_and_drain_messages(inbox_path(team_dir, agent_name).as_path())?;
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
        let _guard = self.inner.state.lock().expect("team manager poisoned");
        store_has_pending_messages(team_dir, agent_name)
    }

    pub(crate) fn requeue_messages(
        &self,
        team_dir: &Path,
        agent_name: &str,
        messages: Vec<TeamMessage>,
    ) -> Result<(), RuntimeError> {
        let notification = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            store_requeue_messages(team_dir, agent_name, messages)?;
            team.unread_counts.insert(
                agent_name.to_string(),
                unread_message_count(team_dir, agent_name)?,
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
        let (observers, members, requests, request, wake_tx, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
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

            append_message(
                inbox_path(team_dir, to).as_path(),
                &TeamMessage::request(sender.to_string(), &request),
            )?;
            increment_unread_count(team, to);

            team.requests.push(request.clone());
            persist_team_state(&team.team_dir, &team.members, &team.requests)?;
            (
                team.observers.clone(),
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
            observers,
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
        let (observers, members, requests, request, wake_tx, notification) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
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

            append_message(
                inbox_path(team_dir, &request.from).as_path(),
                &TeamMessage::response(responder.to_string(), &request, approve, response_body),
            )?;
            increment_unread_count(team, &request.from);

            if approve && request.protocol == "shutdown" {
                team.pending_shutdowns.insert(responder.to_string());
            }

            persist_team_state(&team.team_dir, &team.members, &team.requests)?;
            let wake_tx = team
                .actors
                .get(&request.from)
                .map(|actor| actor.wake_tx.clone());
            let notification = inbox_notification(team, &request.from);
            (
                team.observers.clone(),
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
            observers,
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
            let team = ensure_team_state(&mut state, team_dir)?;
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
        let team = ensure_team_state(&mut state, team_dir)?;
        Ok(team.pending_shutdowns.remove(teammate_name))
    }

    fn publish_to_observers(
        &self,
        observers: Vec<TeamObserver>,
        members: Vec<TeamMemberSummary>,
        requests: Vec<TeamProtocolRequestSummary>,
        event: AgentEvent,
    ) {
        for observer in observers {
            let unread_count = observer
                .snapshot
                .lock()
                .expect("agent snapshot poisoned")
                .pending_team_messages;
            Self::publish_snapshot(
                Arc::clone(&observer.snapshot),
                &members,
                &requests,
                unread_count,
            );
            let snapshot = observer
                .snapshot
                .lock()
                .expect("agent snapshot poisoned")
                .clone();
            observer.snapshot_tx.send_replace(snapshot);
            let _ = observer.events.send(event.clone());
        }
    }

    fn publish_snapshot(
        snapshot: Arc<Mutex<AgentSnapshot>>,
        members: &[TeamMemberSummary],
        requests: &[TeamProtocolRequestSummary],
        unread_count: usize,
    ) {
        let mut guard = snapshot.lock().expect("agent snapshot poisoned");
        guard.teammates = members.to_vec();
        guard.protocol_requests = requests.to_vec();
        guard.pending_team_messages = unread_count;
    }

    fn publish_inbox_notification(&self, notification: Option<InboxNotification>) {
        let Some(notification) = notification else {
            return;
        };

        for observer in notification.observers {
            Self::publish_snapshot(
                Arc::clone(&observer.snapshot),
                &notification.members,
                &notification.requests,
                notification.unread_count,
            );
            let snapshot = observer
                .snapshot
                .lock()
                .expect("agent snapshot poisoned")
                .clone();
            observer.snapshot_tx.send_replace(snapshot);
            let _ = observer.events.send(AgentEvent::TeamInboxUpdated {
                unread_count: notification.unread_count,
            });
        }
    }
}

impl Default for TeamState {
    fn default() -> Self {
        Self {
            team_dir: PathBuf::new(),
            members: Vec::new(),
            requests: Vec::new(),
            known_agents: HashSet::new(),
            unread_counts: HashMap::new(),
            observers: Vec::new(),
            actors: HashMap::new(),
            pending_shutdowns: HashSet::new(),
        }
    }
}

fn ensure_team_state<'a>(
    state: &'a mut TeamManagerState,
    team_dir: &Path,
) -> Result<&'a mut TeamState, RuntimeError> {
    let key = team_key(team_dir);
    if !state.teams.contains_key(&key) {
        ensure_team_dirs(team_dir)?;
        let disk_state = load_team_state(team_dir)?;
        state.teams.insert(
            key.clone(),
            TeamState {
                team_dir: team_dir.to_path_buf(),
                members: disk_state
                    .members
                    .into_iter()
                    .map(|mut member| {
                        if matches!(
                            member.status,
                            TeamMemberStatus::Idle | TeamMemberStatus::Working
                        ) {
                            member.status = TeamMemberStatus::Shutdown;
                        }
                        member
                    })
                    .collect(),
                requests: disk_state.requests,
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
