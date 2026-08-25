//! [`PermissionRuleStore`] on one `rules.json` holding every scope, replaced
//! atomically. Scoping semantics mirror the volatile and SQLite stores:
//! saving replaces only the session-scoped rules of that session; loading
//! unions session, matching-project, and global rules.

use serde::{Deserialize, Serialize};

use crate::session::{PermissionRuleScope, permission::RememberedRule};

use super::{
    super::store::PermissionRuleStore, FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util,
    lock_unpoisoned, store_error,
};

#[derive(Serialize, Deserialize)]
struct RulesFile {
    schema: u32,
    rules: Vec<StoredRule>,
}

#[derive(Serialize, Deserialize)]
struct StoredRule {
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    rule: RememberedRule,
}

impl PermissionRuleStore for FileRuntimeStore {
    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError> {
        let _guard = lock_unpoisoned(&self.rules_lock);
        let mut stored = self.read_rules()?;
        stored.retain(|entry| {
            !(entry.session_id == session_id && entry.rule.scope == PermissionRuleScope::Session)
        });
        stored.extend(rules.iter().map(|rule| StoredRule {
            session_id: session_id.to_string(),
            project_id: project_id.map(str::to_string),
            rule: rule.clone(),
        }));
        self.write_rules(stored)
    }

    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        Ok(self
            .read_rules()?
            .into_iter()
            .filter(|entry| match entry.rule.scope {
                PermissionRuleScope::Session => entry.session_id == session_id,
                PermissionRuleScope::Project => {
                    project_id.is_some() && entry.project_id.as_deref() == project_id
                }
                PermissionRuleScope::Global => true,
            })
            .map(|entry| entry.rule)
            .collect())
    }

    fn clear_rules(&self, session_id: &str) -> Result<(), RuntimeError> {
        let _guard = lock_unpoisoned(&self.rules_lock);
        let mut stored = self.read_rules()?;
        stored.retain(|entry| entry.session_id != session_id);
        self.write_rules(stored)
    }
}

impl FileRuntimeStore {
    fn read_rules(&self) -> Result<Vec<StoredRule>, RuntimeError> {
        let path = self.rules_path();
        let Some(contents) = fs_util::read_optional(&path)? else {
            return Ok(Vec::new());
        };
        let file: RulesFile = serde_json::from_str(&contents)
            .map_err(|error| store_error(&format!("parse '{}'", path.display()), error))?;
        if file.schema > SCHEMA_VERSION {
            return Err(RuntimeError::Store(format!(
                "rules.json schema {} is newer than this build understands ({SCHEMA_VERSION})",
                file.schema
            )));
        }
        Ok(file.rules)
    }

    fn write_rules(&self, rules: Vec<StoredRule>) -> Result<(), RuntimeError> {
        let file = RulesFile {
            schema: SCHEMA_VERSION,
            rules,
        };
        let contents = serde_json::to_string_pretty(&file)
            .map_err(|error| RuntimeError::Store(error.to_string()))?;
        fs_util::atomic_replace(&self.rules_path(), contents.as_bytes())
    }
}
