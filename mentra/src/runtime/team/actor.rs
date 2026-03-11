use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::{ContentBlock, runtime::error::RuntimeError};

use super::{TeamManager, TeamMemberStatus};
use crate::runtime::Agent;

const TEAM_WAKE_PROMPT: &str = "Process any new team inbox messages and continue your work.";

pub(crate) async fn teammate_actor_loop(
    manager: TeamManager,
    team_dir: PathBuf,
    teammate_name: String,
    agent: Arc<AsyncMutex<Agent>>,
    mut wake_rx: mpsc::UnboundedReceiver<()>,
) {
    let mut should_shutdown = false;
    while wake_rx.recv().await.is_some() {
        loop {
            match manager.has_pending_messages(&team_dir, &teammate_name) {
                Ok(true) => {}
                Ok(false) => {
                    match manager.take_shutdown_signal(&team_dir, &teammate_name) {
                        Ok(true) => {
                            should_shutdown = true;
                            break;
                        }
                        Ok(false) => {
                            let _ = manager.update_member_status(
                                &team_dir,
                                &teammate_name,
                                TeamMemberStatus::Idle,
                            );
                        }
                        Err(error) => {
                            let _ = mark_failed(&manager, &team_dir, &teammate_name, error);
                            break;
                        }
                    }
                    break;
                }
                Err(error) => {
                    let _ = mark_failed(&manager, &team_dir, &teammate_name, error);
                    break;
                }
            }

            let _ =
                manager.update_member_status(&team_dir, &teammate_name, TeamMemberStatus::Working);
            let result = {
                let mut guard = agent.lock().await;
                guard
                    .send(vec![ContentBlock::Text {
                        text: TEAM_WAKE_PROMPT.to_string(),
                    }])
                    .await
            };

            if let Err(error) = result {
                let _ = mark_failed(&manager, &team_dir, &teammate_name, error);
                break;
            }
        }

        if should_shutdown {
            break;
        }
    }

    let _ = manager.update_member_status(&team_dir, &teammate_name, TeamMemberStatus::Shutdown);
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
        TeamMemberStatus::Failed(format!("{error:?}")),
    )
}
