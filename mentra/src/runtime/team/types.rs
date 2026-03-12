use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use strum::Display;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TeamMemberStatus {
    #[default]
    Idle,
    Working,
    #[strum(to_string = "failed: {0}")]
    Failed(String),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberSummary {
    pub id: String,
    pub name: String,
    pub role: String,
    pub model: String,
    pub status: TeamMemberStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TeamProtocolStatus {
    #[default]
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamProtocolRequestSummary {
    pub request_id: String,
    pub protocol: String,
    pub from: String,
    pub to: String,
    pub content: String,
    pub status: TeamProtocolStatus,
    pub created_at: u64,
    pub resolved_at: Option<u64>,
    pub resolution_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TeamMessageKind {
    Message,
    Broadcast,
    Request,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMessage {
    #[serde(rename = "type")]
    pub kind: TeamMessageKind,
    #[serde(rename = "from")]
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve: Option<bool>,
}

impl TeamMessage {
    pub(crate) fn message(sender: String, content: String) -> Self {
        Self {
            kind: TeamMessageKind::Message,
            sender,
            content,
            timestamp: unix_timestamp_secs(),
            request_id: None,
            protocol: None,
            approve: None,
        }
    }

    pub(crate) fn broadcast(sender: String, content: String) -> Self {
        Self {
            kind: TeamMessageKind::Broadcast,
            sender,
            content,
            timestamp: unix_timestamp_secs(),
            request_id: None,
            protocol: None,
            approve: None,
        }
    }

    pub(crate) fn request(sender: String, request: &TeamProtocolRequestSummary) -> Self {
        Self {
            kind: TeamMessageKind::Request,
            sender,
            content: request.content.clone(),
            timestamp: unix_timestamp_secs(),
            request_id: Some(request.request_id.clone()),
            protocol: Some(request.protocol.clone()),
            approve: None,
        }
    }

    pub(crate) fn response(
        sender: String,
        request: &TeamProtocolRequestSummary,
        approve: bool,
        reason: String,
    ) -> Self {
        Self {
            kind: TeamMessageKind::Response,
            sender,
            content: reason,
            timestamp: unix_timestamp_secs(),
            request_id: Some(request.request_id.clone()),
            protocol: Some(request.protocol.clone()),
            approve: Some(approve),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDispatch {
    pub teammate: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum TeamRequestDirection {
    Inbound,
    Outbound,
    #[default]
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TeamRequestFilter {
    pub status: Option<TeamProtocolStatus>,
    pub protocol: Option<String>,
    pub counterparty: Option<String>,
    pub direction: TeamRequestDirection,
}

impl TeamRequestFilter {
    pub(crate) fn matches(&self, agent_name: &str, request: &TeamProtocolRequestSummary) -> bool {
        if let Some(status) = &self.status
            && &request.status != status
        {
            return false;
        }

        if let Some(protocol) = &self.protocol
            && request.protocol != *protocol
        {
            return false;
        }

        if let Some(counterparty) = &self.counterparty
            && request.from != *counterparty
            && request.to != *counterparty
        {
            return false;
        }

        match self.direction {
            TeamRequestDirection::Inbound => request.to == agent_name,
            TeamRequestDirection::Outbound => request.from == agent_name,
            TeamRequestDirection::Any => request.from == agent_name || request.to == agent_name,
        }
    }
}

pub(crate) fn format_inbox(messages: &[TeamMessage]) -> String {
    let body = serde_json::to_string_pretty(messages).unwrap_or_else(|_| "[]".to_string());
    format!("<team-inbox>\n{body}\n</team-inbox>")
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
