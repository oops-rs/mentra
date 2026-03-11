use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberStatus {
    #[default]
    Idle,
    Working,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMessage {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "from")]
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
}

impl TeamMessage {
    pub(crate) fn message(sender: String, content: String) -> Self {
        Self {
            kind: "message".to_string(),
            sender,
            content,
            timestamp: unix_timestamp_secs(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamDispatch {
    pub teammate: String,
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
