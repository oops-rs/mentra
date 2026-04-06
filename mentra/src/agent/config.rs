use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::compaction::CompactionMode;
#[cfg(test)]
use crate::provider::ToolSearchMode;
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
            tasks_dir: default_tasks_dir(),
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
pub struct CompactionConfig {
    pub keep_recent_tool_results: usize,
    pub auto_compact_threshold_tokens: Option<usize>,
    pub transcript_dir: PathBuf,
    pub summary_max_input_chars: usize,
    pub summary_max_output_tokens: u32,
    #[serde(default)]
    pub mode: CompactionMode,
    pub preserve_recent_user_tokens: usize,
    pub preserve_recent_delegation_results: usize,
    pub max_persisted_transcripts: Option<usize>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            keep_recent_tool_results: 3,
            auto_compact_threshold_tokens: Some(50_000),
            transcript_dir: default_transcript_dir(),
            summary_max_input_chars: 80_000,
            summary_max_output_tokens: 2_000,
            mode: CompactionMode::LocalOnly,
            preserve_recent_user_tokens: 20_000,
            preserve_recent_delegation_results: 8,
            max_persisted_transcripts: Some(10),
        }
    }
}

pub type ContextCompactionConfig = CompactionConfig;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub auto_recall_enabled: bool,
    pub auto_recall_limit: usize,
    pub auto_recall_char_budget: usize,
    pub tool_search_limit: usize,
    pub write_tools_enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_recall_enabled: true,
            auto_recall_limit: 3,
            auto_recall_char_budget: 2_000,
            tool_search_limit: 10,
            write_tools_enabled: true,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProfile {
    #[serde(default)]
    pub allowed_tools: Option<BTreeSet<String>>,
    #[serde(default)]
    pub hidden_tools: BTreeSet<String>,
}

impl ToolProfile {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn only<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed_tools: Some(tools.into_iter().map(Into::into).collect()),
            hidden_tools: BTreeSet::new(),
        }
    }

    pub fn hide<I, S>(tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowed_tools: None,
            hidden_tools: tools.into_iter().map(Into::into).collect(),
        }
    }

    pub fn allows(&self, tool_name: &str) -> bool {
        if let Some(allowed_tools) = &self.allowed_tools
            && !allowed_tools.contains(tool_name)
        {
            return false;
        }

        !self.hidden_tools.contains(tool_name)
    }
}

#[cfg(not(test))]
fn default_team_dir() -> PathBuf {
    crate::default_paths::workspace_default_paths().team_dir
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
    crate::default_paths::workspace_default_paths().transcripts_dir
}

#[cfg(not(test))]
fn default_tasks_dir() -> PathBuf {
    crate::default_paths::workspace_default_paths().tasks_dir
}

#[cfg(test)]
fn default_tasks_dir() -> PathBuf {
    let suffix = NEXT_TEST_TRANSCRIPT_DIR_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("mentra-test-tasks")
        .join(format!("process-{}-{suffix}", std::process::id()))
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
    #[serde(default)]
    pub tool_profile: ToolProfile,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_request_options: ProviderRequestOptions,
    pub team: TeamConfig,
    pub task: TaskConfig,
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(alias = "context_compaction")]
    pub compaction: CompactionConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system: None,
            tool_choice: Some(ToolChoice::default()),
            tool_profile: ToolProfile::default(),
            temperature: None,
            max_output_tokens: Some(8192),
            metadata: BTreeMap::new(),
            provider_request_options: ProviderRequestOptions::default(),
            team: TeamConfig::default(),
            task: TaskConfig::default(),
            workspace: WorkspaceConfig::default(),
            memory: MemoryConfig::default(),
            compaction: CompactionConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::provider::{ReasoningEffort, ReasoningOptions};

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join("mentra-agent-config-tests")
            .join(label)
    }

    #[test]
    fn explicit_paths_override_defaults() {
        let tasks_dir = test_path("custom-tasks");
        let team_dir = test_path("custom-team");
        let transcript_dir = test_path("custom-transcripts");

        let config = AgentConfig {
            task: TaskConfig {
                tasks_dir: tasks_dir.clone(),
                ..Default::default()
            },
            team: TeamConfig {
                team_dir: team_dir.clone(),
                ..Default::default()
            },
            compaction: ContextCompactionConfig {
                transcript_dir: transcript_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(config.task.tasks_dir, tasks_dir);
        assert_eq!(config.team.team_dir, team_dir);
        assert_eq!(config.compaction.transcript_dir, transcript_dir);
    }

    #[test]
    fn tool_profile_defaults_to_allowing_everything() {
        let profile = ToolProfile::default();

        assert!(profile.allows("shell"));
        assert!(profile.allows("files"));
    }

    #[test]
    fn tool_profile_only_restricts_to_allowlist() {
        let profile = ToolProfile::only(["shell", "files"]);

        assert!(profile.allows("shell"));
        assert!(profile.allows("files"));
        assert!(!profile.allows("task"));
    }

    #[test]
    fn tool_profile_hide_blocks_named_tools() {
        let profile = ToolProfile::hide(["shell", "background_run"]);

        assert!(!profile.allows("shell"));
        assert!(!profile.allows("background_run"));
        assert!(profile.allows("files"));
    }

    #[test]
    fn tool_profile_respects_allowlist_and_hidden_overrides() {
        let profile = ToolProfile {
            allowed_tools: Some(["shell", "files"].into_iter().map(str::to_string).collect()),
            hidden_tools: ["shell"].into_iter().map(str::to_string).collect(),
        };

        assert!(!profile.allows("shell"));
        assert!(profile.allows("files"));
        assert!(!profile.allows("task"));
    }

    #[test]
    fn agent_config_deserializes_without_tool_profile_field() {
        let config: AgentConfig = serde_json::from_value(json!({
            "system": null,
            "tool_choice": serde_json::to_value(ToolChoice::Auto).expect("serialize tool choice"),
            "temperature": null,
            "max_output_tokens": 8192,
            "metadata": {},
            "provider_request_options": {},
            "team": TeamConfig::default(),
            "task": TaskConfig::default(),
            "workspace": WorkspaceConfig::default(),
            "memory": MemoryConfig::default(),
            "context_compaction": ContextCompactionConfig::default()
        }))
        .expect("deserialize config without tool profile");

        assert_eq!(config.tool_profile, ToolProfile::default());
    }

    #[test]
    fn provider_request_options_default_to_disabled_tool_search() {
        let options = ProviderRequestOptions::default();

        assert_eq!(options.tool_search_mode, ToolSearchMode::Disabled);
        assert_eq!(options.reasoning, None);
    }

    #[test]
    fn agent_config_deserializes_without_tool_search_mode() {
        let config: AgentConfig = serde_json::from_value(json!({
            "system": null,
            "tool_choice": serde_json::to_value(ToolChoice::Auto).expect("serialize tool choice"),
            "temperature": null,
            "max_output_tokens": 8192,
            "metadata": {},
            "provider_request_options": {
                "responses": {
                    "parallel_tool_calls": true
                }
            },
            "team": TeamConfig::default(),
            "task": TaskConfig::default(),
            "workspace": WorkspaceConfig::default(),
            "memory": MemoryConfig::default(),
            "context_compaction": ContextCompactionConfig::default()
        }))
        .expect("deserialize config without tool search mode");

        assert_eq!(
            config.provider_request_options.tool_search_mode,
            ToolSearchMode::Disabled
        );
        assert_eq!(
            config
                .provider_request_options
                .responses
                .parallel_tool_calls,
            Some(true)
        );
    }

    #[test]
    fn agent_config_deserializes_reasoning_options() {
        let config: AgentConfig = serde_json::from_value(json!({
            "system": null,
            "tool_choice": serde_json::to_value(ToolChoice::Auto).expect("serialize tool choice"),
            "temperature": null,
            "max_output_tokens": 8192,
            "metadata": {},
            "provider_request_options": {
                "reasoning": {
                    "effort": "high"
                }
            },
            "team": TeamConfig::default(),
            "task": TaskConfig::default(),
            "workspace": WorkspaceConfig::default(),
            "memory": MemoryConfig::default(),
            "context_compaction": ContextCompactionConfig::default()
        }))
        .expect("deserialize config with reasoning options");

        assert_eq!(
            config.provider_request_options.reasoning,
            Some(ReasoningOptions {
                effort: Some(ReasoningEffort::High),
                summary: None,
            })
        );
    }
}
