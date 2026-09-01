//! Public-API tests for skill registration and enumeration.
//!
//! These exercise what a host can actually reach: registering roots through
//! `Runtime`, taking them back again, listing what loaded, and naming the
//! error type in its own signatures.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use mentra::{BuiltinProvider, Runtime, SkillInfo, SkillLoadError, skill_root_key};

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
    assert_eq!(review.root, workspace);
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
        root: source_root,
        ..
    } = &skills[0];
    assert_eq!(name, "haiku");
    assert_eq!(description, "writes haiku");
    assert!(
        *model_invocable,
        "a skill is the model's to reach unless its frontmatter says otherwise"
    );
    assert_eq!(path, &root.join("haiku").join("SKILL.md"));
    assert_eq!(
        source_root, &root,
        "the root is reported exactly as it was registered, so a host can pass \
         it straight back to unregister_skills_dir"
    );
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
fn a_failed_registration_commits_none_of_its_roots() {
    // All-or-nothing per call: a host that gets an `Err` back knows the
    // runtime is exactly as it was, so fixing the bad root and calling again
    // is a retry rather than a second, overlapping registration.
    let good = temp_dir("partial-good");
    write_skill(&good, "keep", "keep", "kept", "BODY");
    let missing = temp_dir("partial-missing").join("absent");

    let runtime = runtime();
    let error = runtime
        .register_skills_dirs([good.as_path(), missing.as_path()])
        .expect_err("the second root fails");

    assert!(matches!(error, SkillLoadError::ReadDir { .. }));
    assert!(
        runtime.skills().is_empty(),
        "the root that loaded before the failure must not stay registered"
    );

    // And the retry, once the bad root is dropped, is clean.
    runtime
        .register_skills_dirs([good.as_path()])
        .expect("the retry registers");
    assert_eq!(runtime.skills().len(), 1);
}

#[test]
fn a_failed_registration_leaves_earlier_registrations_alone() {
    let existing = temp_dir("atomic-existing");
    write_skill(&existing, "keep", "keep", "kept", "BODY");
    let good = temp_dir("atomic-good");
    write_skill(&good, "extra", "extra", "extra", "BODY");
    let missing = temp_dir("atomic-missing").join("absent");

    let runtime = runtime();
    runtime
        .register_skills_dir(&existing)
        .expect("the first call registers");

    runtime
        .register_skills_dirs([good.as_path(), missing.as_path()])
        .expect_err("the failing call fails");

    let names: Vec<String> = runtime
        .skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(
        names,
        vec!["keep".to_string()],
        "a failed call rolls back only itself"
    );
}

#[test]
fn an_unregistered_root_takes_its_skills_with_it() {
    let workspace = temp_dir("drop-workspace");
    write_skill(&workspace, "ship", "ship", "workspace ship", "SHIP");
    let global = temp_dir("drop-global");
    write_skill(&global, "deploy", "deploy", "personal deploy", "DEPLOY");

    let runtime = runtime();
    runtime
        .register_skills_dirs([workspace.as_path(), global.as_path()])
        .expect("both roots register");

    assert!(
        runtime.unregister_skills_dir(&workspace),
        "the root was registered, so removing it reports true"
    );

    let names: Vec<String> = runtime
        .skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(names, vec!["deploy".to_string()]);
    assert!(
        runtime.skill_body("ship").is_err(),
        "a dropped root's skill is unreachable, not merely unlisted"
    );
    assert!(
        !runtime.unregister_skills_dir(&workspace),
        "removing it twice reports false the second time"
    );
    assert!(
        !runtime.unregister_skills_dir(temp_dir("drop-never")),
        "a root that was never registered reports false"
    );
}

#[test]
fn unregistering_the_root_that_won_restores_the_skill_it_shadowed() {
    // The case a destructive merge could not express: the shadowed skill was
    // never deleted, only outranked, so dropping the winner brings it back.
    let workspace = temp_dir("restore-workspace");
    write_skill(&workspace, "review", "review", "project review", "PROJECT");
    let global = temp_dir("restore-global");
    write_skill(&global, "review", "review", "personal review", "PERSONAL");

    let runtime = runtime();
    runtime
        .register_skills_dirs([workspace.as_path(), global.as_path()])
        .expect("both roots register");
    assert!(
        runtime
            .skill_body("review")
            .expect("review resolves")
            .contains("PROJECT")
    );

    runtime.unregister_skills_dir(&workspace);

    let skills = runtime.skills();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].description, "personal review");
    assert_eq!(skills[0].root, global);
    assert!(
        runtime
            .skill_body("review")
            .expect("the shadowed skill is back")
            .contains("PERSONAL")
    );
}

#[test]
fn several_roots_can_be_dropped_in_one_call() {
    let first = temp_dir("drop-many-first");
    write_skill(&first, "one", "one", "one", "A");
    let second = temp_dir("drop-many-second");
    write_skill(&second, "two", "two", "two", "B");
    let third = temp_dir("drop-many-third");
    write_skill(&third, "three", "three", "three", "C");

    let runtime = runtime();
    runtime
        .register_skills_dirs([first.as_path(), second.as_path(), third.as_path()])
        .expect("three roots register");

    let never = temp_dir("drop-many-never");
    assert!(
        runtime.unregister_skills_dirs([first.as_path(), never.as_path()]),
        "one match is enough to report true"
    );

    let names: Vec<String> = runtime
        .skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(names, vec!["three".to_string(), "two".to_string()]);

    assert!(
        !runtime.unregister_skills_dirs([never.as_path()]),
        "no match at all reports false"
    );
}

#[test]
fn root_keys_collapse_equivalent_existing_spellings() {
    let root = temp_dir("key-equivalent");
    let child = root.join("child");
    fs::create_dir(&child).expect("create child");
    let indirect = child.join("..");

    assert_eq!(skill_root_key(&root), skill_root_key(&indirect));
}

#[test]
fn an_unresolved_root_key_is_the_path_verbatim() {
    let missing = temp_dir("key-unresolved").join("does-not-exist");

    assert_eq!(skill_root_key(&missing), missing);
}

#[test]
fn a_captured_root_key_survives_root_deletion() {
    let root = temp_dir("key-deleted");
    write_skill(&root, "one", "one", "one", "A");
    let indirect = root.join("one").join("..");
    let key = skill_root_key(&indirect);

    let runtime = runtime();
    runtime
        .register_skills_dir(&indirect)
        .expect("the indirect root registers");
    assert!(has_load_skill(&runtime));

    fs::remove_dir_all(&root).expect("delete root");

    assert!(runtime.unregister_skills_dir(&key));
    assert!(runtime.skills().is_empty());
    assert!(!has_load_skill(&runtime));
}

#[test]
fn a_root_is_matched_by_the_directory_it_names_not_its_spelling() {
    // A host that built the path differently the second time — or normalized
    // it through the filesystem — is still naming the same root.
    let root = temp_dir("spelling");
    write_skill(&root, "one", "one", "one", "A");

    let runtime = runtime();
    runtime.register_skills_dir(&root).expect("registers");

    assert!(
        runtime.unregister_skills_dir(root.join("one").join("..")),
        "an equivalent path names the same root"
    );
    assert!(runtime.skills().is_empty());
}

#[test]
fn registering_a_root_again_reloads_it_in_place() {
    // One entry per root: re-registering refreshes it and keeps the
    // precedence it already had, rather than stacking a shadowed copy that a
    // single unregister would leave behind.
    let workspace = temp_dir("reload-workspace");
    write_skill(&workspace, "review", "review", "first pass", "A");
    let global = temp_dir("reload-global");
    write_skill(&global, "review", "review", "personal review", "B");

    let runtime = runtime();
    runtime
        .register_skills_dirs([workspace.as_path(), global.as_path()])
        .expect("both roots register");

    write_skill(&workspace, "review", "review", "second pass", "A2");
    runtime
        .register_skills_dir(&workspace)
        .expect("the same root registers again");

    let skills = runtime.skills();
    assert_eq!(skills.len(), 1, "the reloaded root replaced its own entry");
    assert_eq!(skills[0].description, "second pass");

    assert!(runtime.unregister_skills_dir(&workspace));
    assert_eq!(
        runtime.skills()[0].description,
        "personal review",
        "one unregister is enough to drop a root registered twice"
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

#[test]
fn the_load_skill_tool_arrives_with_the_first_root_and_leaves_with_the_last() {
    // A tool that can only answer "no skills are registered" is worth tokens
    // to nobody, so it tracks whether any root is registered at all.
    let root = temp_dir("tool-lifecycle");
    write_skill(&root, "one", "one", "one", "A");

    let runtime = runtime();
    assert!(!has_load_skill(&runtime), "no root, no tool");

    runtime.register_skills_dir(&root).expect("registers");
    assert!(has_load_skill(&runtime));

    assert!(runtime.unregister_skills_dir(&root));
    assert!(
        !has_load_skill(&runtime),
        "the last root took the tool with it"
    );

    runtime.register_skills_dir(&root).expect("registers again");
    assert!(
        has_load_skill(&runtime),
        "the next registration restores it"
    );
}

fn has_load_skill(runtime: &Runtime) -> bool {
    runtime.tool_descriptor("load_skill").is_some()
}
