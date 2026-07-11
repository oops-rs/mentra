use crate::{
    runtime::{PermissionRuleStore, RuntimeError},
    session::{PermissionRuleScope, permission::RememberedRule},
};

use super::VolatileRuntimeStore;

struct StoredRule {
    session_id: String,
    project_id: Option<String>,
    rule: RememberedRule,
}

/// Permission rules mirroring the default store's session/project/global
/// scoping in `permission_rules`.
#[derive(Default)]
pub(super) struct PermissionState {
    rules: Vec<StoredRule>,
}

impl PermissionRuleStore for VolatileRuntimeStore {
    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        // Only session-scoped rules for this session are replaced; project-
        // and global-scoped rules are managed separately and untouched here,
        // matching the default store.
        state.permissions.rules.retain(|stored| {
            !(stored.session_id == session_id && stored.rule.scope == PermissionRuleScope::Session)
        });
        for rule in rules {
            state.permissions.rules.push(StoredRule {
                session_id: session_id.to_string(),
                project_id: project_id.map(str::to_string),
                rule: rule.clone(),
            });
        }
        Ok(())
    }

    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        let state = self.lock();
        Ok(state
            .permissions
            .rules
            .iter()
            .filter(|stored| match stored.rule.scope {
                PermissionRuleScope::Session => stored.session_id == session_id,
                PermissionRuleScope::Project => {
                    project_id.is_some() && stored.project_id.as_deref() == project_id
                }
                PermissionRuleScope::Global => true,
            })
            .map(|stored| stored.rule.clone())
            .collect())
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
        runtime::PermissionRuleStore,
        session::{
            PermissionRuleScope,
            permission::{RememberedRule, RuleKey},
        },
    };

    use super::super::VolatileRuntimeStore;

    fn rule(tool_name: &str, allow: bool, scope: PermissionRuleScope) -> RememberedRule {
        RememberedRule {
            key: RuleKey {
                tool_name: tool_name.to_string(),
                pattern: None,
            },
            allow,
            scope,
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
}
