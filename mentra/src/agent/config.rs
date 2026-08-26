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

/// Controls request-only tool-result elision and canonical summary compaction.
///
/// These mechanisms are separate. Request-only elision changes a cloned main
/// model request and does not further change the persisted transcript. Summary
/// compaction replaces canonical transcript items and persists the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// How many of the most recent tool results remain unchanged when the
    /// history is rebuilt for a provider request.
    ///
    /// Every older result larger than 100 bytes is replaced by a
    /// `[Previous: used <tool>]` marker. That rewrite runs before every main
    /// model request, at any context size, and the projected history is also
    /// what the auto-compaction threshold measures. Each changed request emits
    /// [`AgentEvent::RequestToolResultsElided`](crate::agent::AgentEvent::RequestToolResultsElided).
    ///
    /// This is a count heuristic, not a request-size bound: the newest results
    /// can be arbitrarily large, old results of at most 100 bytes survive, and
    /// non-tool content is unaffected. `usize::MAX` disables the rewrite and is
    /// the default. Lower it only for a workload whose old tool results are
    /// genuinely disposable.
    pub keep_recent_tool_results: usize,
    /// The token count above which a run compacts, when the model's context
    /// window is unknown.
    ///
    /// `None` disables auto-compaction outright, whatever the window is.
    pub auto_compact_threshold_tokens: Option<usize>,
    /// The percentage of the model's context window to compact at, when the
    /// window *is* known.
    ///
    /// A single absolute token count is the one model-dependent constant that
    /// cannot be model-independent: 50k is most of a 64k window and a rounding
    /// error in a 1M one, so a fixed number either compacts a large model far
    /// too eagerly or leaves a small one to overflow. When
    /// [`ModelInfo::context_window`](crate::ModelInfo::context_window) is
    /// known, this percentage of it wins; otherwise
    /// `auto_compact_threshold_tokens` does. `None` here always uses the
    /// absolute number. Values above 100 are treated as 100.
    #[serde(default = "default_auto_compact_threshold_percent")]
    pub auto_compact_threshold_percent: Option<u8>,
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
            keep_recent_tool_results: usize::MAX,
            auto_compact_threshold_tokens: Some(50_000),
            auto_compact_threshold_percent: default_auto_compact_threshold_percent(),
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

fn default_auto_compact_threshold_percent() -> Option<u8> {
    // Leaves a quarter of the window for the turn that follows the compaction:
    // the summary, the next user message, and whatever tool results that turn
    // produces all have to fit after the threshold is crossed.
    Some(75)
}

impl CompactionConfig {
    /// Resolves the token count at which a run compacts, for a model whose
    /// context window is `context_window`.
    ///
    /// `None` means auto-compaction is off.
    pub fn auto_compact_threshold(&self, context_window: Option<usize>) -> Option<usize> {
        let fallback = self.auto_compact_threshold_tokens?;

        match (context_window, self.auto_compact_threshold_percent) {
            (Some(window), Some(percent)) => {
                Some(window.saturating_mul(percent.min(100) as usize) / 100)
            }
            _ => Some(fallback),
        }
    }
}

pub type ContextCompactionConfig = CompactionConfig;

/// Bounds how much of an oversized tool result enters the model's view.
///
/// A result at or below `threshold_bytes` is inserted byte-identically to a
/// run without paging. Above it, the transcript receives the first window
/// (at most `page_bytes`, cut on a line boundary) plus a trailer naming the
/// `read_tool_result` call that returns the next window; the full result is
/// retained in memory for the life of the agent so nothing is lost.
///
/// Paging is applied *after* the runtime's own tool-result limiter
/// (`RuntimePolicy::with_max_tool_result_bytes` /
/// `with_max_tool_result_lines`), so a `threshold_bytes` above those caps
/// never triggers — the limiter clamps the result first. Enabling paging
/// therefore means raising the policy caps to whatever a tool may legitimately
/// return and leaving them as the anti-abuse backstop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultPagingConfig {
    /// Results at or below this size are inserted whole. Default 64 KiB.
    pub threshold_bytes: usize,
    /// Maximum bytes per inserted page/window. Default 32 KiB.
    pub page_bytes: usize,
}

impl Default for ToolResultPagingConfig {
    fn default() -> Self {
        Self {
            threshold_bytes: 64 * 1024,
            page_bytes: 32 * 1024,
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
    /// `None` (the default) preserves the unpaged behaviour exactly: every
    /// tool result enters the transcript as produced, and `read_tool_result`
    /// is absent from the agent's tool roster.
    #[serde(default)]
    pub tool_result_paging: Option<ToolResultPagingConfig>,
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
            tool_result_paging: None,
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
    fn a_known_context_window_sets_the_threshold_not_a_constant() {
        // 50k is most of a 64k window and a rounding error in a 1M one. The
        // same config has to mean something different for each.
        let compaction = CompactionConfig::default();

        assert_eq!(
            compaction.auto_compact_threshold(Some(1_048_576)),
            Some(786_432)
        );
        assert_eq!(
            compaction.auto_compact_threshold(Some(64_000)),
            Some(48_000)
        );
    }

    #[test]
    fn an_unknown_context_window_falls_back_to_the_absolute_threshold() {
        let compaction = CompactionConfig::default();

        assert_eq!(compaction.auto_compact_threshold(None), Some(50_000));
    }

    #[test]
    fn clearing_the_token_threshold_disables_compaction_at_any_window() {
        // `None` has always meant off, and a known window must not switch it
        // back on.
        let compaction = CompactionConfig {
            auto_compact_threshold_tokens: None,
            ..Default::default()
        };

        assert_eq!(compaction.auto_compact_threshold(Some(200_000)), None);
        assert_eq!(compaction.auto_compact_threshold(None), None);
    }

    #[test]
    fn clearing_the_percentage_pins_the_threshold_to_the_absolute_number() {
        let compaction = CompactionConfig {
            auto_compact_threshold_percent: None,
            ..Default::default()
        };

        assert_eq!(
            compaction.auto_compact_threshold(Some(1_000_000)),
            Some(50_000)
        );
    }

    #[test]
    fn compaction_keeps_every_tool_result_by_default() {
        let compaction = CompactionConfig::default();

        assert_eq!(compaction.keep_recent_tool_results, usize::MAX);
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
    fn tool_result_paging_is_disabled_by_default() {
        assert_eq!(AgentConfig::default().tool_result_paging, None);
    }

    #[test]
    fn tool_result_paging_defaults_to_64_kib_threshold_and_32_kib_pages() {
        let paging = ToolResultPagingConfig::default();

        assert_eq!(paging.threshold_bytes, 64 * 1024);
        assert_eq!(paging.page_bytes, 32 * 1024);
    }

    #[test]
    fn agent_config_deserializes_without_tool_result_paging_field() {
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
        .expect("deserialize config persisted before paging existed");

        assert_eq!(config.tool_result_paging, None);
    }

    #[test]
    fn agent_config_round_trips_tool_result_paging() {
        let config = AgentConfig {
            tool_result_paging: Some(ToolResultPagingConfig {
                threshold_bytes: 4_096,
                page_bytes: 1_024,
            }),
            ..Default::default()
        };

        let restored: AgentConfig =
            serde_json::from_value(serde_json::to_value(&config).expect("serialize config"))
                .expect("deserialize config");

        assert_eq!(restored.tool_result_paging, config.tool_result_paging);
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
