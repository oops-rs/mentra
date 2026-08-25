//! [`PermissionRuleStore`] on one `rules.json` holding every scope, replaced
//! atomically. Scoping semantics mirror the volatile and SQLite stores:
//! saving replaces only the session-scoped rules of that session; loading
//! unions session, matching-project, and global rules.

use serde::{Deserialize, Serialize};

use crate::session::{PermissionRuleScope, permission::RememberedRule};

use super::{
    super::store::PermissionRuleStore, FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util,
    lock_unpoisoned, parse_versioned, to_pretty_json,
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

impl PermissionRuleStore for FileRuntimeStore {
    fn save_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
        rules: &[RememberedRule],
    ) -> Result<(), RuntimeError> {
        let _guard = lock_unpoisoned(&self.rules_lock);
        // Saves arrive carrying the session's whole remembered set — project
        // and global rules included — on every call, so writing them
        // verbatim duplicated the non-session rows once per save. The file
        // is rewritten deduplicated by exact row identity, mirroring the
        // SQLite store's load-time UNION semantics.
        let mut stored = dedup_rows(self.read_rules()?);
        stored.retain(|entry| {
            !(entry.session_id == session_id && entry.rule.scope == PermissionRuleScope::Session)
        });
        for rule in rules {
            let row = StoredRule {
                session_id: session_id.to_string(),
                project_id: project_id.map(str::to_string),
                rule: rule.clone(),
            };
            if !stored.contains(&row) {
                stored.push(row);
            }
        }
        self.write_rules(stored)
    }

    fn load_rules(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<RememberedRule>, RuntimeError> {
        let applicable = self
            .read_rules()?
            .into_iter()
            .filter(|entry| match entry.rule.scope {
                PermissionRuleScope::Session => entry.session_id == session_id,
                PermissionRuleScope::Project => {
                    project_id.is_some() && entry.project_id.as_deref() == project_id
                }
                PermissionRuleScope::Global => true,
            })
            .map(|entry| entry.rule);

        // Defensive twin of the save-side dedup, and what the SQLite UNION
        // does: a file that somehow carries duplicates still loads each rule
        // once.
        let mut rules: Vec<RememberedRule> = Vec::new();
        for rule in applicable {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        Ok(rules)
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

/// Collapses exact duplicate rows, keeping first occurrences in order.
fn dedup_rows(rows: Vec<StoredRule>) -> Vec<StoredRule> {
    let mut out: Vec<StoredRule> = Vec::with_capacity(rows.len());
    for row in rows {
        if !out.contains(&row) {
            out.push(row);
        }
    }
    out
}
