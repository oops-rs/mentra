use std::{path::PathBuf, sync::Arc};

use tokio::{
    runtime::{Builder as RuntimeBuilder, Handle as TokioHandle, Runtime as TokioRuntime},
    sync::{Mutex as AsyncMutex, mpsc},
    task::{AbortHandle, JoinHandle},
};

use crate::{agent::Agent, error::RuntimeError, runtime::CancellationToken};

use super::{TeamManager, teammate_actor_loop};

#[derive(Clone)]
pub(crate) struct TeammateHost {
    backend: Arc<TeammateRuntimeBackend>,
}

enum TeammateRuntimeBackend {
    Current(TokioHandle),
    Owned(Arc<TokioRuntime>),
}

pub(crate) struct TeammateActorHandle {
    pub(crate) wake_tx: mpsc::UnboundedSender<()>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) abort: AbortHandle,
}

impl Drop for TeammateActorHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.abort.abort();
    }
}

impl TeammateHost {
    pub(crate) fn new() -> Result<Self, RuntimeError> {
        let backend = match TokioHandle::try_current() {
            Ok(handle) => TeammateRuntimeBackend::Current(handle),
            Err(_) => TeammateRuntimeBackend::Owned(Arc::new(
                RuntimeBuilder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        RuntimeError::Store(format!(
                            "Failed to create shared teammate runtime: {error}"
                        ))
                    })?,
            )),
        };
        Ok(Self {
            backend: Arc::new(backend),
        })
    }

    pub(crate) fn spawn_teammate(
        &self,
        manager: TeamManager,
        team_dir: PathBuf,
        teammate_name: String,
        agent: Arc<AsyncMutex<Agent>>,
    ) -> TeammateActorHandle {
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::default();
        let task = self.spawn_task(teammate_actor_loop(
            manager,
            team_dir,
            teammate_name,
            agent,
            wake_rx,
            cancellation.clone(),
        ));
        let abort = task.abort_handle();
        TeammateActorHandle {
            wake_tx,
            cancellation,
            abort,
        }
    }

    fn spawn_task(
        &self,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> JoinHandle<()> {
        match self.backend.as_ref() {
            TeammateRuntimeBackend::Current(handle) => handle.spawn(future),
            TeammateRuntimeBackend::Owned(runtime) => runtime.handle().spawn(future),
        }
    }
}
