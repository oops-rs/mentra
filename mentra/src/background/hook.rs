use std::path::Path;

use crate::error::RuntimeError;

pub(crate) trait BackgroundHookSink: Send + Sync {
    fn task_started(
        &self,
        agent_id: &str,
        task_id: &str,
        command: &str,
        cwd: &Path,
    ) -> Result<(), RuntimeError>;

    fn task_finished(
        &self,
        agent_id: &str,
        task_id: &str,
        status: &str,
    ) -> Result<(), RuntimeError>;
}
