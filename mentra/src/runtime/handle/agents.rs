use super::*;

impl RuntimeHandle {
    pub fn register_agent(
        &self,
        agent_id: &str,
        agent_name: &str,
        config: AgentExecutionConfig,
        observer: &AgentObserver,
    ) -> Result<(), RuntimeError> {
        self.acquire_agent_lease(agent_id)?;
        self.background_tasks.register_agent(agent_id, observer);
        self.team.register_agent(agent_name, &config, observer)?;
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
