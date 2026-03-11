use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub id: u64,
    pub subject: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub blocked_by: Vec<u64>,
    #[serde(default)]
    pub blocks: Vec<u64>,
    #[serde(default)]
    pub owner: String,
}
