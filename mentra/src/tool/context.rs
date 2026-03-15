use async_trait::async_trait;

use crate::tool::{ParallelToolContext, ToolContext};

#[async_trait]
pub(crate) trait RuntimeContext {
    fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<std::path::PathBuf, String>;
    async fn execute_shell_command(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String>;
    fn start_background_task(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::BackgroundTaskSummary, String>;
}

#[async_trait]
impl RuntimeContext for ToolContext<'_> {
    fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<std::path::PathBuf, String> {
        self.resolve_working_directory(working_directory)
    }

    async fn execute_shell_command(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.execute_shell_command(command, justification, requested_timeout, cwd)
            .await
    }

    fn start_background_task(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::BackgroundTaskSummary, String> {
        self.start_background_task(command, justification, requested_timeout, cwd)
    }
}

#[async_trait]
impl RuntimeContext for ParallelToolContext {
    fn resolve_working_directory(
        &self,
        working_directory: Option<&str>,
    ) -> Result<std::path::PathBuf, String> {
        self.resolve_working_directory(working_directory)
    }

    async fn execute_shell_command(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::runtime::CommandOutput, String> {
        self.execute_shell_command(command, justification, requested_timeout, cwd)
            .await
    }

    fn start_background_task(
        &self,
        command: String,
        justification: Option<String>,
        requested_timeout: Option<std::time::Duration>,
        cwd: std::path::PathBuf,
    ) -> Result<crate::BackgroundTaskSummary, String> {
        self.start_background_task(command, justification, requested_timeout, cwd)
    }
}
