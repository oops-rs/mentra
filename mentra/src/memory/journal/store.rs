use crate::{error::RuntimeError, runtime::RuntimeStore};

use super::state::AgentMemoryState;

pub(crate) trait AgentMemoryStore: Send + Sync {
    fn save_memory(&self, agent_id: &str, state: &AgentMemoryState) -> Result<(), RuntimeError>;
}

impl<T> AgentMemoryStore for T
where
    T: RuntimeStore + ?Sized,
{
    fn save_memory(&self, agent_id: &str, state: &AgentMemoryState) -> Result<(), RuntimeError> {
        self.save_agent_memory(agent_id, state)
    }
}
