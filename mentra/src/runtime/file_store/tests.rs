use std::{path::PathBuf, time::Duration};

use crate::{
    ContentBlock, Message,
    agent::{AgentConfig, AgentStatus},
    memory::MemoryStore,
    memory::journal::{AgentMemoryState, RunMemoryState},
    provider::ProviderId,
    runtime::{
        AgentStore, LeaseStore, PermissionRuleStore, RunStore, RuntimeError, TaskStore,
        store::{PersistedAgentRecord, now_nanos},
    },
    session::{
        PermissionRuleScope,
        permission::{RememberedRule, RuleKey},
    },
    transcript::{AgentTranscript, TranscriptItem},
};

use super::FileRuntimeStore;

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir()
        .join("mentra-file-store-tests")
        .join(format!("{label}-{}-{}", std::process::id(), now_nanos()))
}

fn agent_record(id: &str) -> PersistedAgentRecord {
    PersistedAgentRecord {
        id: id.to_string(),
        runtime_identifier: "test-runtime".to_string(),
        name: format!("agent-{id}"),
        model: "test-model".to_string(),
        provider_id: ProviderId::new("test"),
        config: AgentConfig::default(),
        hidden_tools: Default::default(),
        max_rounds: Some(7),
        teammate_identity: None,
        rounds_since_task: 2,
        idle_requested: false,
        status: AgentStatus::default(),
        subagents: Vec::new(),
    }
}

fn user_entry(text: &str) -> TranscriptItem {
    TranscriptItem::user_turn(Message::user(ContentBlock::text(text)))
}

fn transcript_of(texts: &[&str]) -> AgentTranscript {
    let mut transcript = AgentTranscript::default();
    for text in texts {
        transcript.push(user_entry(text));
    }
    transcript
}

fn memory_with(transcript: AgentTranscript) -> AgentMemoryState {
    AgentMemoryState {
        transcript,
        revision: 3,
        resumable_user_message: Some(Message::user(ContentBlock::text("resume me"))),
        ..AgentMemoryState::default()
    }
}

/// The comparison form of a memory state: its serde JSON, which covers every
/// field, entry id, and ordering the store must preserve.
fn state_value(state: &AgentMemoryState) -> serde_json::Value {
    serde_json::to_value(state).expect("serialize state")
}

fn record_value(record: &PersistedAgentRecord) -> serde_json::Value {
    serde_json::to_value(record).expect("serialize record")
}

fn transcript_line_count(store: &FileRuntimeStore, agent_id: &str) -> usize {
    let path = store.agent_dir(agent_id).join("transcript.jsonl");
    std::fs::read_to_string(path)
        .expect("read transcript log")
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

// -- AgentStore round trips --

#[test]
fn create_agent_then_load_round_trips_exactly() {
    let store = FileRuntimeStore::new(temp_root("round-trip"));
    let record = agent_record("agent-1");
    let memory = memory_with(transcript_of(&["hello", "world"]));

    store.create_agent(&record, &memory).expect("create agent");
    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");

    assert_eq!(record_value(&loaded.record), record_value(&record));
    assert_eq!(state_value(&loaded.memory), state_value(&memory));
    assert!(
        loaded.created_at.is_some(),
        "a durable store reports when it first wrote"
    );
    assert!(loaded.updated_at.is_some());
}

#[test]
fn a_branched_transcript_round_trips_with_its_archive_and_leaf() {
    let store = FileRuntimeStore::new(temp_root("branch"));
    let record = agent_record("agent-1");

    let mut transcript = transcript_of(&["0", "1", "2"]);
    let first = transcript.items()[0].id.clone();
    let abandoned_leaf = transcript.leaf().expect("a leaf").clone();
    store
        .create_agent(&record, &memory_with(transcript.clone()))
        .expect("create agent");

    // Branch away and continue elsewhere, saving each state as the runtime
    // would.
    transcript.branch_from(&first).expect("branch");
    transcript.push(user_entry("elsewhere"));
    let memory = memory_with(transcript.clone());
    store
        .save_agent_memory("agent-1", &memory)
        .expect("save branched state");

    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(loaded.memory.transcript, transcript);
    assert_eq!(loaded.memory.transcript.archived().len(), 2);

    // The reloaded tree can still return to the abandoned branch.
    let mut reloaded = loaded.memory.transcript;
    reloaded
        .branch_from(&abandoned_leaf)
        .expect("return to the abandoned branch");
    assert_eq!(reloaded.items().len(), 3);

    // The leaf file names the active leaf, newline-terminated, for tools
    // that read no JSON.
    let leaf = std::fs::read_to_string(store.agent_dir("agent-1").join("leaf")).expect("read leaf");
    assert_eq!(leaf, format!("{}\n", transcript.leaf().expect("a leaf")));
}

#[test]
fn a_run_baseline_round_trips_and_rolls_back_identically() {
    let store = FileRuntimeStore::new(temp_root("baseline"));
    let record = agent_record("agent-1");

    let baseline = transcript_of(&["before"]);
    let mut transcript = baseline.clone();
    transcript.push(user_entry("during the run"));
    let memory = AgentMemoryState {
        transcript,
        run: Some(RunMemoryState {
            run_id: "run-1".to_string(),
            baseline_transcript: baseline.clone(),
            assistant_committed: false,
        }),
        revision: 9,
        ..AgentMemoryState::default()
    };

    store.create_agent(&record, &memory).expect("create agent");
    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");

    assert_eq!(state_value(&loaded.memory), state_value(&memory));
    assert_eq!(
        loaded
            .memory
            .run
            .as_ref()
            .expect("run state survives")
            .baseline_transcript,
        baseline,
        "an interrupted run recovered from disk rolls back to the same baseline"
    );
}

#[test]
fn a_replaced_transcript_loads_as_replaced_while_the_log_keeps_history() {
    let store = FileRuntimeStore::new(temp_root("compaction"));
    let record = agent_record("agent-1");
    let original = memory_with(transcript_of(&["a", "b", "c"]));
    store
        .create_agent(&record, &original)
        .expect("create agent");

    // What compaction does: install a wholly new transcript.
    let replacement = memory_with(transcript_of(&["summary of a-c", "d"]));
    store
        .save_agent_memory("agent-1", &replacement)
        .expect("save replacement");

    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(state_value(&loaded.memory), state_value(&replacement));

    // The log is history: the superseded entries are still greppable.
    assert_eq!(transcript_line_count(&store, "agent-1"), 5);
}

#[test]
fn appends_do_not_duplicate_entries_across_reopen() {
    let root = temp_root("reopen");
    let mut transcript = transcript_of(&["one", "two"]);
    {
        let store = FileRuntimeStore::new(&root);
        store
            .create_agent(&agent_record("agent-1"), &memory_with(transcript.clone()))
            .expect("create agent");
    }

    // A fresh process appends the next turn.
    let store = FileRuntimeStore::new(&root);
    transcript.push(user_entry("three"));
    store
        .save_agent_memory("agent-1", &memory_with(transcript.clone()))
        .expect("save third entry");

    assert_eq!(
        transcript_line_count(&store, "agent-1"),
        3,
        "already-logged entries must not be appended again"
    );
    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(loaded.memory.transcript, transcript);
}

// -- Crash shapes --

#[test]
fn a_truncated_final_line_is_skipped_and_the_next_append_gets_a_fresh_line() {
    let root = temp_root("truncated");
    let mut transcript = transcript_of(&["one", "two"]);
    let store = FileRuntimeStore::new(&root);
    store
        .create_agent(&agent_record("agent-1"), &memory_with(transcript.clone()))
        .expect("create agent");

    // A crash mid-append: half a line, no newline.
    let log_path = store.agent_dir("agent-1").join("transcript.jsonl");
    let mut contents = std::fs::read(&log_path).expect("read log");
    contents.extend_from_slice(br#"{"schema":1,"id":"entry-trunc"#);
    std::fs::write(&log_path, contents).expect("write truncated log");

    // A fresh process reads past the damage and appends on a fresh line.
    let reopened = FileRuntimeStore::new(&root);
    let loaded = reopened
        .load_agent("agent-1")
        .expect("load agent despite the truncated tail")
        .expect("agent present");
    assert_eq!(loaded.memory.transcript, transcript);

    transcript.push(user_entry("three"));
    reopened
        .save_agent_memory("agent-1", &memory_with(transcript.clone()))
        .expect("append after damage");

    let reloaded = reopened
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(reloaded.memory.transcript, transcript);
    // Every surviving line parses: the damaged tail is gone, not entombed.
    assert_eq!(transcript_line_count(&reopened, "agent-1"), 3);
    for line in std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter(|line| !line.is_empty())
    {
        serde_json::from_str::<serde_json::Value>(line).expect("every kept line parses");
    }
}

#[test]
fn leftover_temp_files_and_stray_entries_are_ignored() {
    let store = FileRuntimeStore::new(temp_root("strays"));
    let record = agent_record("agent-1");
    store
        .create_agent(&record, &memory_with(transcript_of(&["hello"])))
        .expect("create agent");

    // The shapes a crash between write and rename can leave behind.
    let agent_dir = store.agent_dir("agent-1");
    std::fs::write(agent_dir.join(".agent.json.tmp-999-1"), b"{ partial").expect("plant temp file");
    std::fs::write(store.agents_dir().join("not-a-directory"), b"stray").expect("plant stray file");
    std::fs::create_dir_all(store.agents_dir().join("half-created"))
        .expect("plant record-less directory");

    let agents = store.list_agents().expect("list agents");
    assert_eq!(agents.len(), 1, "only the real agent is listed");
    assert_eq!(agents[0].record.id, "agent-1");
    let loaded = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(record_value(&loaded.record), record_value(&record));
}

// -- Record lifecycle parity with the other stores --

#[test]
fn save_agent_record_without_memory_errors_on_load() {
    let store = FileRuntimeStore::new(temp_root("no-memory"));
    store
        .save_agent_record(&agent_record("agent-1"))
        .expect("save record");

    let error = store
        .load_agent("agent-1")
        .expect_err("memory is missing until it is saved");
    assert!(matches!(error, RuntimeError::Store(_)));
}

#[test]
fn resaving_a_record_moves_updated_at_and_keeps_created_at() {
    let store = FileRuntimeStore::new(temp_root("timestamps"));
    let mut record = agent_record("agent-1");
    store
        .create_agent(&record, &AgentMemoryState::default())
        .expect("create agent");
    let first = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");

    record.name = "renamed".to_string();
    store
        .save_agent_record(&record)
        .expect("save renamed record");
    let second = store
        .load_agent("agent-1")
        .expect("load agent")
        .expect("agent present");

    assert_eq!(second.record.name, "renamed");
    assert_eq!(
        second.created_at, first.created_at,
        "the first write settles created_at for good"
    );
    assert!(second.updated_at >= first.updated_at);
}

#[test]
fn delete_agent_removes_everything_and_deleting_absent_succeeds() {
    let store = FileRuntimeStore::new(temp_root("delete"));
    store
        .create_agent(
            &agent_record("agent-1"),
            &memory_with(transcript_of(&["x"])),
        )
        .expect("create agent");

    store.delete_agent("agent-1").expect("delete agent");

    assert!(store.load_agent("agent-1").expect("load agent").is_none());
    assert!(store.list_agents().expect("list agents").is_empty());
    assert!(!store.agent_dir("agent-1").exists());
    store
        .delete_agent("agent-1")
        .expect("deleting an absent agent succeeds: the goal is that it be gone");
}

#[test]
fn a_crashed_delete_leaves_the_shape_readers_already_ignore() {
    let store = FileRuntimeStore::new(temp_root("partial-delete"));
    let memory = AgentMemoryState::default();
    store
        .create_agent(
            &agent_record("agent-1"),
            &memory_with(transcript_of(&["x"])),
        )
        .expect("create agent-1");
    store
        .create_agent(&agent_record("agent-2"), &memory)
        .expect("create agent-2");

    // The state a delete interrupted between its two steps leaves behind:
    // agent.json gone, the rest of the directory still there.
    std::fs::remove_file(store.agent_dir("agent-1").join("agent.json"))
        .expect("simulate the crash point");

    assert!(
        store
            .load_agent("agent-1")
            .expect("load half-deleted agent")
            .is_none(),
        "a record-less directory is not an agent"
    );
    let ids: Vec<_> = store
        .list_agents()
        .expect("listing survives a half-deleted agent")
        .into_iter()
        .map(|loaded| loaded.record.id)
        .collect();
    assert_eq!(ids, vec!["agent-2".to_string()]);

    // Deleting the remains finishes the job.
    store.delete_agent("agent-1").expect("finish the delete");
    assert!(!store.agent_dir("agent-1").exists());
}

#[test]
fn list_agents_orders_by_creation_and_filters_by_runtime() {
    let store = FileRuntimeStore::new(temp_root("list"));
    let memory = AgentMemoryState::default();
    let mut second = agent_record("agent-b");
    second.runtime_identifier = "other-runtime".to_string();
    store
        .create_agent(&agent_record("agent-a"), &memory)
        .expect("create first");
    store.create_agent(&second, &memory).expect("create second");

    let ids: Vec<_> = store
        .list_agents()
        .expect("list agents")
        .into_iter()
        .map(|loaded| loaded.record.id)
        .collect();
    assert_eq!(ids, vec!["agent-a".to_string(), "agent-b".to_string()]);

    let by_runtime: Vec<_> = store
        .list_agents_by_runtime("other-runtime")
        .expect("list by runtime")
        .into_iter()
        .map(|loaded| loaded.record.id)
        .collect();
    assert_eq!(by_runtime, vec!["agent-b".to_string()]);
}

#[test]
fn an_id_needing_encoding_still_round_trips() {
    let store = FileRuntimeStore::new(temp_root("encoding"));
    let record = agent_record("agent/one two");
    store
        .create_agent(&record, &AgentMemoryState::default())
        .expect("create agent");

    let loaded = store
        .load_agent("agent/one two")
        .expect("load agent")
        .expect("agent present");
    assert_eq!(loaded.record.id, "agent/one two");
    assert_eq!(store.list_agents().expect("list agents").len(), 1);
}

#[test]
fn a_file_from_a_newer_schema_is_refused_not_misread() {
    let store = FileRuntimeStore::new(temp_root("schema"));
    store
        .create_agent(&agent_record("agent-1"), &AgentMemoryState::default())
        .expect("create agent");

    let path = store.agent_dir("agent-1").join("agent.json");
    let rewritten = std::fs::read_to_string(&path)
        .expect("read agent.json")
        .replacen("\"schema\": 1", "\"schema\": 99", 1);
    std::fs::write(&path, rewritten).expect("write future schema");

    let error = store
        .load_agent("agent-1")
        .expect_err("a future schema must be refused");
    assert!(error.to_string().contains("schema"), "{error}");
}

// -- Permission rules --

fn rule(tool_name: &str, allow: bool, scope: PermissionRuleScope) -> RememberedRule {
    RememberedRule {
        key: RuleKey {
            tool_name: tool_name.to_string(),
            pattern: None,
        },
        allow,
        scope,
        reason: None,
    }
}

#[test]
fn permission_rules_round_trip_across_scopes_and_reopen() {
    let root = temp_root("rules");
    {
        let store = FileRuntimeStore::new(&root);
        store
            .save_rules(
                "session-1",
                Some("proj-x"),
                &[
                    rule("shell", true, PermissionRuleScope::Session),
                    rule("read", false, PermissionRuleScope::Project),
                    rule("write", false, PermissionRuleScope::Global),
                ],
            )
            .expect("save rules");
    }

    // A restarted process sees the same rules.
    let store = FileRuntimeStore::new(&root);
    let with_project = store
        .load_rules("session-1", Some("proj-x"))
        .expect("load with project");
    assert_eq!(with_project.len(), 3);

    let other_session = store
        .load_rules("session-2", None)
        .expect("load other session");
    assert_eq!(
        other_session.len(),
        1,
        "only the global rule crosses sessions"
    );
    assert_eq!(other_session[0].key.tool_name, "write");

    store.clear_rules("session-1").expect("clear session");
    assert!(
        store
            .load_rules("session-1", Some("proj-x"))
            .expect("load after clear")
            .is_empty()
    );
}

#[test]
fn repeated_saves_keep_one_copy_of_each_rule() {
    let store = FileRuntimeStore::new(temp_root("rules-dedup"));
    let remembered = [
        rule("shell", true, PermissionRuleScope::Session),
        rule("read", false, PermissionRuleScope::Project),
        rule("write", false, PermissionRuleScope::Global),
    ];

    // Every save carries the session's whole remembered set, project and
    // global rules included — this used to duplicate the non-session rows
    // once per save.
    for _ in 0..4 {
        store
            .save_rules("session-1", Some("proj-x"), &remembered)
            .expect("save rules");
    }

    let loaded = store
        .load_rules("session-1", Some("proj-x"))
        .expect("load rules");
    assert_eq!(loaded.len(), 3, "each rule loads exactly once: {loaded:?}");

    let on_disk: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(store.rules_path()).expect("read rules.json"),
    )
    .expect("parse rules.json");
    assert_eq!(
        on_disk["rules"].as_array().expect("rules array").len(),
        3,
        "the file holds one copy of each rule"
    );
}

#[test]
fn saving_rules_replaces_only_that_sessions_session_scope() {
    let store = FileRuntimeStore::new(temp_root("rules-replace"));
    store
        .save_rules(
            "session-1",
            Some("proj-x"),
            &[
                rule("shell", true, PermissionRuleScope::Session),
                rule("read", false, PermissionRuleScope::Project),
            ],
        )
        .expect("save initial");

    store
        .save_rules(
            "session-1",
            Some("proj-x"),
            &[rule("write", false, PermissionRuleScope::Session)],
        )
        .expect("save replacement");

    let loaded = store
        .load_rules("session-1", Some("proj-x"))
        .expect("load rules");
    let mut tools: Vec<_> = loaded
        .iter()
        .map(|rule| rule.key.tool_name.as_str())
        .collect();
    tools.sort_unstable();
    assert_eq!(
        tools,
        vec!["read", "write"],
        "the project rule survives a session-scope replacement"
    );
}

// -- Runs --

#[test]
fn run_lifecycle_is_an_append_only_event_log() {
    let store = FileRuntimeStore::new(temp_root("runs"));
    let run_id = store.start_run("agent-1").expect("start run");
    store.finish_run(&run_id).expect("finish run");
    store
        .fail_run("run-from-a-previous-process", "interrupted")
        .expect("a transition for an unseen run id is still recorded");

    let contents = std::fs::read_to_string(store.runs_path()).expect("read runs.jsonl");
    let events: Vec<serde_json::Value> = contents
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("event line parses"))
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["run_id"], serde_json::json!(run_id));
    assert_eq!(events[0]["state"], serde_json::json!("running"));
    assert_eq!(events[0]["agent_id"], serde_json::json!("agent-1"));
    assert_eq!(events[1]["state"], serde_json::json!("finished"));
    assert_eq!(events[2]["state"], serde_json::json!("failed"));
    assert_eq!(events[2]["error"], serde_json::json!("interrupted"));
}

// -- The deliberately-not-persisted subsystems --

#[test]
fn long_term_memory_is_refused_with_the_fix_named() {
    let store = FileRuntimeStore::new(temp_root("memory"));
    let error = store
        .upsert_records(&[])
        .expect_err("the file store refuses long-term memory");
    assert!(error.to_string().contains("store-sqlite"), "{error}");
    assert!(
        store.search_records("agent-1", "anything", 5).is_err(),
        "search is refused the same way"
    );
}

#[test]
fn leases_and_tasks_work_in_process() {
    let store = FileRuntimeStore::new(temp_root("volatile"));

    assert!(
        store
            .acquire_lease("agent:x", "owner-1", Duration::from_secs(60))
            .expect("acquire")
    );
    assert!(
        !store
            .acquire_lease("agent:x", "owner-2", Duration::from_secs(60))
            .expect("second acquire"),
        "a clone-shared lease excludes a second owner in this process"
    );
    store.release_lease("agent:x", "owner-1").expect("release");

    let namespace = std::path::Path::new("/tasks/example");
    store
        .replace_tasks(namespace, &[])
        .expect("task board is usable");
    assert!(store.load_tasks(namespace).expect("load tasks").is_empty());
}

#[test]
fn prepare_recovery_creates_the_store_home() {
    let root = temp_root("recovery");
    let store = FileRuntimeStore::new(&root);
    assert!(!root.exists(), "construction alone touches nothing");

    store.prepare_recovery().expect("prepare recovery");
    assert!(store.agents_dir().is_dir());
}
