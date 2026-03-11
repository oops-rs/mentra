use std::{borrow::Cow, sync::Arc};

use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::runtime::{
    TASK_CREATE_TOOL_NAME, TASK_UPDATE_TOOL_NAME, TeamDispatch, TeamMemberStatus,
    TeamMemberSummary, TeamProtocolRequestSummary, TeamProtocolStatus,
    error::RuntimeError,
    team::{
        TEAM_BROADCAST_TOOL_NAME, TEAM_SPAWN_TOOL_NAME, TEAMMATE_MAX_ROUNDS, TeamMessage,
        TeamRequestDirection, TeamRequestFilter, build_teammate_system_prompt, teammate_actor_loop,
    },
};

use super::Agent;

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
            hidden_tools,
            Some(TEAMMATE_MAX_ROUNDS),
        )?;

        let summary = TeamMemberSummary {
            id: teammate.id().to_string(),
            name: name.clone(),
            role,
            model: teammate.model().to_string(),
            status: TeamMemberStatus::Idle,
        };

        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let manager = self.runtime.team_manager();
        let team_dir = self.config.team.team_dir.clone();
        let actor = Arc::new(AsyncMutex::new(teammate));
        let actor_team_dir = team_dir.clone();
        let actor_name = name.clone();
        let task = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("teammate runtime");
            runtime.block_on(teammate_actor_loop(
                manager,
                actor_team_dir,
                actor_name,
                actor,
                wake_rx,
            ));
        });

        let summary = self
            .runtime
            .register_teammate(&team_dir, summary, wake_tx, task)?;

        if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
            self.send_team_message(&name, prompt)?;
        }

        Ok(summary)
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

    pub(crate) fn list_team_protocol_requests(
        &self,
        status: Option<TeamProtocolStatus>,
        protocol: Option<String>,
        counterparty: Option<String>,
        direction: TeamRequestDirection,
    ) -> Result<Vec<TeamProtocolRequestSummary>, RuntimeError> {
        self.runtime.list_team_requests(
            self.config.team.team_dir.as_path(),
            &self.name,
            TeamRequestFilter {
                status,
                protocol,
                counterparty,
                direction,
            },
        )
    }

    pub(crate) fn spawn_disposable_subagent(&self) -> Result<Self, RuntimeError> {
        let mut hidden_tools = self.hidden_tools.clone();
        hidden_tools.insert(crate::runtime::TASK_TOOL_NAME.to_string());

        let mut config = self.config.clone();
        config.system = Some(crate::runtime::task::build_subagent_system_prompt(
            self.config.system.as_deref().map(Cow::Borrowed),
        ));

        Self::new(
            self.runtime.clone(),
            self.model.clone(),
            format!("{}::task", self.name),
            config,
            Arc::clone(&self.provider),
            hidden_tools,
            Some(crate::runtime::task::SUBAGENT_MAX_ROUNDS),
        )
    }
}

fn teammate_hidden_tools() -> [String; 4] {
    [
        TEAM_SPAWN_TOOL_NAME.to_string(),
        TEAM_BROADCAST_TOOL_NAME.to_string(),
        TASK_CREATE_TOOL_NAME.to_string(),
        TASK_UPDATE_TOOL_NAME.to_string(),
    ]
}
