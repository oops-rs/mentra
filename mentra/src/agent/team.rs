use std::{borrow::Cow, sync::Arc};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::RuntimeError;
use crate::runtime::task::TaskIntrinsicTool;
use crate::team::{
    TEAMMATE_MAX_ROUNDS, TeamDispatch, TeamIntrinsicTool, TeamMemberStatus, TeamMemberSummary,
    TeamMessage, TeamProtocolRequestSummary, build_teammate_system_prompt,
};

use super::{Agent, AgentSpawnOptions, TeammateIdentity};

impl Agent {
    pub async fn spawn_teammate(
        &mut self,
        name: impl Into<String>,
        role: impl Into<String>,
        prompt: Option<String>,
    ) -> Result<TeamMemberSummary, RuntimeError> {
        let name = name.into();
        let role = role.into();
        if name.trim().is_empty() {
            return Err(RuntimeError::InvalidTeam(
                "Teammate name must not be empty".to_string(),
            ));
        }
        if role.trim().is_empty() {
            return Err(RuntimeError::InvalidTeam(
                "Teammate role must not be empty".to_string(),
            ));
        }
        if name == self.name {
            return Err(RuntimeError::InvalidTeam(
                "Teammate name must differ from the current agent".to_string(),
            ));
        }

        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.extend(teammate_hidden_tools());

        let mut config = self.config.clone();
        config.system = Some(build_teammate_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
            &name,
            &role,
            &self.name,
        ));

        let teammate = Self::new(
            self.runtime.clone(),
            self.model.clone(),
            name.clone(),
            config,
            Arc::clone(&self.provider),
            AgentSpawnOptions {
                hidden_tools,
                max_rounds: Some(TEAMMATE_MAX_ROUNDS),
                teammate_identity: Some(TeammateIdentity {
                    role: role.clone(),
                    lead: self.name.clone(),
                }),
            },
        )?;

        let summary = TeamMemberSummary {
            id: teammate.id().to_string(),
            name: name.clone(),
            role,
            model: teammate.model().to_string(),
            status: TeamMemberStatus::Idle,
        };

        let team_dir = self.config.team.team_dir.clone();
        let actor = Arc::new(AsyncMutex::new(teammate));
        let actor_handle = self.runtime.spawn_teammate_actor(&team_dir, &name, actor)?;

        let summary = self
            .runtime
            .register_teammate(&team_dir, summary, actor_handle)?;

        if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
            self.send_team_message(&name, prompt)?;
        }

        Ok(summary)
    }

    pub(crate) fn revive_teammate_actor(self) -> Result<(), RuntimeError> {
        let Some(identity) = self.teammate_identity.clone() else {
            return Err(RuntimeError::InvalidTeam(
                "Only teammate agents can be revived as teammate actors".to_string(),
            ));
        };

        let summary = TeamMemberSummary {
            id: self.id().to_string(),
            name: self.name.clone(),
            role: identity.role,
            model: self.model.clone(),
            status: TeamMemberStatus::Idle,
        };

        let runtime = self.runtime.clone();
        let team_dir = self.config.team.team_dir.clone();
        let actor = Arc::new(AsyncMutex::new(self));
        let actor_handle = runtime.spawn_teammate_actor(&team_dir, &summary.name, actor)?;
        runtime.register_teammate(&team_dir, summary.clone(), actor_handle)?;
        runtime.wake_teammate(&team_dir, &summary.name)?;
        Ok(())
    }

    pub fn send_team_message(
        &self,
        to: &str,
        content: impl Into<String>,
    ) -> Result<TeamDispatch, RuntimeError> {
        self.runtime.send_team_message(
            self.config.team.team_dir.as_path(),
            &self.name,
            to,
            content.into(),
        )
    }

    pub fn broadcast_team_message(
        &self,
        content: impl Into<String>,
    ) -> Result<Vec<TeamDispatch>, RuntimeError> {
        self.runtime.broadcast_team_message(
            self.config.team.team_dir.as_path(),
            &self.name,
            content.into(),
        )
    }

    pub fn read_team_inbox(&self) -> Result<Vec<TeamMessage>, RuntimeError> {
        self.runtime
            .read_team_inbox(self.config.team.team_dir.as_path(), &self.name)
    }

    pub fn request_team_protocol(
        &self,
        to: &str,
        protocol: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.runtime.create_team_request(
            self.config.team.team_dir.as_path(),
            &self.name,
            to,
            protocol.into(),
            content.into(),
        )
    }

    pub fn respond_team_protocol(
        &self,
        request_id: &str,
        approve: bool,
        reason: Option<String>,
    ) -> Result<TeamProtocolRequestSummary, RuntimeError> {
        self.runtime.resolve_team_request(
            self.config.team.team_dir.as_path(),
            &self.name,
            request_id,
            approve,
            reason,
        )
    }
}

#[derive(Debug, Deserialize)]
struct TaskInput {
    prompt: String,
}

fn teammate_hidden_tools() -> [String; 3] {
    [
        TeamIntrinsicTool::Spawn.to_string(),
        TeamIntrinsicTool::Broadcast.to_string(),
        TaskIntrinsicTool::Create.to_string(),
    ]
}

pub(crate) fn parse_task_input(input: Value) -> Result<String, String> {
    let parsed = serde_json::from_value::<TaskInput>(input)
        .map_err(|error| format!("Invalid task input: {error}"))?;

    if parsed.prompt.trim().is_empty() {
        return Err("Task prompt must not be empty".to_string());
    }

    Ok(parsed.prompt)
}
