use crate::{
    ContentBlock,
    agent::{AgentEvent, CompactionDetails, SpawnedAgentStatus, SpawnedAgentSummary},
    background::{BackgroundTaskStatus, BackgroundTaskSummary},
    session::event::{EventSeq, SessionEvent, TaskKind, TaskLifecycleStatus, ToolMutability},
    team::{TeamMemberStatus, TeamMemberSummary},
    tool::{ToolExecutionCategory, ToolSideEffectLevel},
};

/// Maps an `AgentEvent` to zero or more `SessionEvent` values.
///
/// Some agent events map one-to-one, others produce multiple session events
/// (e.g. compaction), and some are intentionally silenced at the session layer.
pub(crate) fn map_agent_event(
    event: &AgentEvent,
    seq: &mut EventSeq,
) -> Vec<(EventSeq, SessionEvent)> {
    let mut out = Vec::new();

    let mapped = map_event_inner(event);
    for session_event in mapped {
        let current_seq = *seq;
        *seq += 1;
        out.push((current_seq, session_event));
    }

    out
}

fn map_event_inner(event: &AgentEvent) -> Vec<SessionEvent> {
    match event {
        AgentEvent::TextDelta { delta, full_text } => {
            vec![SessionEvent::AssistantTokenDelta {
                delta: delta.clone(),
                full_text: full_text.clone(),
            }]
        }

        AgentEvent::ToolUseReady { call, .. } => {
            let input_str = call.input.to_string();
            let summary = derive_tool_summary(&call.name, &input_str);
            vec![SessionEvent::ToolQueued {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                summary,
                mutability: ToolMutability::Unknown,
                input_json: input_str,
            }]
        }

        AgentEvent::ToolExecutionStarted { call } => {
            vec![SessionEvent::ToolStarted {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
            }]
        }

        AgentEvent::ToolExecutionFinished { result } => map_tool_result(result),

        AgentEvent::ToolExecutionProgress {
            id,
            name,
            progress,
        } => {
            vec![SessionEvent::ToolProgress {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                progress: progress.clone(),
            }]
        }

        AgentEvent::ContextCompacted { details } => map_compaction(details),

        AgentEvent::SubagentSpawned { agent } => map_subagent(agent, TaskLifecycleStatus::Spawned),
        AgentEvent::SubagentFinished { agent } => map_subagent_finished(agent),

        AgentEvent::BackgroundTaskStarted { task } => {
            map_background_task(task, TaskLifecycleStatus::Running)
        }
        AgentEvent::BackgroundTaskFinished { task } => map_background_task_finished(task),

        AgentEvent::TeammateSpawned { teammate } => {
            map_teammate(teammate, TaskLifecycleStatus::Spawned)
        }
        AgentEvent::TeammateUpdated { teammate } => map_teammate_updated(teammate),

        // Events handled at Session level or intentionally silent at session layer.
        AgentEvent::AssistantMessageCommitted { .. }
        | AgentEvent::RunStarted
        | AgentEvent::RunFinished
        | AgentEvent::RunFailed { .. }
        | AgentEvent::ToolUseUpdated { .. }
        | AgentEvent::TeamProtocolRequested { .. }
        | AgentEvent::TeamProtocolResolved { .. }
        | AgentEvent::TeamInboxUpdated { .. } => Vec::new(),
    }
}

fn map_tool_result(block: &ContentBlock) -> Vec<SessionEvent> {
    if let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = block
    {
        let summary = truncate_input_summary(&content.to_display_string(), 200);
        vec![SessionEvent::ToolCompleted {
            tool_call_id: tool_use_id.clone(),
            tool_name: String::new(), // tool name not available on ToolResult
            summary,
            is_error: *is_error,
        }]
    } else {
        Vec::new()
    }
}

fn map_compaction(details: &CompactionDetails) -> Vec<SessionEvent> {
    vec![
        SessionEvent::CompactionStarted {
            agent_id: details.agent_id.clone(),
        },
        SessionEvent::CompactionCompleted {
            agent_id: details.agent_id.clone(),
            replaced_items: details.replaced_items,
            preserved_items: details.preserved_items,
            resulting_transcript_len: details.resulting_transcript_len,
        },
    ]
}

fn map_subagent(agent: &SpawnedAgentSummary, status: TaskLifecycleStatus) -> Vec<SessionEvent> {
    vec![SessionEvent::TaskUpdated {
        task_id: agent.id.clone(),
        kind: TaskKind::Subagent,
        status,
        title: agent.name.clone(),
        detail: None,
    }]
}

fn map_subagent_finished(agent: &SpawnedAgentSummary) -> Vec<SessionEvent> {
    let status = match &agent.status {
        SpawnedAgentStatus::Finished => TaskLifecycleStatus::Finished,
        SpawnedAgentStatus::Failed(_) => TaskLifecycleStatus::Failed,
        SpawnedAgentStatus::Running => TaskLifecycleStatus::Running,
    };
    let detail = match &agent.status {
        SpawnedAgentStatus::Failed(msg) => Some(msg.clone()),
        _ => None,
    };
    vec![SessionEvent::TaskUpdated {
        task_id: agent.id.clone(),
        kind: TaskKind::Subagent,
        status,
        title: agent.name.clone(),
        detail,
    }]
}

fn map_background_task(
    task: &BackgroundTaskSummary,
    status: TaskLifecycleStatus,
) -> Vec<SessionEvent> {
    vec![SessionEvent::TaskUpdated {
        task_id: task.id.clone(),
        kind: TaskKind::BackgroundTask,
        status,
        title: task.command.clone(),
        detail: task.output_preview.clone(),
    }]
}

fn map_background_task_finished(task: &BackgroundTaskSummary) -> Vec<SessionEvent> {
    let status = match task.status {
        BackgroundTaskStatus::Finished => TaskLifecycleStatus::Finished,
        BackgroundTaskStatus::Failed | BackgroundTaskStatus::Interrupted => {
            TaskLifecycleStatus::Failed
        }
        BackgroundTaskStatus::Running => TaskLifecycleStatus::Running,
    };
    vec![SessionEvent::TaskUpdated {
        task_id: task.id.clone(),
        kind: TaskKind::BackgroundTask,
        status,
        title: task.command.clone(),
        detail: task.output_preview.clone(),
    }]
}

fn map_teammate(teammate: &TeamMemberSummary, status: TaskLifecycleStatus) -> Vec<SessionEvent> {
    vec![SessionEvent::TaskUpdated {
        task_id: teammate.id.clone(),
        kind: TaskKind::Teammate,
        status,
        title: teammate.name.clone(),
        detail: Some(teammate.role.clone()),
    }]
}

fn map_teammate_updated(teammate: &TeamMemberSummary) -> Vec<SessionEvent> {
    let status = match &teammate.status {
        TeamMemberStatus::Idle | TeamMemberStatus::Working => TaskLifecycleStatus::Running,
        TeamMemberStatus::Shutdown => TaskLifecycleStatus::Finished,
        TeamMemberStatus::Failed(_) => TaskLifecycleStatus::Failed,
    };
    let detail = match &teammate.status {
        TeamMemberStatus::Failed(msg) => Some(msg.clone()),
        _ => Some(teammate.role.clone()),
    };
    vec![SessionEvent::TaskUpdated {
        task_id: teammate.id.clone(),
        kind: TaskKind::Teammate,
        status,
        title: teammate.name.clone(),
        detail,
    }]
}

#[allow(dead_code)] // exposed for session-handle enrichment in upcoming tasks
pub(crate) fn classify_mutability(
    side_effect_level: ToolSideEffectLevel,
    execution_category: ToolExecutionCategory,
) -> ToolMutability {
    match (side_effect_level, execution_category) {
        (ToolSideEffectLevel::None, _) => ToolMutability::ReadOnly,
        (_, ToolExecutionCategory::ReadOnlyParallel) => ToolMutability::ReadOnly,
        _ => ToolMutability::Mutating,
    }
}

pub(crate) fn derive_tool_summary(tool_name: &str, input_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(input_json) {
        if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
            return format!("{tool_name}: {}", truncate_input_summary(command, 100));
        }
        if let Some(path) = value.get("path").and_then(|v| v.as_str()) {
            return format!("{tool_name}: {}", truncate_input_summary(path, 100));
        }
        if let Some(file_path) = value.get("file_path").and_then(|v| v.as_str()) {
            return format!("{tool_name}: {}", truncate_input_summary(file_path, 100));
        }
    }
    format!(
        "{tool_name}({})",
        truncate_input_summary(input_json, 60)
    )
}

fn truncate_input_summary(input: &str, max_len: usize) -> String {
    if input.len() <= max_len {
        input.to_string()
    } else {
        let mut truncated = input[..max_len].to_string();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tool::ToolCall;

    #[test]
    fn text_delta_maps_to_assistant_token_delta() {
        let event = AgentEvent::TextDelta {
            delta: "hi".to_string(),
            full_text: "hi".to_string(),
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::AssistantTokenDelta { delta, .. } if delta == "hi"
        ));
        assert_eq!(seq, 1);
    }

    #[test]
    fn tool_use_ready_maps_to_tool_queued() {
        let event = AgentEvent::ToolUseReady {
            index: 0,
            call: ToolCall {
                id: "tc-1".to_string(),
                name: "read".to_string(),
                input: json!({"path": "/foo"}),
            },
        };
        let mut seq = 10;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::ToolQueued { tool_call_id, tool_name, .. }
            if tool_call_id == "tc-1" && tool_name == "read"
        ));
        assert_eq!(mapped[0].0, 10);
        assert_eq!(seq, 11);
    }

    #[test]
    fn tool_execution_finished_maps_to_tool_completed() {
        let event = AgentEvent::ToolExecutionFinished {
            result: ContentBlock::ToolResult {
                tool_use_id: "tc-2".to_string(),
                content: mentra_provider::ToolResultContent::text("ok"),
                is_error: false,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::ToolCompleted { tool_call_id, is_error, .. }
            if tool_call_id == "tc-2" && !is_error
        ));
    }

    #[test]
    fn compaction_maps_to_started_and_completed() {
        let event = AgentEvent::ContextCompacted {
            details: CompactionDetails {
                trigger: crate::agent::CompactionTrigger::Auto,
                mode: crate::compaction::CompactionExecutionMode::Local,
                agent_id: "a1".to_string(),
                transcript_path: std::path::PathBuf::from("/tmp"),
                replaced_items: 10,
                preserved_items: 5,
                preserved_user_turns: 2,
                preserved_delegation_results: 1,
                resulting_transcript_len: 7,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 2);
        assert!(matches!(&mapped[0].1, SessionEvent::CompactionStarted { .. }));
        assert!(matches!(
            &mapped[1].1,
            SessionEvent::CompactionCompleted { .. }
        ));
        assert_eq!(seq, 2);
    }

    #[test]
    fn run_started_maps_to_empty() {
        let event = AgentEvent::RunStarted;
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert!(mapped.is_empty());
        assert_eq!(seq, 0);
    }

    // --- classify_mutability tests ---

    #[test]
    fn classify_mutability_no_side_effects_is_read_only() {
        let result = classify_mutability(
            ToolSideEffectLevel::None,
            ToolExecutionCategory::ExclusiveLocalMutation,
        );
        assert_eq!(result, ToolMutability::ReadOnly);
    }

    #[test]
    fn classify_mutability_read_only_parallel_is_read_only() {
        let result = classify_mutability(
            ToolSideEffectLevel::Process,
            ToolExecutionCategory::ReadOnlyParallel,
        );
        assert_eq!(result, ToolMutability::ReadOnly);
    }

    #[test]
    fn classify_mutability_side_effects_exclusive_is_mutating() {
        let result = classify_mutability(
            ToolSideEffectLevel::LocalState,
            ToolExecutionCategory::ExclusiveLocalMutation,
        );
        assert_eq!(result, ToolMutability::Mutating);
    }

    #[test]
    fn classify_mutability_external_delegation_is_mutating() {
        let result = classify_mutability(
            ToolSideEffectLevel::External,
            ToolExecutionCategory::Delegation,
        );
        assert_eq!(result, ToolMutability::Mutating);
    }

    #[test]
    fn classify_mutability_none_with_read_only_parallel_is_read_only() {
        let result = classify_mutability(
            ToolSideEffectLevel::None,
            ToolExecutionCategory::ReadOnlyParallel,
        );
        assert_eq!(result, ToolMutability::ReadOnly);
    }

    // --- derive_tool_summary tests ---

    #[test]
    fn derive_tool_summary_extracts_command() {
        let summary = derive_tool_summary("shell", r#"{"command":"ls -la /tmp"}"#);
        assert_eq!(summary, "shell: ls -la /tmp");
    }

    #[test]
    fn derive_tool_summary_extracts_path() {
        let summary = derive_tool_summary("read", r#"{"path":"/home/user/file.rs"}"#);
        assert_eq!(summary, "read: /home/user/file.rs");
    }

    #[test]
    fn derive_tool_summary_extracts_file_path() {
        let summary = derive_tool_summary("write", r#"{"file_path":"/tmp/out.txt","content":"hi"}"#);
        assert_eq!(summary, "write: /tmp/out.txt");
    }

    #[test]
    fn derive_tool_summary_falls_back_to_raw_input() {
        let summary = derive_tool_summary("custom", r#"{"foo":"bar"}"#);
        assert_eq!(summary, r#"custom({"foo":"bar"})"#);
    }

    #[test]
    fn derive_tool_summary_handles_invalid_json() {
        let summary = derive_tool_summary("broken", "not json at all");
        assert_eq!(summary, "broken(not json at all)");
    }

    #[test]
    fn derive_tool_summary_truncates_long_command() {
        let long_cmd = "x".repeat(200);
        let input = format!(r#"{{"command":"{long_cmd}"}}"#);
        let summary = derive_tool_summary("shell", &input);
        assert!(summary.len() < 200);
        assert!(summary.ends_with("..."));
    }

    // --- ToolExecutionProgress mapping test ---

    #[test]
    fn tool_execution_progress_maps_to_tool_progress() {
        let event = AgentEvent::ToolExecutionProgress {
            id: "tc-5".to_string(),
            name: "shell".to_string(),
            progress: "50% complete".to_string(),
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::ToolProgress { tool_call_id, tool_name, progress }
            if tool_call_id == "tc-5" && tool_name == "shell" && progress == "50% complete"
        ));
        assert_eq!(seq, 1);
    }

    // --- tool_use_ready now uses derive_tool_summary ---

    #[test]
    fn tool_use_ready_summary_uses_path_field() {
        let event = AgentEvent::ToolUseReady {
            index: 0,
            call: ToolCall {
                id: "tc-10".to_string(),
                name: "read".to_string(),
                input: json!({"path": "/src/main.rs"}),
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        if let SessionEvent::ToolQueued { summary, .. } = &mapped[0].1 {
            assert_eq!(summary, "read: /src/main.rs");
        } else {
            panic!("expected ToolQueued");
        }
    }

    // --- SubagentSpawned / SubagentFinished mapping tests ---

    #[test]
    fn subagent_spawned_maps_to_task_updated_spawned() {
        let event = AgentEvent::SubagentSpawned {
            agent: SpawnedAgentSummary {
                id: "sub-1".to_string(),
                name: "researcher".to_string(),
                model: "mock-model".to_string(),
                status: SpawnedAgentStatus::Running,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::Subagent,
                status: TaskLifecycleStatus::Spawned,
                title,
                detail: None,
            }
            if task_id == "sub-1" && title == "researcher"
        ));
        assert_eq!(seq, 1);
    }

    #[test]
    fn subagent_finished_success_maps_to_task_updated_finished() {
        let event = AgentEvent::SubagentFinished {
            agent: SpawnedAgentSummary {
                id: "sub-2".to_string(),
                name: "analyst".to_string(),
                model: "mock-model".to_string(),
                status: SpawnedAgentStatus::Finished,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::Subagent,
                status: TaskLifecycleStatus::Finished,
                title,
                detail: None,
            }
            if task_id == "sub-2" && title == "analyst"
        ));
    }

    #[test]
    fn subagent_finished_failure_maps_to_task_updated_failed_with_detail() {
        let event = AgentEvent::SubagentFinished {
            agent: SpawnedAgentSummary {
                id: "sub-3".to_string(),
                name: "writer".to_string(),
                model: "mock-model".to_string(),
                status: SpawnedAgentStatus::Failed("provider timeout".to_string()),
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::Subagent,
                status: TaskLifecycleStatus::Failed,
                title,
                detail: Some(msg),
            }
            if task_id == "sub-3" && title == "writer" && msg == "provider timeout"
        ));
    }

    // --- BackgroundTaskStarted / BackgroundTaskFinished mapping tests ---

    #[test]
    fn background_task_started_maps_to_task_updated_running() {
        let event = AgentEvent::BackgroundTaskStarted {
            task: BackgroundTaskSummary {
                id: "bg-1".to_string(),
                command: "cargo test".to_string(),
                cwd: std::path::PathBuf::from("/tmp"),
                status: BackgroundTaskStatus::Running,
                output_preview: None,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::BackgroundTask,
                status: TaskLifecycleStatus::Running,
                title,
                detail: None,
            }
            if task_id == "bg-1" && title == "cargo test"
        ));
    }

    #[test]
    fn background_task_finished_success_maps_to_task_updated_finished() {
        let event = AgentEvent::BackgroundTaskFinished {
            task: BackgroundTaskSummary {
                id: "bg-2".to_string(),
                command: "npm run build".to_string(),
                cwd: std::path::PathBuf::from("/project"),
                status: BackgroundTaskStatus::Finished,
                output_preview: Some("Build complete".to_string()),
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::BackgroundTask,
                status: TaskLifecycleStatus::Finished,
                title,
                detail: Some(preview),
            }
            if task_id == "bg-2" && title == "npm run build" && preview == "Build complete"
        ));
    }

    #[test]
    fn background_task_finished_failure_maps_to_task_updated_failed() {
        let event = AgentEvent::BackgroundTaskFinished {
            task: BackgroundTaskSummary {
                id: "bg-3".to_string(),
                command: "make".to_string(),
                cwd: std::path::PathBuf::from("/build"),
                status: BackgroundTaskStatus::Failed,
                output_preview: Some("exit code 2".to_string()),
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::BackgroundTask,
                status: TaskLifecycleStatus::Failed,
                title,
                detail: Some(preview),
            }
            if task_id == "bg-3" && title == "make" && preview == "exit code 2"
        ));
    }

    // --- TeammateSpawned / TeammateUpdated mapping tests ---

    #[test]
    fn teammate_spawned_maps_to_task_updated_spawned() {
        let event = AgentEvent::TeammateSpawned {
            teammate: TeamMemberSummary {
                id: "tm-1".to_string(),
                name: "reviewer".to_string(),
                role: "code review".to_string(),
                model: "mock-model".to_string(),
                status: TeamMemberStatus::Idle,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                task_id,
                kind: TaskKind::Teammate,
                status: TaskLifecycleStatus::Spawned,
                title,
                detail: Some(role),
            }
            if task_id == "tm-1" && title == "reviewer" && role == "code review"
        ));
    }

    #[test]
    fn teammate_updated_shutdown_maps_to_finished() {
        let event = AgentEvent::TeammateUpdated {
            teammate: TeamMemberSummary {
                id: "tm-2".to_string(),
                name: "tester".to_string(),
                role: "testing".to_string(),
                model: "mock-model".to_string(),
                status: TeamMemberStatus::Shutdown,
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                status: TaskLifecycleStatus::Finished,
                ..
            }
        ));
    }

    #[test]
    fn teammate_updated_failed_maps_to_failed_with_message() {
        let event = AgentEvent::TeammateUpdated {
            teammate: TeamMemberSummary {
                id: "tm-3".to_string(),
                name: "deployer".to_string(),
                role: "deploy".to_string(),
                model: "mock-model".to_string(),
                status: TeamMemberStatus::Failed("connection refused".to_string()),
            },
        };
        let mut seq = 0;
        let mapped = map_agent_event(&event, &mut seq);
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0].1,
            SessionEvent::TaskUpdated {
                status: TaskLifecycleStatus::Failed,
                detail: Some(msg),
                ..
            }
            if msg == "connection refused"
        ));
    }
}
