use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::provider::{ProviderRequestOptions, ToolChoice};

#[cfg(test)]
static NEXT_TEST_TRANSCRIPT_DIR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskConfig {
    pub tasks_dir: PathBuf,
    pub reminder_threshold: usize,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            tasks_dir: PathBuf::from(".tasks"),
            reminder_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamAutonomyConfig {
    pub enabled: bool,
    pub poll_interval: Duration,
    pub idle_timeout: Duration,
}

impl Default for TeamAutonomyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamConfig {
    pub team_dir: PathBuf,
    pub autonomy: TeamAutonomyConfig,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            team_dir: default_team_dir(),
            autonomy: TeamAutonomyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionConfig {
    pub keep_recent_tool_results: usize,
    pub auto_compact_threshold_tokens: Option<usize>,
    pub transcript_dir: PathBuf,
    pub summary_max_input_chars: usize,
    pub summary_max_output_tokens: u32,
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            keep_recent_tool_results: 3,
            auto_compact_threshold_tokens: Some(50_000),
            transcript_dir: default_transcript_dir(),
            summary_max_input_chars: 80_000,
            summary_max_output_tokens: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub base_dir: PathBuf,
    pub auto_route_shell: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            base_dir,
            auto_route_shell: true,
        }
    }
}

#[cfg(not(test))]
fn default_team_dir() -> PathBuf {
    PathBuf::from(".team")
}

#[cfg(test)]
fn default_team_dir() -> PathBuf {
    let suffix = NEXT_TEST_TRANSCRIPT_DIR_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("mentra-test-team")
        .join(format!("process-{}-{suffix}", std::process::id()))
}

#[cfg(not(test))]
fn default_transcript_dir() -> PathBuf {
    PathBuf::from(".transcripts")
}

#[cfg(test)]
fn default_transcript_dir() -> PathBuf {
    let suffix = NEXT_TEST_TRANSCRIPT_DIR_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("mentra-test-transcripts")
        .join(format!("process-{}-{suffix}", std::process::id()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_request_options: ProviderRequestOptions,
    pub team: TeamConfig,
    pub task: TaskConfig,
    pub workspace: WorkspaceConfig,
    pub context_compaction: ContextCompactionConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system: None,
            tool_choice: Some(ToolChoice::default()),
            temperature: None,
            max_output_tokens: Some(8192),
            metadata: BTreeMap::new(),
            provider_request_options: ProviderRequestOptions::default(),
            team: TeamConfig::default(),
            task: TaskConfig::default(),
            workspace: WorkspaceConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
        }
    }
}
