use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::{broadcast, mpsc, watch};

use crate::runtime::{AgentEvent, AgentSnapshot, error::RuntimeError};

use super::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage,
    store::{
        append_message, ensure_team_dirs, has_pending_messages as store_has_pending_messages,
        inbox_path, load_team_state, persist_team_state, read_and_drain_messages,
        requeue_messages as store_requeue_messages,
    },
};

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
    known_agents: HashSet<String>,
    observers: Vec<TeamObserver>,
    actors: HashMap<String, TeammateActorHandle>,
}

#[derive(Clone)]
struct TeamObserver {
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
        let members = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            team.known_agents.insert(agent_name.to_string());
            team.observers.push(TeamObserver {
                events,
                snapshot_tx: snapshot_tx.clone(),
                snapshot: Arc::clone(&snapshot),
            });
            team.members.clone()
        };

        Self::publish_snapshot(Arc::clone(&snapshot), &members);
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
        let (observers, members) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            if team.members.iter().any(|member| member.name == summary.name) {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Team member '{}' already exists",
                    summary.name
                )));
            }
            team.known_agents.insert(summary.name.clone());
            team.members.push(summary.clone());
            team.actors.insert(
                summary.name.clone(),
                TeammateActorHandle {
                    wake_tx,
                    _task: task,
                },
            );
            persist_team_state(&team.team_dir, &team.members)?;
            (team.observers.clone(), team.members.clone())
        };

        self.publish_to_observers(
            observers,
            members,
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
        let (observers, members, teammate) = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            let teammate = team
                .members
                .iter_mut()
                .find(|member| member.name == name)
                .ok_or_else(|| RuntimeError::InvalidTeam(format!("Unknown team member '{name}'")))?;
            teammate.status = status;
            let teammate = teammate.clone();
            persist_team_state(&team.team_dir, &team.members)?;
            (team.observers.clone(), team.members.clone(), teammate)
        };

        self.publish_to_observers(observers, members, AgentEvent::TeammateUpdated { teammate });
        Ok(())
    }

    pub(crate) fn send_message(
        &self,
        team_dir: &Path,
        sender: &str,
        to: &str,
        content: String,
    ) -> Result<TeamDispatch, RuntimeError> {
        let wake_tx = {
            let mut state = self.inner.state.lock().expect("team manager poisoned");
            let team = ensure_team_state(&mut state, team_dir)?;
            if !team.known_agents.contains(to) && !team.members.iter().any(|member| member.name == to) {
                return Err(RuntimeError::InvalidTeam(format!(
                    "Unknown team recipient '{to}'"
                )));
            }

            append_message(
                inbox_path(team_dir, to).as_path(),
                &TeamMessage::message(sender.to_string(), content),
            )?;

            team.actors.get(to).map(|actor| actor.wake_tx.clone())
        };

        if let Some(wake_tx) = wake_tx {
            let _ = wake_tx.send(());
        }

        Ok(TeamDispatch {
            teammate: to.to_string(),
        })
    }

    pub(crate) fn read_inbox(
        &self,
        team_dir: &Path,
        agent_name: &str,
    ) -> Result<Vec<TeamMessage>, RuntimeError> {
        let _guard = self.inner.state.lock().expect("team manager poisoned");
        read_and_drain_messages(inbox_path(team_dir, agent_name).as_path())
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
        let _guard = self.inner.state.lock().expect("team manager poisoned");
        store_requeue_messages(team_dir, agent_name, messages)
    }

    fn publish_to_observers(
        &self,
        observers: Vec<TeamObserver>,
        members: Vec<TeamMemberSummary>,
        event: AgentEvent,
    ) {
        for observer in observers {
            Self::publish_snapshot(Arc::clone(&observer.snapshot), &members);
            let snapshot = observer
                .snapshot
                .lock()
                .expect("agent snapshot poisoned")
                .clone();
            observer.snapshot_tx.send_replace(snapshot);
            let _ = observer.events.send(event.clone());
        }
    }

    fn publish_snapshot(snapshot: Arc<Mutex<AgentSnapshot>>, members: &[TeamMemberSummary]) {
        let mut guard = snapshot.lock().expect("agent snapshot poisoned");
        guard.teammates = members.to_vec();
    }
}

impl Default for TeamState {
    fn default() -> Self {
        Self {
            team_dir: PathBuf::new(),
            members: Vec::new(),
            known_agents: HashSet::new(),
            observers: Vec::new(),
            actors: HashMap::new(),
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
        state.teams.insert(
            key.clone(),
            TeamState {
                team_dir: team_dir.to_path_buf(),
                members: load_team_state(team_dir)?.members,
                ..Default::default()
            },
        );
    }

    Ok(state.teams.get_mut(&key).expect("team state missing"))
}

fn team_key(team_dir: &Path) -> String {
    team_dir.to_string_lossy().into_owned()
}
