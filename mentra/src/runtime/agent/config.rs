use std::{collections::BTreeMap, path::PathBuf};

use crate::provider::ToolChoice;

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
            transcript_dir: PathBuf::from(".transcripts"),
            summary_max_input_chars: 80_000,
            summary_max_output_tokens: 2_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: BTreeMap<String, String>,
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
            task_graph: TaskGraphConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
        }
    }
}
