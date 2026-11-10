//! A skill profile decides which registered skills one agent's model is told
//! about and may load, beside the tool profile that decides its tools.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    BuiltinProvider, ContentBlock, Message,
    agent::{AgentConfig, SkillProfile, ToolProfile},
    provider::Request,
    runtime::Runtime,
};

use super::support::{
    PersistentStore, ScriptedProvider, StaticTool, model_info, text_stream, tool_use_stream,
};

const LIST_ASK_ONLY: &str = "Skills available:\n  - ask: Answers questions\nUse the load_skill tool only when one of these skills is relevant to the task.";
const LIST_BUILD_ONLY: &str = "Skills available:\n  - build: Runs builds\nUse the load_skill tool only when one of these skills is relevant to the task.";

#[tokio::test]
async fn a_profile_narrows_the_list_and_refuses_what_it_leaves_out() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "closed", "load_skill", r#"{"name":"build"}"#),
            tool_use_stream(&model.id, "open", "load_skill", r#"{"name":"ask"}"#),
            text_stream(&model.id, "done"),
        ],
    );
    let requests = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_skills_dir(two_skills("narrow"))
        .expect("register skills")
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("Base".to_string()),
                skill_profile: SkillProfile::only(["ask"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(text("hello")).await.unwrap();

    assert_eq!(
        agent.history()[2],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "closed".to_string(),
            content: "Unknown skill 'build'".into(),
            is_error: true,
        }),
        "a skill outside the profile does not exist for this agent"
    );
    assert_eq!(
        agent.history()[4],
        Message::user(ContentBlock::ToolResult {
            tool_use_id: "open".to_string(),
            content: "<skill name=\"ask\">\nAsk body.\n</skill>".into(),
            is_error: false,
        })
    );
    let requests = requests.recorded_requests().await;
    assert!(tool_names(&requests[0]).contains("load_skill"));
    assert_eq!(
        requests[0].system.as_deref(),
        Some(format!("Base\n\n{LIST_ASK_ONLY}").as_str())
    );
    // A host's own readers are not the model's.
    assert_eq!(runtime.skills().len(), 2);
    assert!(runtime.skill_body("build").is_ok());
}

#[tokio::test]
async fn an_agent_allowed_no_skill_is_offered_neither_a_list_nor_load_skill() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![text_stream(&model.id, "done")],
    );
    let requests = provider.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_skills_dir(two_skills("none"))
        .expect("register skills")
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                system: Some("Base".to_string()),
                skill_profile: SkillProfile::only(Vec::<String>::new()),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(text("hello")).await.unwrap();

    let requests = requests.recorded_requests().await;
    assert!(!tool_names(&requests[0]).contains("load_skill"));
    assert_eq!(requests[0].system.as_deref(), Some("Base"));
}

#[tokio::test]
async fn a_subagent_starts_from_its_parents_skill_profile() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![model.clone()],
        vec![
            tool_use_stream(&model.id, "delegate", "task", r#"{"prompt":"look"}"#),
            tool_use_stream(&model.id, "child", "load_skill", r#"{"name":"build"}"#),
            text_stream(&model.id, "child summary"),
            text_stream(&model.id, "parent done"),
        ],
    );
    let requests = provider.clone();
    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_skills_dir(two_skills("child"))
        .expect("register skills")
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config(
            "agent",
            model,
            AgentConfig {
                skill_profile: SkillProfile::only(["ask"]),
                ..Default::default()
            },
        )
        .unwrap();

    agent.send(text("delegate")).await.unwrap();

    let requests = requests.recorded_requests().await;
    let child_system = requests[1].system.as_deref().expect("child system prompt");
    assert!(child_system.ends_with(LIST_ASK_ONLY), "{child_system}");
    let refused = requests[2]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "child"
            )
        });
    assert!(refused, "the child cannot load what its parent cannot");
}

#[tokio::test]
async fn a_resumed_session_takes_new_profiles_and_a_later_resume_keeps_them() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let store = temp_store("resume");
    let skills = two_skills("resume");
    let runtime = |scripts| {
        let provider =
            ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], scripts);
        let runtime = Runtime::empty_builder()
            .with_store(store.clone())
            .with_provider_instance(provider.clone())
            .with_skills_dir(&skills)
            .expect("register skills")
            .build()
            .expect("build runtime");
        runtime.register_tool(StaticTool::success("build_tool", "built"));
        runtime.register_tool(StaticTool::success("ask_tool", "answered"));
        (runtime, provider)
    };

    // The first run is built for a caller allowed only `build`.
    let agent_id = {
        let (runtime, provider) = runtime(vec![text_stream(&model.id, "first")]);
        let mut session = runtime
            .create_session_with_config(
                "conversation",
                model.clone(),
                AgentConfig {
                    system: Some("Base".to_string()),
                    tool_profile: ToolProfile::only(["build_tool", "load_skill"]),
                    skill_profile: SkillProfile::only(["build"]),
                    ..Default::default()
                },
            )
            .expect("create session");
        session.append_turn(text("one")).await.expect("first turn");
        let requests = provider.recorded_requests().await;
        assert_eq!(
            tool_names(&requests[0]),
            HashSet::from(["build_tool".to_string(), "load_skill".to_string()])
        );
        assert_eq!(
            requests[0].system.as_deref(),
            Some(format!("Base\n\n{LIST_BUILD_ONLY}").as_str())
        );
        session.agent_id().to_string()
    };

    // The conversation resumes for a caller allowed only `ask`: a resume
    // alone still carries the first run's profiles, and restating them
    // replaces both.
    {
        let (runtime, provider) = runtime(vec![
            tool_use_stream(&model.id, "stale", "build_tool", "{}"),
            tool_use_stream(&model.id, "closed", "load_skill", r#"{"name":"build"}"#),
            text_stream(&model.id, "second"),
        ]);
        let mut session = runtime.resume_session(&agent_id).expect("resume");
        assert_eq!(
            session.config().skill_profile,
            SkillProfile::only(["build"]),
            "a resume by itself keeps what the first run stored"
        );
        session
            .set_profiles(
                ToolProfile::only(["ask_tool", "load_skill"]),
                SkillProfile::only(["ask"]),
            )
            .expect("restate the profiles");
        session.append_turn(text("two")).await.expect("second turn");

        let requests = provider.recorded_requests().await;
        assert_eq!(
            tool_names(&requests[0]),
            HashSet::from(["ask_tool".to_string(), "load_skill".to_string()])
        );
        assert_eq!(
            requests[0].system.as_deref(),
            Some(format!("Base\n\n{LIST_ASK_ONLY}").as_str())
        );
        // The model remembers the first run's tool and skill and asks for
        // them by name; neither answers.
        let results = tool_results(&requests[2]);
        assert!(results.iter().all(|(_, is_error)| *is_error), "{results:?}");
        assert_eq!(results.len(), 2);
    }

    // What was restated is what is stored.
    let (runtime, _) = runtime(Vec::new());
    let session = runtime.resume_session(&agent_id).expect("resume again");
    assert_eq!(session.config().skill_profile, SkillProfile::only(["ask"]));
    assert_eq!(
        session.config().tool_profile,
        ToolProfile::only(["ask_tool", "load_skill"])
    );
}

#[test]
fn a_config_stored_before_skill_profiles_allows_every_skill() {
    let mut stored = serde_json::to_value(AgentConfig::default()).expect("serialize config");
    stored
        .as_object_mut()
        .expect("config is an object")
        .remove("skill_profile");

    let config: AgentConfig = serde_json::from_value(stored).expect("deserialize legacy config");

    assert_eq!(config.skill_profile, SkillProfile::all());
    assert!(config.skill_profile.allows("anything"));
    assert!(!config.skill_profile.allows_none());
}

fn text(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text.to_string(),
    }]
}

fn tool_names(request: &Request<'_>) -> HashSet<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// Every tool result in `request`'s messages: its call id, and whether it
/// is an error.
fn tool_results(request: &Request<'_>) -> Vec<(String, bool)> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => Some((tool_use_id.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn temp_path(label: &str) -> PathBuf {
    let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!("mentra-skill-profile-{label}-{timestamp}-{unique}"))
}

fn temp_store(label: &str) -> PersistentStore {
    PersistentStore::new(temp_path(label).with_extension("store"))
}

/// A skills root holding `build` and `ask`.
fn two_skills(label: &str) -> PathBuf {
    let root = temp_path(label);
    write_skill(
        &root,
        "build",
        "---\nname: build\ndescription: Runs builds\n---\nBuild body.\n",
    );
    write_skill(
        &root,
        "ask",
        "---\nname: ask\ndescription: Answers questions\n---\nAsk body.\n",
    );
    root
}

fn write_skill(root: &Path, name: &str, content: &str) {
    let skill_dir = root.join(name);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(skill_dir.join("SKILL.md"), content).expect("write skill");
}
