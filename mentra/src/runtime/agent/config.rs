use std::{collections::BTreeMap, path::PathBuf};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::provider::ToolChoice;

#[cfg(test)]
static NEXT_TEST_TRANSCRIPT_DIR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGraphConfig {
    pub tasks_dir: PathBuf,
    pub reminder_threshold: usize,
}

impl Default for TaskGraphConfig {
    fn default() -> Self {
        Self {
            tasks_dir: PathBuf::from(".tasks"),
            reminder_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamConfig {
    pub team_dir: PathBuf,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            team_dir: default_team_dir(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: BTreeMap<String, String>,
    pub team: TeamConfig,
    pub task_graph: TaskGraphConfig,
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
            team: TeamConfig::default(),
            task_graph: TaskGraphConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
        }
    }
}
