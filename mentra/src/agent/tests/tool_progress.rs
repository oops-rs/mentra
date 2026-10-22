use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    BuiltinProvider, ContentBlock, Role, Runtime,
    agent::AgentEvent,
    provider::{ContentBlockDelta, ContentBlockStart, ProviderEvent},
    runtime::RunOptions,
    tool::{
        ParallelToolContext, ToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor,
        ToolProgress, ToolResult, ToolSpec,
    },
};

use super::support::{ScriptedProvider, controlled_stream, model_info, tool_use_stream};

struct BackgroundTool {
    category: ToolExecutionCategory,
    handles: mpsc::UnboundedSender<ToolProgress>,
}

impl ToolDefinition for BackgroundTool {
    fn descriptor(&self) -> ToolSpec {
        ToolSpec::builder("background")
            .execution_category(self.category)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for BackgroundTool {
    fn execution_category(&self, _input: &Value) -> ToolExecutionCategory {
        self.category
    }

    async fn execute_mut(&self, ctx: ToolContext<'_>, _input: Value) -> ToolResult {
        ctx.emit_progress("starting".to_string());
        self.handles.send(ctx.progress()).expect("handle receiver");
        Ok("started".to_string())
    }

    async fn execute(&self, ctx: ParallelToolContext, _input: Value) -> ToolResult {
        ctx.emit_progress("starting".to_string());
        self.handles.send(ctx.progress()).expect("handle receiver");
        Ok("started".to_string())
    }
}

async fn background_progress(category: ToolExecutionCategory) {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let (stream, tx) = controlled_stream();
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "call-1", "background", "{}"),
            stream,
        ],
    );
    let provider_handle = provider.clone();
    let (handles, mut handle_rx) = mpsc::unbounded_channel();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(BackgroundTool { category, handles })
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("parent", model.clone()).expect("parent");
    let other_agent = runtime.spawn("other", model.clone()).expect("other agent");
    let mut other_events = other_agent.subscribe_events();
    let mut events = agent.subscribe_events();
    let tapped = Arc::new(Mutex::new(Vec::new()));
    let tapped_events = Arc::clone(&tapped);
    let _tap = agent.register_event_tap(move |event| {
        tapped_events
            .lock()
            .expect("tapped events")
            .push(event.clone());
    });
    let expected = |progress: &str| AgentEvent::ToolExecutionProgress {
        id: "call-1".to_string(),
        name: "background".to_string(),
        progress: progress.to_string(),
    };

    let drive = async {
        // The next request proves the tool returned. The runner cannot reach
        // another round boundary until we complete this controlled stream.
        while provider_handle.recorded_requests().await.len() < 2 {
            tokio::task::yield_now().await;
        }
        let progress = handle_rx.recv().await.expect("owned progress handle");
        let mut before = Vec::new();
        while let Ok(event) = events.try_recv() {
            before.push(event);
        }
        assert!(before.contains(&expected("starting")));
        assert!(
            before
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolExecutionFinished { .. }))
        );

        // Clones move into a 'static task without keeping a context alive.
        let background = progress.clone();
        tokio::spawn(async move {
            background.emit("halfway");
            background.emit("almost done");
        })
        .await
        .expect("background task");
        for message in ["halfway", "almost done"] {
            assert_eq!(
                events.try_recv().expect("immediate progress"),
                expected(message)
            );
        }
        assert!(events.try_recv().is_err(), "no duplicate events");
        let observed = tapped.lock().expect("tapped events").clone();
        assert_eq!(
            &observed[observed.len() - 2..],
            &[expected("halfway"), expected("almost done")]
        );
        assert!(
            other_events.try_recv().is_err(),
            "progress stays on its parent"
        );

        for event in [
            ProviderEvent::MessageStarted {
                id: "final".to_string(),
                model: model.id.clone(),
                role: Role::Assistant,
            },
            ProviderEvent::ContentBlockStarted {
                index: 0,
                kind: ContentBlockStart::Text,
            },
            ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("done".to_string()),
            },
            ProviderEvent::ContentBlockStopped { index: 0 },
            ProviderEvent::MessageStopped,
        ] {
            tx.send(Ok(event)).expect("provider stream");
        }
        drop(tx);
        progress
    };
    let (result, progress) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            agent.run(vec![ContentBlock::text("go")], RunOptions::default()),
            drive
        )
    })
    .await
    .expect("run and progress complete without waiting for a round boundary");
    assert_eq!(result.expect("run succeeds").text(), "done");
    assert_eq!(
        provider_handle.recorded_requests().await.len(),
        2,
        "progress does not keep the run going"
    );
    assert!(
        !agent.has_deferred(),
        "progress is never queued as deferred context"
    );
    assert!(
        !agent.history().iter().any(|message| {
            let text = message.text();
            text.contains("halfway") || text.contains("almost done")
        }),
        "progress is not model context"
    );

    // Background work can also outlive the run and still reach its observers.
    while events.try_recv().is_ok() {}
    progress.emit("finished in background");
    assert_eq!(
        events.try_recv().expect("late progress"),
        expected("finished in background")
    );
}

#[tokio::test]
async fn mutable_tool_progress_outlives_its_call() {
    background_progress(ToolExecutionCategory::ExclusiveLocalMutation).await;
}

#[tokio::test]
async fn parallel_tool_progress_outlives_its_call() {
    background_progress(ToolExecutionCategory::ReadOnlyParallel).await;
}
