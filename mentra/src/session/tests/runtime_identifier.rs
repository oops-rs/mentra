use crate::{
    ContentBlock, Runtime,
    runtime::{FileRuntimeStore, RuntimeStore, SessionResumeOptions, VolatileRuntimeStore},
    session::SessionId,
    test::MockRuntime,
};

fn assert_listing(runtime: &Runtime, identifier: &str, expected: &[&str]) {
    let agents = runtime.list_persisted_agents(identifier).unwrap();
    assert_eq!(
        agents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        expected,
        "unexpected agents under {identifier}"
    );
}

async fn assert_resume_identifier_override(store: impl RuntimeStore + Clone + 'static) {
    // Older hosts tagged every conversation as "default". A new shared
    // runtime must be able to adopt just the conversation its workspace uses.
    let legacy = MockRuntime::builder()
        .runtime_identifier("default")
        .with_store(store.clone())
        .build()
        .unwrap();
    let session = legacy
        .runtime()
        .create_session("legacy", legacy.model())
        .unwrap();
    let sibling = legacy
        .runtime()
        .create_session("sibling", legacy.model())
        .unwrap();
    let agent_id = session.agent_id().to_string();
    let sibling_id = sibling.agent_id().to_string();
    drop(session);
    drop(sibling);
    drop(legacy);

    let mock = MockRuntime::builder()
        .runtime_identifier("shared-runtime")
        .with_store(store.clone())
        .text("adopted")
        .build()
        .unwrap();
    let mut resumed = mock
        .runtime()
        .resume_session_with_options(
            &agent_id,
            SessionResumeOptions {
                runtime_identifier: Some("workspace-a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(resumed.agent_id(), agent_id);
    // A normal resume alone does not write a new snapshot.
    assert_listing(mock.runtime(), "workspace-a", &[]);
    resumed
        .append_turn(vec![ContentBlock::text("continue")])
        .await
        .unwrap();
    assert_listing(mock.runtime(), "workspace-a", &[&agent_id]);
    assert_listing(mock.runtime(), "default", &[&sibling_id]);
    assert_listing(mock.runtime(), "shared-runtime", &[]);

    let fresh = mock
        .runtime()
        .create_session("fresh", mock.model())
        .unwrap();
    assert_listing(mock.runtime(), "shared-runtime", &[fresh.agent_id()]);
    drop(fresh);
    drop(resumed);
    drop(mock);

    // The corrected tag survives another runtime and both default resume
    // wrappers. An unrelated legacy session still retains its own tag.
    let reopened = MockRuntime::builder()
        .runtime_identifier("another-runtime")
        .with_store(store)
        .text("resumed again")
        .text("resumed with project")
        .text("sibling resumed")
        .build()
        .unwrap();
    let mut resumed = reopened.runtime().resume_session(&agent_id).unwrap();
    resumed
        .append_turn(vec![ContentBlock::text("continue again")])
        .await
        .unwrap();
    drop(resumed);
    let mut resumed = reopened
        .runtime()
        .resume_session_with_project(&agent_id, Some("current-project".to_owned()))
        .unwrap();
    resumed
        .append_turn(vec![ContentBlock::text("continue with project")])
        .await
        .unwrap();
    drop(resumed);
    let mut sibling = reopened.runtime().resume_session(&sibling_id).unwrap();
    sibling
        .append_turn(vec![ContentBlock::text("continue sibling")])
        .await
        .unwrap();
    assert_listing(reopened.runtime(), "workspace-a", &[&agent_id]);
    assert_listing(reopened.runtime(), "default", &[&sibling_id]);
    assert_listing(reopened.runtime(), "another-runtime", &[]);
}

#[tokio::test]
async fn resume_identifier_override_volatile() {
    assert_resume_identifier_override(VolatileRuntimeStore::new()).await;
}

#[tokio::test]
async fn resume_identifier_override_file() {
    let directory = std::env::temp_dir().join(format!("mentra-resume-{}", SessionId::new()));
    assert_resume_identifier_override(FileRuntimeStore::new(&directory)).await;
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn resume_identifier_override_sqlite() {
    let directory = std::env::temp_dir().join(format!("mentra-resume-{}", SessionId::new()));
    assert_resume_identifier_override(crate::runtime::SqliteRuntimeStore::new(
        directory.join("runtime.db"),
    ))
    .await;
    std::fs::remove_dir_all(directory).unwrap();
}
