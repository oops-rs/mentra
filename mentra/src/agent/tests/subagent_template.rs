//! `DisposableSubagentTemplate`'s builder: `with_tool_profile`, `with_model`,
//! and `with_system` each override one field of the parent clone, and
//! `spawn`/`Agent::spawn_subagent_from` must apply the same depth-guard,
//! hidden-tool, and subagent-system-prompt treatment regardless of which
//! fields were overridden.

use crate::{
    BuiltinProvider, ModelInfo,
    agent::ToolProfile,
    runtime::{Runtime, RuntimeError, RuntimeIntrinsicTool},
};

use super::support::{ScriptedProvider, model_info, text_stream};

#[tokio::test]
async fn narrowed_tool_profile_restricts_the_childs_roster_and_keeps_task_hidden() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let parent = runtime.spawn("parent", model).expect("spawn parent");

    // `task` is deliberately included in the allowlist to prove the
    // spawn-level hidden-tools set still wins over an overridden profile that
    // would otherwise allow it.
    let template = parent
        .disposable_subagent_template()
        .with_tool_profile(ToolProfile::only(["read", "grep", "task"]));
    let child = parent
        .spawn_subagent_from(template)
        .expect("spawn from an overridden template");

    assert!(
        child.can_use_tool("read"),
        "allowlisted tool must be usable"
    );
    assert!(
        child.can_use_tool("grep"),
        "allowlisted tool must be usable"
    );
    assert!(
        !child.can_use_tool("write"),
        "a tool outside the narrowed allowlist must be blocked"
    );
    assert!(
        !child.can_use_tool(&RuntimeIntrinsicTool::Task.to_string()),
        "task stays hidden from a subagent even when an overridden profile allows it"
    );
}

#[tokio::test]
async fn overridden_model_reaches_the_childs_config_and_routing() {
    let parent_model = model_info("parent-model", BuiltinProvider::Anthropic);
    let anthropic = ScriptedProvider::new(
        BuiltinProvider::Anthropic,
        vec![parent_model.clone()],
        vec![],
    );
    let anthropic_handle = anthropic.clone();

    let mut cheap_model = ModelInfo::new("cheap-model", BuiltinProvider::OpenAI);
    cheap_model.context_window = Some(4_096);
    let openai = ScriptedProvider::new(
        BuiltinProvider::OpenAI,
        vec![cheap_model.clone()],
        vec![text_stream(&cheap_model.id, "child done")],
    );
    let openai_handle = openai.clone();

    let runtime = Runtime::empty_builder()
        .with_provider_instance(anthropic)
        .with_provider_instance(openai)
        .build()
        .expect("build runtime");
    let parent = runtime.spawn("parent", parent_model).expect("spawn parent");

    let template = parent
        .disposable_subagent_template()
        .with_model(cheap_model.clone());
    let mut child = parent
        .spawn_subagent_from(template)
        .expect("spawn from an overridden template");

    assert_eq!(child.model(), cheap_model.id);
    assert_eq!(child.context_window(), cheap_model.context_window);

    // Prove the provider actually switched, not just the model id string: the
    // child's run must reach the OpenAI-kind provider, never the parent's.
    child
        .run(
            vec![crate::ContentBlock::text("go")],
            crate::runtime::RunOptions::default(),
        )
        .await
        .expect("child run succeeds on the overridden provider");

    assert_eq!(openai_handle.recorded_requests().await.len(), 1);
    assert_eq!(anthropic_handle.recorded_requests().await.len(), 0);
}

#[tokio::test]
async fn overridden_model_naming_an_unregistered_provider_fails_at_spawn() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let parent = runtime.spawn("parent", model).expect("spawn parent");

    let ghost_model = ModelInfo::new("ghost-model", BuiltinProvider::Gemini);
    let template = parent
        .disposable_subagent_template()
        .with_model(ghost_model);

    match parent.spawn_subagent_from(template) {
        Err(RuntimeError::ProviderNotFound(_)) => {}
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("no provider is registered for the overridden model"),
    }
}

#[tokio::test]
async fn overridden_system_still_carries_the_subagent_suffix() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let parent = runtime.spawn("parent", model).expect("spawn parent");

    // The parent has no system prompt of its own, so a plain
    // `spawn_subagent()` child's system prompt is exactly the standard
    // subagent suffix -- derived here rather than duplicating its text.
    let baseline = parent.spawn_subagent().expect("spawn baseline child");
    let suffix = baseline
        .config()
        .system
        .clone()
        .expect("a subagent always gets a system prompt");

    let template = parent
        .disposable_subagent_template()
        .with_system("You are a narrow triage worker.");
    let overridden = parent
        .spawn_subagent_from(template)
        .expect("spawn from an overridden template");

    assert_eq!(
        overridden.config().system.as_deref(),
        Some(format!("You are a narrow triage worker.\n\n{suffix}").as_str())
    );
}

#[tokio::test]
async fn default_template_spawns_byte_identically_to_spawn_subagent() {
    let model = model_info("model", BuiltinProvider::Anthropic);
    let provider = ScriptedProvider::new(BuiltinProvider::Anthropic, vec![model.clone()], vec![]);
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let parent = runtime.spawn("parent", model).expect("spawn parent");

    let via_spawn_subagent = parent.spawn_subagent().expect("spawn via spawn_subagent");
    let via_template = parent
        .spawn_subagent_from(parent.disposable_subagent_template())
        .expect("spawn via an un-overridden template");

    assert_eq!(via_spawn_subagent.name(), via_template.name());
    assert_eq!(via_spawn_subagent.model(), via_template.model());
    assert_eq!(
        via_spawn_subagent.context_window(),
        via_template.context_window()
    );
    assert_eq!(via_spawn_subagent.config(), via_template.config());
    assert_eq!(via_spawn_subagent.max_rounds(), via_template.max_rounds());

    for tool in ["read", "grep", "write", "task"] {
        assert_eq!(
            via_spawn_subagent.can_use_tool(tool),
            via_template.can_use_tool(tool),
            "tool visibility diverged for {tool:?}"
        );
    }
}
