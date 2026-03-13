use super::*;
use crate::{
    agent::{AgentEvent, AgentSnapshot},
    background::{BackgroundObserverSink, BackgroundRegistration},
    team::{TeamObserverSink, TeamRegistration},
};

struct AgentTeamObserver {
    store: Arc<dyn RuntimeStore>,
    tasks_dir: PathBuf,
    events: broadcast::Sender<AgentEvent>,
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
}

impl AgentTeamObserver {
    fn new(store: Arc<dyn RuntimeStore>, tasks_dir: PathBuf, observer: &AgentObserver) -> Self {
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
    snapshot_tx: watch::Sender<AgentSnapshot>,
    snapshot: Arc<Mutex<AgentSnapshot>>,
    events: broadcast::Sender<AgentEvent>,
}

impl AgentBackgroundObserver {
    fn new(observer: &AgentObserver) -> Self {
        Self {
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
    }

    fn publish_event(&self, event: AgentEvent) {
        let _ = self.events.send(event);
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
        self.background_tasks.register_agent(BackgroundRegistration {
            agent_id: agent_id.to_string(),
            observer: Arc::new(AgentBackgroundObserver::new(observer)),
        });
        self.team.register_agent(TeamRegistration {
            agent_name: agent_name.to_string(),
            team_dir: config.team_dir.clone(),
            observer: Arc::new(AgentTeamObserver::new(
                self.store.clone(),
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
        let acquired =
            self.store
                .acquire_lease(&key, &self.runtime_instance_id, Duration::from_secs(3600))?;
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
