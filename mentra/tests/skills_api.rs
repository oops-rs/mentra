//! Public-API tests for skill registration and enumeration.
//!
//! These exercise what a host can actually reach: registering roots through
//! `Runtime`, listing what loaded, and naming the error type in its own
//! signatures.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use mentra::{BuiltinProvider, Runtime, SkillInfo, SkillLoadError};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("mentra-skills-api-{label}-{stamp}-{unique}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_skill(root: &Path, dir: &str, name: &str, description: &str, body: &str) {
    let skill_dir = root.join(dir);
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .expect("write skill");
}

fn runtime() -> Runtime {
    Runtime::builder()
        .with_provider(BuiltinProvider::OpenAI, "test-key")
        .build()
        .expect("runtime builds")
}

/// The error type must be nameable by a caller — this signature is the test.
fn register(runtime: &Runtime, path: &Path) -> Result<(), SkillLoadError> {
    runtime.register_skills_dir(path)
}

#[test]
fn a_host_can_name_the_error_type() {
    let runtime = runtime();
    let missing = temp_dir("nameable").join("does-not-exist");

    let error = register(&runtime, &missing).expect_err("an unreadable root is an error");

    // And match on it, which is the point of it being an enum.
    assert!(matches!(error, SkillLoadError::ReadDir { .. }));
}

#[test]
fn registering_a_second_root_keeps_the_first() {
    let workspace = temp_dir("layer-workspace");
    write_skill(&workspace, "review", "review", "project review", "PROJECT");
    let global = temp_dir("layer-global");
    write_skill(&global, "review", "review", "personal review", "PERSONAL");
    write_skill(&global, "deploy", "deploy", "personal deploy", "DEPLOY");

    let runtime = runtime();
    runtime
        .register_skills_dirs([workspace.as_path(), global.as_path()])
        .expect("both roots register");

    let skills = runtime.skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["deploy", "review"]);

    let review = skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review present");
    assert_eq!(
        review.description, "project review",
        "the earlier root must win the collision"
    );
    assert!(review.path.starts_with(&workspace));
}

#[test]
fn enumeration_reports_name_description_and_source() {
    let root = temp_dir("enumerate");
    write_skill(&root, "haiku", "haiku", "writes haiku", "BODY");

    let runtime = runtime();
    runtime.register_skills_dir(&root).expect("registers");

    let skills = runtime.skills();
    assert_eq!(skills.len(), 1);
    let SkillInfo {
        name,
        description,
        model_invocable,
        path,
    } = &skills[0];
    assert_eq!(name, "haiku");
    assert_eq!(description, "writes haiku");
    assert!(
        *model_invocable,
        "a skill is the model's to reach unless its frontmatter says otherwise"
    );
    assert_eq!(path, &root.join("haiku").join("SKILL.md"));
}

#[test]
fn a_runtime_without_skills_lists_none() {
    assert!(runtime().skills().is_empty());
}

#[test]
fn a_duplicate_name_inside_one_root_is_still_an_error() {
    let root = temp_dir("duplicate");
    write_skill(&root, "first", "shared", "one", "A");
    write_skill(&root, "second", "shared", "two", "B");

    let error = runtime()
        .register_skills_dir(&root)
        .expect_err("a repeated name in one root is a mistake");

    assert!(matches!(error, SkillLoadError::DuplicateSkillName { .. }));
}

#[test]
fn roots_before_a_failing_one_stay_registered() {
    let good = temp_dir("partial-good");
    write_skill(&good, "keep", "keep", "kept", "BODY");
    let missing = temp_dir("partial-missing").join("absent");

    let runtime = runtime();
    let error = runtime
        .register_skills_dirs([good.as_path(), missing.as_path()])
        .expect_err("the second root fails");

    assert!(matches!(error, SkillLoadError::ReadDir { .. }));
    assert_eq!(
        runtime.skills().len(),
        1,
        "the root that loaded before the failure stays registered"
    );
}

#[test]
fn a_skill_can_be_kept_out_of_the_models_reach() {
    // `disable-model-invocation` exists for a skill a person invokes
    // deliberately and a model should never reach for on its own.
    let root = temp_dir("disabled-invocation");
    fs::create_dir_all(root.join("release")).expect("create skill dir");
    fs::write(
        root.join("release").join("SKILL.md"),
        "---\nname: release\ndescription: cuts a release\ndisable-model-invocation: true\n---\nSteps\n",
    )
    .expect("write skill");
    write_skill(&root, "haiku", "haiku", "writes haiku", "A");

    let runtime = runtime();
    runtime.register_skills_dir(&root).expect("registers");

    let skills = runtime.skills();
    // The host still sees it: that is what makes it invocable by a person.
    let release = skills
        .iter()
        .find(|skill| skill.name == "release")
        .expect("the skill is still loaded");
    assert!(!release.model_invocable);
    assert!(
        skills
            .iter()
            .find(|skill| skill.name == "haiku")
            .expect("the other skill loaded")
            .model_invocable
    );
}

#[test]
fn a_host_can_run_a_skill_the_model_may_not() {
    // The flag's promise is that a person can still invoke it. Without a host
    // path to the body, such a skill was visible in a listing and reachable by
    // nobody at all.
    let root = temp_dir("host-invocable");
    fs::create_dir_all(root.join("release")).expect("create skill dir");
    fs::write(
        root.join("release").join("SKILL.md"),
        "---\nname: release\ndescription: cuts a release\ndisable-model-invocation: true\n---\nStep one\n",
    )
    .expect("write skill");

    let runtime = runtime();
    runtime.register_skills_dir(&root).expect("registers");

    let body = runtime.skill_body("release").expect("the host may run it");
    assert!(body.contains("Step one"), "{body}");

    assert!(
        runtime.skill_body("nope").is_err(),
        "an unknown skill is still unknown"
    );
}
