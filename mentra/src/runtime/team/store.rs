use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::runtime::error::RuntimeError;

use super::{TeamMemberSummary, TeamMessage};

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct TeamDiskState {
    pub(super) members: Vec<TeamMemberSummary>,
}

pub(super) fn ensure_team_dirs(team_dir: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(team_dir.join("inbox")).map_err(RuntimeError::FailedToWriteTeam)
}

pub(super) fn inbox_path(team_dir: &Path, name: &str) -> PathBuf {
    team_dir.join("inbox").join(format!("{name}.jsonl"))
}

pub(super) fn load_team_state(team_dir: &Path) -> Result<TeamDiskState, RuntimeError> {
    let path = config_path(team_dir);
    if !path.exists() {
        return Ok(TeamDiskState::default());
    }

    let content = fs::read_to_string(path).map_err(RuntimeError::FailedToLoadTeam)?;
    serde_json::from_str(&content).map_err(RuntimeError::FailedToDeserializeTeam)
}

pub(super) fn persist_team_state(
    team_dir: &Path,
    members: &[TeamMemberSummary],
) -> Result<(), RuntimeError> {
    ensure_team_dirs(team_dir)?;
    let content = serde_json::to_string_pretty(&TeamDiskState {
        members: members.to_vec(),
    })
    .map_err(RuntimeError::FailedToSerializeTeam)?;
    fs::write(config_path(team_dir), content).map_err(RuntimeError::FailedToWriteTeam)
}

pub(super) fn append_message(path: &Path, message: &TeamMessage) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(RuntimeError::FailedToWriteTeam)?;
    }
    let mut existing = if path.exists() {
        fs::read_to_string(path).map_err(RuntimeError::FailedToLoadTeam)?
    } else {
        String::new()
    };
    existing.push_str(&serialize_message(message)?);
    existing.push('\n');
    fs::write(path, existing).map_err(RuntimeError::FailedToWriteTeam)
}

pub(super) fn read_and_drain_messages(path: &Path) -> Result<Vec<TeamMessage>, RuntimeError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path).map_err(RuntimeError::FailedToLoadTeam)?;
    let messages = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<TeamMessage>(line).map_err(RuntimeError::FailedToDeserializeTeam)
        })
        .collect::<Result<Vec<_>, _>>()?;
    fs::write(path, "").map_err(RuntimeError::FailedToWriteTeam)?;
    Ok(messages)
}

pub(super) fn has_pending_messages(team_dir: &Path, agent_name: &str) -> Result<bool, RuntimeError> {
    ensure_team_dirs(team_dir)?;
    let path = inbox_path(team_dir, agent_name);
    if !path.exists() {
        return Ok(false);
    }

    Ok(!fs::read_to_string(path)
        .map_err(RuntimeError::FailedToLoadTeam)?
        .trim()
        .is_empty())
}

pub(super) fn requeue_messages(
    team_dir: &Path,
    agent_name: &str,
    messages: Vec<TeamMessage>,
) -> Result<(), RuntimeError> {
    if messages.is_empty() {
        return Ok(());
    }

    ensure_team_dirs(team_dir)?;
    let path = inbox_path(team_dir, agent_name);
    let existing = if path.exists() {
        fs::read_to_string(&path).map_err(RuntimeError::FailedToLoadTeam)?
    } else {
        String::new()
    };

    let mut requeued = messages
        .into_iter()
        .map(|message| serialize_message(&message))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    if !requeued.is_empty() {
        requeued.push('\n');
    }
    requeued.push_str(&existing);
    fs::write(path, requeued).map_err(RuntimeError::FailedToWriteTeam)?;
    Ok(())
}

fn config_path(team_dir: &Path) -> PathBuf {
    team_dir.join("config.json")
}

fn serialize_message(message: &TeamMessage) -> Result<String, RuntimeError> {
    serde_json::to_string(message).map_err(RuntimeError::FailedToSerializeTeam)
}
