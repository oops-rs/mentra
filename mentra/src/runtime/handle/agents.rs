use super::*;
use crate::{
    agent::{AgentEvent, AgentSnapshot},
    background::{BackgroundObserverSink, BackgroundRegistration},
    team::{TeamObserverSink, TeamRegistration},
};

struct AgentTeamObserver {
    store: Arc<dyn crate::runtime::TaskStore>,
    tasks_dir: PathBuf,
    events: broadcast::Sender<AgentEvent>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
}

impl AgentTeamObserver {
    fn new(
        store: Arc<dyn crate::runtime::TaskStore>,
        tasks_dir: PathBuf,
        observer: &AgentObserver,
    ) -> Self {
        Self {
            store,
            tasks_dir,
            events: observer.events.clone(),
            snapshot_tx: observer.snapshot_tx.clone(),
            snapshot: Arc::clone(&observer.snapshot),
        }
    }
}

impl TeamObserverSink for AgentTeamObserver {
    fn publish_snapshot(
        &self,
        members: &[crate::team::TeamMemberSummary],
        requests: &[crate::team::TeamProtocolRequestSummary],
        unread_count: usize,
    ) {
        let mut snapshot = self.snapshot.lock().expect("agent snapshot poisoned");
        if let Ok(tasks) = self.store.load_tasks(self.tasks_dir.as_path()) {
            snapshot.tasks = tasks;
        }
        snapshot.teammates = members.to_vec();
        snapshot.protocol_requests = requests.to_vec();
        snapshot.pending_team_messages = unread_count;
        let next_snapshot = snapshot.clone();
        drop(snapshot);
        self.snapshot_tx.send_replace(next_snapshot);
    }

    fn publish_event(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}

struct AgentBackgroundObserver {
    background_tasks: crate::background::BackgroundTaskManager,
    team: crate::team::TeamManager,
    agent_id: String,
    team_dir: PathBuf,
    agent_name: String,
    is_teammate: bool,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
    events: broadcast::Sender<AgentEvent>,
}

impl AgentBackgroundObserver {
    fn new(
        background_tasks: crate::background::BackgroundTaskManager,
        team: crate::team::TeamManager,
        agent_id: String,
        config: &AgentExecutionConfig,
        observer: &AgentObserver,
    ) -> Self {
        Self {
            background_tasks,
            team,
            agent_id,
            team_dir: config.team_dir.clone(),
            agent_name: config.name.clone(),
            is_teammate: config.is_teammate,
            snapshot_tx: observer.snapshot_tx.clone(),
            snapshot: Arc::clone(&observer.snapshot),
            events: observer.events.clone(),
        }
    }
}

impl BackgroundObserverSink for AgentBackgroundObserver {
    fn publish_snapshot(&self, tasks: &[crate::background::BackgroundTaskSummary]) {
        let mut snapshot = self.snapshot.lock().expect("agent snapshot poisoned");
        snapshot.background_tasks = tasks.to_vec();
        let next_snapshot = snapshot.clone();
        drop(snapshot);
        self.snapshot_tx.send_replace(next_snapshot);
        if self.is_teammate
            && self
                .background_tasks
                .has_pending_notifications(&self.agent_id)
        {
            let _ = self
                .team
                .wake_teammate(self.team_dir.as_path(), &self.agent_name);
        }
    }

    fn publish_event(&self, event: AgentEvent) {
        let should_wake_teammate =
            self.is_teammate && matches!(event, AgentEvent::BackgroundTaskFinished { .. });
        let _ = self.events.send(event);
        if should_wake_teammate {
            let _ = self
                .team
                .wake_teammate(self.team_dir.as_path(), &self.agent_name);
        }
    }
}

impl RuntimeHandle {
    pub fn register_agent(
        &self,
        agent_id: &str,
        agent_name: &str,
        config: AgentExecutionConfig,
        observer: &AgentObserver,
    ) -> Result<(), RuntimeError> {
        self.acquire_agent_lease(agent_id)?;
        self.collaboration
            .background_tasks
            .register_agent(BackgroundRegistration {
                agent_id: agent_id.to_string(),
                observer: Arc::new(AgentBackgroundObserver::new(
                    self.collaboration.background_tasks.clone(),
                    self.collaboration.team.clone(),
                    agent_id.to_string(),
                    &config,
                    observer,
                )),
            });
        self.collaboration.team.register_agent(TeamRegistration {
            agent_name: agent_name.to_string(),
            team_dir: config.team_dir.clone(),
            observer: Arc::new(AgentTeamObserver::new(
                self.persistence.store.clone(),
                config.tasks_dir.clone(),
                observer,
            )),
        })?;
        self.agent_contexts
            .write()
            .expect("agent context registry poisoned")
            .insert(agent_id.to_string(), config);
        Ok(())
    }

    pub fn acquire_agent_lease(&self, agent_id: &str) -> Result<(), RuntimeError> {
        let key = format!("agent:{agent_id}");
        let acquired = self.persistence.store.acquire_lease(
            &key,
            &self.runtime_instance_id,
            Duration::from_secs(3600),
        )?;
        if acquired {
            self.lease_keys
                .lock()
                .expect("lease key registry poisoned")
                .insert(key);
            Ok(())
        } else {
            Err(RuntimeError::LeaseUnavailable(format!(
                "Agent '{agent_id}' is already leased by another runtime"
            )))
        }
    }

    pub(crate) fn agent_config(&self, agent_id: &str) -> Result<AgentExecutionConfig, String> {
        self.agent_contexts
            .read()
            .expect("agent context registry poisoned")
            .get(agent_id)
            .cloned()
            .ok_or_else(|| format!("Unknown agent '{agent_id}'"))
    }
}
