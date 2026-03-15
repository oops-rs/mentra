use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::{
    Agent, ContentBlock, agent::TeamAutonomyConfig, error::RuntimeError, runtime::CancellationToken,
};

use super::{TeamManager, TeamMemberStatus};

const TEAM_WAKE_PROMPT: &str = "Process any new team inbox messages and continue your work.";
const BACKGROUND_WAKE_PROMPT: &str =
    "Review any completed background task results and continue your work.";

pub(crate) async fn teammate_actor_loop(
    manager: TeamManager,
    team_dir: PathBuf,
    teammate_name: String,
    agent: Arc<AsyncMutex<Agent>>,
    mut wake_rx: mpsc::UnboundedReceiver<()>,
    cancellation: CancellationToken,
) {
    let autonomy = {
        let guard = agent.lock().await;
        guard.config().team.autonomy.clone()
    };
    let mut should_process = false;
    let mut idle_since = None;

    loop {
        if cancellation.is_cancelled() {
            break;
        }

        if should_process {
            match process_pending_work(&manager, &team_dir, &teammate_name, &agent).await {
                Ok(ActorState::Idle) => {
                    idle_since.get_or_insert_with(Instant::now);
                }
                Ok(ActorState::Shutdown) => break,
                Err(()) => {
                    idle_since.get_or_insert_with(Instant::now);
                }
            }
            should_process = false;
        }

        if cancellation.is_cancelled() {
            break;
        }

        if autonomy.enabled {
            let started_idle_at = idle_since.unwrap_or_else(|| {
                let now = Instant::now();
                idle_since = Some(now);
                now
            });

            let wait = tokio::time::sleep(autonomy.poll_interval);
            tokio::pin!(wait);

            tokio::select! {
                wake = wake_rx.recv() => {
                    match wake {
                        Some(()) => {
                            should_process = true;
                            idle_since = None;
                        }
                        None => break,
                    }
                }
                _ = &mut wait => {
                    match autonomy_tick(
                        &manager,
                        &team_dir,
                        &teammate_name,
                        &agent,
                        started_idle_at,
                        &autonomy,
                    ).await {
                        Ok(AutonomyState::ContinueIdle) => {}
                        Ok(AutonomyState::Claimed(prompt)) => {
                            if execute_prompt(&manager, &team_dir, &teammate_name, &agent, prompt)
                                .await
                                .is_err()
                            {
                                idle_since.get_or_insert_with(Instant::now);
                                continue;
                            }
                            let _ = manager.update_member_status(
                                &team_dir,
                                &teammate_name,
                                TeamMemberStatus::Idle,
                            );
                            should_process = true;
                            idle_since = Some(Instant::now());
                        }
                        Ok(AutonomyState::Shutdown) => break,
                        Err(()) => {
                            idle_since.get_or_insert_with(Instant::now);
                        }
                    }
                }
            }
        } else {
            match wake_rx.recv().await {
                Some(()) => {
                    should_process = true;
                    idle_since = None;
                }
                None => break,
            }
        }
    }

    let _ = manager.unregister_teammate_actor(&team_dir, &teammate_name);
}

enum ActorState {
    Idle,
    Shutdown,
}

enum PendingWork {
    Prompt(String),
    Idle,
    Shutdown,
}

enum AutonomyState {
    ContinueIdle,
    Claimed(String),
    Shutdown,
}

async fn process_pending_work(
    manager: &TeamManager,
    team_dir: &Path,
    teammate_name: &str,
    agent: &Arc<AsyncMutex<Agent>>,
) -> Result<ActorState, ()> {
    let mut processed_prompt = false;

    loop {
        match next_pending_work(manager, team_dir, teammate_name, agent).await {
            Ok(PendingWork::Prompt(prompt)) => {
                processed_prompt = true;
                execute_prompt(manager, team_dir, teammate_name, agent, prompt).await?
            }
            Ok(PendingWork::Idle) => {
                if processed_prompt {
                    let _ = manager.update_member_status(
                        team_dir,
                        teammate_name,
                        TeamMemberStatus::Idle,
                    );
                }
                return Ok(ActorState::Idle);
            }
            Ok(PendingWork::Shutdown) => {
                let _ = manager.update_member_status(
                    team_dir,
                    teammate_name,
                    TeamMemberStatus::Shutdown,
                );
                return Ok(ActorState::Shutdown);
            }
            Err(error) => {
                let _ = mark_failed(manager, team_dir, teammate_name, error);
                return Err(());
            }
        }
    }
}

async fn next_pending_work(
    manager: &TeamManager,
    team_dir: &Path,
    teammate_name: &str,
    agent: &Arc<AsyncMutex<Agent>>,
) -> Result<PendingWork, RuntimeError> {
    if manager.has_pending_messages(team_dir, teammate_name)? {
        return Ok(PendingWork::Prompt(TEAM_WAKE_PROMPT.to_string()));
    }

    let has_background_notifications = {
        let guard = agent.lock().await;
        guard
            .runtime_handle()
            .has_deliverable_background_notifications(guard.id())
    };
    if has_background_notifications {
        return Ok(PendingWork::Prompt(BACKGROUND_WAKE_PROMPT.to_string()));
    }

    if manager.take_shutdown_signal(team_dir, teammate_name)? {
        return Ok(PendingWork::Shutdown);
    }

    Ok(PendingWork::Idle)
}

async fn autonomy_tick(
    manager: &TeamManager,
    team_dir: &Path,
    teammate_name: &str,
    agent: &Arc<AsyncMutex<Agent>>,
    idle_since: Instant,
    autonomy: &TeamAutonomyConfig,
) -> Result<AutonomyState, ()> {
    match manager.take_shutdown_signal(team_dir, teammate_name) {
        Ok(true) => {
            let _ =
                manager.update_member_status(team_dir, teammate_name, TeamMemberStatus::Shutdown);
            return Ok(AutonomyState::Shutdown);
        }
        Ok(false) => {}
        Err(error) => {
            let _ = mark_failed(manager, team_dir, teammate_name, error);
            return Err(());
        }
    }

    let claimed = {
        let mut guard = agent.lock().await;
        match guard.try_claim_ready_task() {
            Ok(task) => task,
            Err(error) => {
                let _ = mark_failed(manager, team_dir, teammate_name, error);
                return Err(());
            }
        }
    };
    if let Some(task) = claimed {
        let task_body = if task.description.trim().is_empty() {
            format!("Task #{}: {}", task.id, task.subject)
        } else {
            format!(
                "Task #{}: {}\nDescription: {}",
                task.id, task.subject, task.description
            )
        };
        return Ok(AutonomyState::Claimed(format!(
            "<auto-claimed>{task_body}</auto-claimed>\n<reminder>Update your task status. Mark it in_progress when you start and completed when you finish.</reminder>"
        )));
    }

    if idle_since.elapsed() >= autonomy.idle_timeout {
        let _ = manager.update_member_status(team_dir, teammate_name, TeamMemberStatus::Shutdown);
        return Ok(AutonomyState::Shutdown);
    }

    Ok(AutonomyState::ContinueIdle)
}

async fn execute_prompt(
    manager: &TeamManager,
    team_dir: &Path,
    teammate_name: &str,
    agent: &Arc<AsyncMutex<Agent>>,
    prompt: String,
) -> Result<(), ()> {
    let _ = manager.update_member_status(team_dir, teammate_name, TeamMemberStatus::Working);
    let result = {
        let mut guard = agent.lock().await;
        guard.send(vec![ContentBlock::Text { text: prompt }]).await
    };

    match result {
        Ok(_) | Err(RuntimeError::EmptyAssistantResponse) => Ok(()),
        Err(error) => {
            let _ = mark_failed(manager, team_dir, teammate_name, error);
            Err(())
        }
    }
}

fn mark_failed(
    manager: &TeamManager,
    team_dir: &Path,
    teammate_name: &str,
    error: RuntimeError,
) -> Result<(), RuntimeError> {
    manager.update_member_status(
        team_dir,
        teammate_name,
        TeamMemberStatus::Failed(error.to_string()),
    )
}
