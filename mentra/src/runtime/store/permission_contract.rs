use crate::{
    runtime::{PermissionRuleContext, PermissionRuleStore},
    session::{PermissionRuleAddress, PermissionRuleScope, RememberedRule, RuleKey},
};

fn context(session_id: &str, project_id: Option<&str>) -> PermissionRuleContext {
    PermissionRuleContext {
        session_id: session_id.to_owned(),
        project_id: project_id.map(str::to_owned),
    }
}

fn rule(scope: PermissionRuleScope, tool_name: &str, allow: bool, reason: &str) -> RememberedRule {
    RememberedRule {
        key: RuleKey {
            tool_name: tool_name.to_owned(),
            pattern: None,
        },
        allow,
        scope,
        reason: Some(reason.to_owned()),
    }
}

fn address(scope: PermissionRuleScope, tool_name: &str) -> PermissionRuleAddress {
    PermissionRuleAddress {
        scope,
        key: RuleKey {
            tool_name: tool_name.to_owned(),
            pattern: None,
        },
    }
}

pub(crate) fn assert_permission_rule_store_contract(store: &dyn PermissionRuleStore) {
    let session_a = context("session-a", Some("project-1"));
    let session_b = context("session-b", Some("project-1"));
    let other_project = context("session-c", Some("project-2"));
    let no_project = context("session-no-project", None);
    let empty_project = context("session-empty-project", Some(""));

    store
        .upsert_rule(
            &session_a,
            &rule(PermissionRuleScope::Global, "shell", true, "global"),
        )
        .expect("upsert global rule");
    store
        .upsert_rule(
            &session_a,
            &rule(PermissionRuleScope::Project, "shell", false, "project"),
        )
        .expect("upsert project rule");
    store
        .upsert_rule(
            &session_a,
            &rule(PermissionRuleScope::Session, "shell", true, "session"),
        )
        .expect("upsert session rule");

    let applicable_a = store
        .load_applicable_rules(&session_a)
        .expect("load session-a rules");
    assert_eq!(
        applicable_a
            .iter()
            .map(|rule| rule.scope)
            .collect::<Vec<_>>(),
        vec![
            PermissionRuleScope::Session,
            PermissionRuleScope::Project,
            PermissionRuleScope::Global,
        ]
    );
    assert_eq!(
        store
            .load_applicable_rules(&session_b)
            .expect("load sibling project rules")
            .iter()
            .map(|rule| rule.scope)
            .collect::<Vec<_>>(),
        vec![PermissionRuleScope::Project, PermissionRuleScope::Global]
    );
    assert_eq!(
        store
            .load_applicable_rules(&other_project)
            .expect("load other project rules")
            .iter()
            .map(|rule| rule.scope)
            .collect::<Vec<_>>(),
        vec![PermissionRuleScope::Global]
    );

    store
        .upsert_rule(
            &session_b,
            &rule(
                PermissionRuleScope::Global,
                "shell",
                false,
                "global replacement",
            ),
        )
        .expect("replace the one global address");
    let global = store
        .load_applicable_rules(&other_project)
        .expect("load replaced global rule");
    assert_eq!(global.len(), 1);
    assert!(!global[0].allow);
    assert_eq!(global[0].reason.as_deref(), Some("global replacement"));

    let project_rule = rule(PermissionRuleScope::Project, "files", true, "project files");
    assert!(store.upsert_rule(&no_project, &project_rule).is_err());
    assert!(
        store
            .revoke_rule(&no_project, &address(PermissionRuleScope::Project, "files"))
            .is_err()
    );
    assert!(
        store
            .clear_scope(&no_project, PermissionRuleScope::Project)
            .is_err()
    );
    assert!(
        store
            .load_applicable_rules(&no_project)
            .expect("load without project")
            .iter()
            .all(|rule| rule.scope != PermissionRuleScope::Project)
    );
    store
        .upsert_rule(
            &empty_project,
            &rule(
                PermissionRuleScope::Project,
                "empty-project-only",
                false,
                "empty project",
            ),
        )
        .expect("an explicit empty project id remains an opaque namespace");
    assert!(
        store
            .load_applicable_rules(&no_project)
            .expect("missing project must not alias empty project")
            .iter()
            .all(|rule| rule.key.tool_name != "empty-project-only")
    );

    let project_shell = address(PermissionRuleScope::Project, "shell");
    assert!(
        store
            .revoke_rule(&session_b, &project_shell)
            .expect("revoke project rule")
    );
    assert!(
        !store
            .revoke_rule(&session_b, &project_shell)
            .expect("repeat project revoke")
    );
    assert_eq!(
        store
            .load_applicable_rules(&session_a)
            .expect("load after project revoke")
            .iter()
            .map(|rule| rule.scope)
            .collect::<Vec<_>>(),
        vec![PermissionRuleScope::Session, PermissionRuleScope::Global]
    );

    assert_eq!(
        store
            .clear_scope(&other_project, PermissionRuleScope::Global)
            .expect("clear global namespace"),
        1
    );
    assert_eq!(
        store
            .clear_scope(&session_a, PermissionRuleScope::Global)
            .expect("repeat global clear"),
        0
    );
    assert_eq!(
        store
            .clear_scope(&session_a, PermissionRuleScope::Session)
            .expect("clear session namespace"),
        1
    );

    store
        .upsert_rule(
            &session_a,
            &rule(PermissionRuleScope::Project, "read", true, "shared project"),
        )
        .expect("upsert shared project rule");
    store
        .upsert_rule(
            &session_a,
            &rule(
                PermissionRuleScope::Global,
                "network",
                false,
                "shared global",
            ),
        )
        .expect("upsert shared global rule");
    let mut inherited = store
        .load_applicable_rules(&session_b)
        .expect("load inherited rules");
    inherited.push(rule(
        PermissionRuleScope::Session,
        "shell",
        true,
        "session-b",
    ));
    store
        .save_rules(
            &session_b.session_id,
            session_b.project_id.as_deref(),
            &inherited,
        )
        .expect("legacy bulk save");

    assert_eq!(
        store
            .clear_scope(&session_b, PermissionRuleScope::Project)
            .expect("clear canonical project rule"),
        1,
        "saving an inherited snapshot must not duplicate the project address"
    );
    assert_eq!(
        store
            .clear_scope(&session_b, PermissionRuleScope::Global)
            .expect("clear canonical global rule"),
        1,
        "saving an inherited snapshot must not duplicate the global address"
    );
    let remaining = store
        .load_applicable_rules(&session_b)
        .expect("load remaining session rule");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].scope, PermissionRuleScope::Session);
    assert_eq!(remaining[0].key.tool_name, "shell");
}
