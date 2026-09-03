//! [`PermissionRuleStore`] on one `rules.json` holding every scope, replaced
//! atomically. Scoping semantics mirror the volatile and SQLite stores:
//! saving replaces only the session-scoped rules of that session; loading
//! unions session, matching-project, and global rules.

use serde::{Deserialize, Serialize};

use crate::session::{PermissionRuleAddress, PermissionRuleScope, permission::RememberedRule};

use super::{
    super::store::{PermissionRuleContext, PermissionRuleStore, canonicalize_permission_rules},
    FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util, lock_unpoisoned, parse_versioned,
    to_pretty_json,
};

#[derive(Serialize, Deserialize)]
struct RulesFile {
    schema: u32,
    rules: Vec<StoredRule>,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct StoredRule {
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    rule: RememberedRule,
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
            PermissionRuleScope::Session | PermissionRuleScope::Global => None,
        },
        rule: rule.clone(),
    }
}

fn upsert(stored: &mut Vec<StoredRule>, context: &PermissionRuleContext, rule: &RememberedRule) {
    let address = PermissionRuleAddress::from(rule);
    stored.retain(|entry| !at_address(entry, context, &address));
    stored.push(stored_rule(context, rule));
}

impl PermissionRuleStore for FileRuntimeStore {
    fn upsert_rule(
        &self,
        context: &PermissionRuleContext,
        rule: &RememberedRule,
    ) -> Result<(), RuntimeError> {
        context.validate_scope(rule.scope)?;
        self.mutate_rules(|stored| {
            upsert(stored, context, rule);
            ((), true)
        })
    }

    fn load_applicable_rules(
        &self,
        context: &PermissionRuleContext,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        Ok(canonicalize_permission_rules(
            self.read_rules()?
                .into_iter()
                .filter(|entry| in_namespace(entry, context, entry.rule.scope))
                .map(|entry| entry.rule),
        ))
    }

    fn revoke_rule(
        &self,
        context: &PermissionRuleContext,
        address: &PermissionRuleAddress,
    ) -> Result<bool, RuntimeError> {
        context.validate_scope(address.scope)?;
        self.mutate_rules(|stored| {
            let before = stored.len();
            stored.retain(|entry| !at_address(entry, context, address));
            let removed = before != stored.len();
            (removed, removed)
        })
    }

    fn clear_scope(
        &self,
        context: &PermissionRuleContext,
        scope: PermissionRuleScope,
    ) -> Result<usize, RuntimeError> {
        context.validate_scope(scope)?;
        self.mutate_rules(|stored| {
            let before = stored.len();
            stored.retain(|entry| !in_namespace(entry, context, scope));
            let removed = before - stored.len();
            (removed, removed != 0)
        })
    }

    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError> {
        let context = context(session_id, project_id);
        for rule in rules {
            context.validate_scope(rule.scope)?;
        }
        self.mutate_rules(|stored| {
            stored.retain(|entry| !in_namespace(entry, &context, PermissionRuleScope::Session));
            for rule in rules {
                upsert(stored, &context, rule);
            }
            ((), true)
        })
    }

    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        self.load_applicable_rules(&context(session_id, project_id))
    }

    fn clear_rules(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.mutate_rules(|stored| {
            stored.retain(|entry| entry.session_id != session_id);
            ((), true)
        })
    }
}

impl FileRuntimeStore {
    /// Runs one read-modify-write while holding both clone-local exclusion and
    /// the stable cross-process sidecar lock. The disk read deliberately
    /// happens after both locks are held; atomic replacement alone cannot
    /// prevent two writers from deriving replacements from the same snapshot.
    fn mutate_rules<T>(
        &self,
        mutation: impl FnOnce(&mut Vec<StoredRule>) -> (T, bool),
    ) -> Result<T, RuntimeError> {
        let _process_guard = lock_unpoisoned(&self.rules_lock);
        let _file_guard = fs_util::lock_exclusive(&self.rules_lock_path())?;
        let mut stored = self.read_rules()?;
        let (result, changed) = mutation(&mut stored);
        if changed {
            self.write_rules(stored)?;
        }
        Ok(result)
    }

    fn read_rules(&self) -> Result<Vec<StoredRule>, RuntimeError> {
        let Some(contents) = fs_util::read_optional(&self.rules_path())? else {
            return Ok(Vec::new());
        };
        let file: RulesFile = parse_versioned(&contents, "rules.json")?;
        Ok(file.rules)
    }

    fn write_rules(&self, rules: Vec<StoredRule>) -> Result<(), RuntimeError> {
        let file = RulesFile {
            schema: SCHEMA_VERSION,
            rules,
        };
        fs_util::atomic_replace(&self.rules_path(), to_pretty_json(&file)?.as_bytes())
    }
}
