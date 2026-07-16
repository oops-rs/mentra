mod common;

use mentra::{
    ContentBlock, NewTask, TaskPatch, TerminalOutputSpec,
    agent::{AgentConfig, AgentStatus, TaskConfig},
    runtime::{RunOptions, TaskStatus},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct HostReport {
    summary: String,
    risks: Vec<String>,
    next_action: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = common::openai_runtime()?;
    let model = common::openai_model(&runtime).await?;
    let task_namespace = std::env::temp_dir().join(format!(
        "mentra-host-orchestration-example-{}",
        std::process::id()
    ));
    let board = runtime.task_board(&task_namespace);

    // The host assigns and starts the task. The model never decides which
    // agent to spawn or how task state transitions.
    let task = board.create(NewTask {
        description: "Inspect the current workspace and prepare a concise engineering report."
            .to_string(),
        owner: "report-agent".to_string(),
        ..NewTask::new("Prepare workspace engineering report")
    })?;
    board.update(
        task.id,
        TaskPatch {
            status: Some(TaskStatus::InProgress),
            ..TaskPatch::default()
        },
    )?;

    let mut agent = runtime.spawn_with_config(
        "report-agent",
        model,
        AgentConfig {
            task: TaskConfig {
                tasks_dir: task_namespace,
                ..TaskConfig::default()
            },
            ..AgentConfig::default()
        },
    )?;

    // Obtain owned handles before run(&mut self). The wait future and steering
    // handle remain usable while the run owns the mutable agent borrow.
    let steering = agent.steering_handle();
    let run_started = agent.wait_for_snapshot(|snapshot| {
        snapshot.run_generation > 0
            && matches!(
                snapshot.status,
                AgentStatus::AwaitingModel | AgentStatus::Streaming
            )
    });
    let steer_during_run = async {
        run_started.await;
        steering.steer(vec![ContentBlock::text(
            "Prioritize concrete risks and make every recommendation actionable.",
        )]);
    };

    // Phase 1 is fixed by the host: gather and refine notes. Steering is
    // injected inside Mentra's loop at the next eligible boundary.
    let (work_result, ()) = tokio::join!(
        agent.run(
            vec![ContentBlock::text(format!(
                "Work on task {}: {}. Gather the facts needed for a final report.",
                task.id, task.subject
            ))],
            RunOptions::default(),
        ),
        steer_during_run,
    );
    work_result?;

    // If the model finished before reaching another boundary, the queued steer
    // remains agent-scoped; the host explicitly starts it rather than relying
    // on implicit auto-run behavior.
    if steering.has_pending() {
        agent.run_queued(RunOptions::default()).await?;
    }

    // Phase 2 is also host-selected: force a typed terminal tool and deserialize
    // its exact transcript detail. No model response-format routing is involved.
    let output = agent
        .run_to_output::<HostReport>(
            vec![ContentBlock::text(
                "Return the final report using the required terminal tool.",
            )],
            RunOptions::default(),
            TerminalOutputSpec::new(
                "finish_workspace_report",
                "Return the completed workspace engineering report.",
                json!({
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string" },
                        "risks": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "next_action": { "type": "string" }
                    },
                    "required": ["summary", "risks", "next_action"]
                }),
            ),
        )
        .await?;

    // The host owns completion policy and updates the same transactional board.
    board.update(
        task.id,
        TaskPatch {
            status: Some(TaskStatus::Completed),
            ..TaskPatch::default()
        },
    )?;

    println!("Summary: {}", output.value.summary);
    println!("Risks:");
    for risk in output.value.risks {
        println!("- {risk}");
    }
    println!("Next action: {}", output.value.next_action);
    Ok(())
}
