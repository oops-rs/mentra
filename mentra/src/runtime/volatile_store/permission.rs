use crate::{
    runtime::{PermissionRuleContext, PermissionRuleStore, RuntimeError},
    session::{PermissionRuleAddress, PermissionRuleScope, permission::RememberedRule},
};

use super::super::store::canonicalize_permission_rules;
use super::VolatileRuntimeStore;

struct StoredRule {
    session_id: String,
    project_id: Option<String>,
    rule: RememberedRule,
}

/// Durable permission rules mirroring the default store's
/// session/project/global scoping in `permission_rules`. Process rules belong
/// to a live session binding and are rejected here even though this backend is
/// itself in memory: the backend can outlive and be shared by many sessions.
#[derive(Default)]
pub(super) struct PermissionState {
    rules: Vec<StoredRule>,
}

fn context(session_id: &str, project_id: Option<&str>) -> PermissionRuleContext {
    PermissionRuleContext {
        session_id: session_id.to_owned(),
        project_id: project_id.map(str::to_owned),
    }
}

fn in_namespace(
    stored: &StoredRule,
    context: &PermissionRuleContext,
    scope: PermissionRuleScope,
) -> bool {
    if stored.rule.scope != scope {
        return false;
    }
    match scope {
        PermissionRuleScope::Process => false,
        PermissionRuleScope::Session => stored.session_id == context.session_id,
        PermissionRuleScope::Project => {
            context.project_id.is_some()
                && stored.project_id.as_deref() == context.project_id.as_deref()
        }
        PermissionRuleScope::Global => true,
    }
}

fn at_address(
    stored: &StoredRule,
    context: &PermissionRuleContext,
    address: &PermissionRuleAddress,
) -> bool {
    in_namespace(stored, context, address.scope) && stored.rule.key == address.key
}

fn stored_rule(context: &PermissionRuleContext, rule: &RememberedRule) -> StoredRule {
    StoredRule {
        session_id: context.session_id.clone(),
        project_id: match rule.scope {
            PermissionRuleScope::Project => context.project_id.clone(),
            PermissionRuleScope::Process
            | PermissionRuleScope::Session
            | PermissionRuleScope::Global => None,
        },
        rule: rule.clone(),
    }
}

fn upsert(rules: &mut Vec<StoredRule>, context: &PermissionRuleContext, rule: &RememberedRule) {
    let address = PermissionRuleAddress::from(rule);
    rules.retain(|stored| !at_address(stored, context, &address));
    rules.push(stored_rule(context, rule));
}

impl PermissionRuleStore for VolatileRuntimeStore {
    fn upsert_rule(
        &self,
        context: &PermissionRuleContext,
        rule: &RememberedRule,
    ) -> Result<(), RuntimeError> {
        context.validate_persisted_scope(rule.scope)?;
        let mut state = self.lock();
        upsert(&mut state.permissions.rules, context, rule);
        Ok(())
    }

    fn load_applicable_rules(
        &self,
        context: &PermissionRuleContext,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        let state = self.lock();
        Ok(canonicalize_permission_rules(
            state
                .permissions
                .rules
                .iter()
                .filter(|stored| in_namespace(stored, context, stored.rule.scope))
                .map(|stored| stored.rule.clone()),
        ))
    }

    fn revoke_rule(
        &self,
        context: &PermissionRuleContext,
        address: &PermissionRuleAddress,
    ) -> Result<bool, RuntimeError> {
        context.validate_persisted_scope(address.scope)?;
        let mut state = self.lock();
        let before = state.permissions.rules.len();
        state
            .permissions
            .rules
            .retain(|stored| !at_address(stored, context, address));
        Ok(before != state.permissions.rules.len())
    }

    fn clear_scope(
        &self,
        context: &PermissionRuleContext,
        scope: PermissionRuleScope,
    ) -> Result<usize, RuntimeError> {
        context.validate_persisted_scope(scope)?;
        let mut state = self.lock();
        let before = state.permissions.rules.len();
        state
            .permissions
            .rules
            .retain(|stored| !in_namespace(stored, context, scope));
        Ok(before - state.permissions.rules.len())
    }

    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError> {
        let context = context(session_id, project_id);
        for rule in rules {
            context.validate_persisted_scope(rule.scope)?;
        }
        let mut state = self.lock();
        state
            .permissions
            .rules
            .retain(|stored| !in_namespace(stored, &context, PermissionRuleScope::Session));
        for rule in rules {
            upsert(&mut state.permissions.rules, &context, rule);
        }
        Ok(())
    }

    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        self.load_applicable_rules(&context(session_id, project_id))
    }

    fn clear_rules(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.lock()
            .permissions
            .rules
            .retain(|stored| stored.session_id != session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        runtime::{PermissionRuleContext, PermissionRuleStore},
        session::{
            PermissionRuleAddress, PermissionRuleScope,
            permission::{RememberedRule, RuleKey},
        },
    };

    use super::{super::VolatileRuntimeStore, StoredRule};

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
    fn save_load_clear_round_trip_scoped_by_session_and_project() {
        let store = VolatileRuntimeStore::new();

        store
            .save_rules(
                "session-a",
                None,
                &[rule("shell", true, PermissionRuleScope::Session)],
            )
            .expect("save session-a rules");
        store
            .save_rules(
                "session-b",
                Some("proj-b"),
                &[rule("read", false, PermissionRuleScope::Project)],
            )
            .expect("save session-b rules");

        let loaded_a = store.load_rules("session-a", None).expect("load session-a");
        assert_eq!(loaded_a.len(), 1);
        assert_eq!(loaded_a[0].key.tool_name, "shell");

        let loaded_b = store
            .load_rules("session-b", Some("proj-b"))
            .expect("load session-b");
        assert_eq!(loaded_b.len(), 1);
        assert_eq!(loaded_b[0].key.tool_name, "read");

        // session-b's project rule does not leak into session-a's load
        // without a matching project id.
        assert!(
            store
                .load_rules("session-b", None)
                .expect("load session-b without project id")
                .is_empty()
        );

        store.clear_rules("session-a").expect("clear session-a");
        assert!(
            store
                .load_rules("session-a", None)
                .expect("load after clear")
                .is_empty()
        );
    }

    #[test]
    fn save_rules_replaces_only_session_scoped_rules() {
        let store = VolatileRuntimeStore::new();
        store
            .save_rules(
                "session-1",
                None,
                &[rule("shell", true, PermissionRuleScope::Session)],
            )
            .expect("save initial");
        store
            .save_rules(
                "session-1",
                None,
                &[rule("write", false, PermissionRuleScope::Session)],
            )
            .expect("save replacement");

        let loaded = store.load_rules("session-1", None).expect("load rules");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key.tool_name, "write");
    }

    #[test]
    fn a_remembered_refusal_keeps_its_reason() {
        let store = VolatileRuntimeStore::new();
        let refusal = RememberedRule {
            reason: Some("this run does not allow writes".to_string()),
            ..rule("write", false, PermissionRuleScope::Session)
        };

        store
            .save_rules("session-1", None, &[refusal])
            .expect("save refusal");

        let loaded = store.load_rules("session-1", None).expect("load refusal");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].reason.as_deref(),
            Some("this run does not allow writes"),
            "the volatile store answers a remembered refusal the same as the persistent one"
        );
    }

    #[test]
    fn point_operations_follow_the_shared_permission_store_contract() {
        let store = VolatileRuntimeStore::new();
        crate::runtime::store::permission_contract::assert_permission_rule_store_contract(&store);
    }

    #[test]
    fn legacy_duplicates_load_fail_safe_and_exact_mutations_remove_every_row() {
        let store = VolatileRuntimeStore::new();
        let context = PermissionRuleContext {
            session_id: "current".to_owned(),
            project_id: None,
        };
        let duplicate = |session_id: &str, allow: bool, reason: Option<&str>| StoredRule {
            session_id: session_id.to_owned(),
            project_id: None,
            rule: RememberedRule {
                key: RuleKey {
                    tool_name: "shell".to_owned(),
                    pattern: None,
                },
                allow,
                scope: PermissionRuleScope::Global,
                reason: reason.map(str::to_owned),
            },
        };
        {
            let mut state = store.lock();
            state.permissions.rules.extend([
                duplicate("legacy-allow", true, Some("allowed")),
                duplicate("legacy-reasonless", false, None),
                duplicate("legacy-zeta", false, Some("zeta")),
                duplicate("legacy-alpha", false, Some("alpha")),
            ]);
        }

        let loaded = store
            .load_applicable_rules(&context)
            .expect("load legacy duplicates");
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].allow);
        assert_eq!(loaded[0].reason.as_deref(), Some("alpha"));

        let address = PermissionRuleAddress {
            scope: PermissionRuleScope::Global,
            key: loaded[0].key.clone(),
        };
        assert!(
            store
                .revoke_rule(&context, &address)
                .expect("revoke duplicate address")
        );
        assert!(
            store
                .load_applicable_rules(&context)
                .expect("load after revoke")
                .is_empty()
        );

        {
            let mut state = store.lock();
            state.permissions.rules.extend([
                duplicate("legacy-1", false, Some("one")),
                duplicate("legacy-2", false, Some("two")),
            ]);
        }
        assert_eq!(
            store
                .clear_scope(&context, PermissionRuleScope::Global)
                .expect("clear duplicate namespace"),
            2
        );
    }
}
