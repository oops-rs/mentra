//! [`PermissionRuleStore`] on one `rules.json` holding every durable scope,
//! replaced atomically. Scoping semantics mirror the volatile and SQLite
//! stores: saving replaces only the session-scoped rules of that session;
//! loading unions session, matching-project, and global rules. Process rules
//! belong to a live session binding and are rejected here. Loads reuse a
//! parsed snapshot while the file handle identity and metadata stay unchanged;
//! mutations always reread disk under `rules.lock` before changing that cache.

use std::{fs::File, io::Read as _, time::SystemTime};

use same_file::Handle;
use serde::{Deserialize, Serialize};

use crate::session::{PermissionRuleAddress, PermissionRuleScope, permission::RememberedRule};

use super::{
    super::store::{PermissionRuleContext, PermissionRuleStore, canonicalize_permission_rules},
    FileRuntimeStore, RuntimeError, SCHEMA_VERSION, fs_util, lock_unpoisoned, parse_versioned,
    to_pretty_json,
};

#[derive(Deserialize)]
struct RulesFile {
    #[serde(rename = "schema")]
    _schema: u32,
    rules: Vec<StoredRule>,
}

#[derive(Serialize)]
struct RulesFileRef<'a> {
    schema: u32,
    rules: &'a [StoredRule],
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct StoredRule {
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    rule: RememberedRule,
}

#[derive(Eq, PartialEq)]
enum RulesFileIdentity {
    Missing,
    Present {
        /// The open handle keeps a replaced file's identity alive, preventing
        /// inode/file-id reuse from turning a replacement into a cache hit.
        handle: Handle,
        len: u64,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
    },
}

struct RulesSnapshot {
    identity: RulesFileIdentity,
    rules: Vec<StoredRule>,
}

enum ObservedRulesFile {
    Missing,
    Present {
        file: File,
        identity: RulesFileIdentity,
    },
}

impl ObservedRulesFile {
    fn identity(&self) -> &RulesFileIdentity {
        match self {
            Self::Missing => &RulesFileIdentity::Missing,
            Self::Present { identity, .. } => identity,
        }
    }
}

#[derive(Default)]
pub(super) struct RulesState {
    cached: Option<RulesSnapshot>,
    #[cfg(test)]
    cache_misses: usize,
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
        context.validate_persisted_scope(rule.scope)?;
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
            self.read_rules_cached()?
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
        context.validate_persisted_scope(address.scope)?;
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
        context.validate_persisted_scope(scope)?;
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
            context.validate_persisted_scope(rule.scope)?;
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
        let mut state = lock_unpoisoned(&self.rules_state);
        let _file_guard = fs_util::lock_exclusive(&self.rules_lock_path())?;
        // This is intentionally never served from `state.cached`: a mutation
        // must derive from the last replacement made by any store/process.
        let mut snapshot = self.read_rules_from_disk()?;
        state.cache_miss();
        let (result, changed) = mutation(&mut snapshot.rules);
        if changed {
            self.write_rules(&snapshot.rules)?;
            // The replacement is authoritative. No later path observation is
            // transactionally tied to that exact file, so invalidate instead
            // of risking a cache-only error or pairing these rows with a file
            // another (non-locking) actor installed after the commit.
            state.cached = None;
        } else {
            state.cached = Some(snapshot);
        }
        Ok(result)
    }

    fn read_rules_cached(&self) -> Result<Vec<StoredRule>, RuntimeError> {
        let mut state = lock_unpoisoned(&self.rules_state);
        let observed = self.observe_rules_file()?;
        if let Some(cached) = &state.cached
            && cached.identity == *observed.identity()
        {
            return Ok(cached.rules.clone());
        }

        let snapshot = self.parse_observed_rules(observed)?;
        state.cache_miss();
        let rules = snapshot.rules.clone();
        state.cached = Some(snapshot);
        Ok(rules)
    }

    fn read_rules_from_disk(&self) -> Result<RulesSnapshot, RuntimeError> {
        let observed = self.observe_rules_file()?;
        self.parse_observed_rules(observed)
    }

    fn observe_rules_file(&self) -> Result<ObservedRulesFile, RuntimeError> {
        let path = self.rules_path();
        let Some(file) = fs_util::open_optional(&path)? else {
            return Ok(ObservedRulesFile::Missing);
        };
        let metadata = file.metadata().map_err(|error| {
            super::store_error(&format!("read metadata for '{}'", path.display()), error)
        })?;
        let identity_handle = file.try_clone().map_err(|error| {
            super::store_error(&format!("duplicate handle for '{}'", path.display()), error)
        })?;
        let handle = Handle::from_file(identity_handle).map_err(|error| {
            super::store_error(&format!("identify '{}'", path.display()), error)
        })?;
        Ok(ObservedRulesFile::Present {
            file,
            identity: RulesFileIdentity::Present {
                handle,
                len: metadata.len(),
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
            },
        })
    }

    fn parse_observed_rules(
        &self,
        observed: ObservedRulesFile,
    ) -> Result<RulesSnapshot, RuntimeError> {
        let ObservedRulesFile::Present { mut file, identity } = observed else {
            return Ok(RulesSnapshot {
                identity: RulesFileIdentity::Missing,
                rules: Vec::new(),
            });
        };
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|error| super::store_error("read 'rules.json'", error))?;
        let parsed: RulesFile = parse_versioned(&contents, "rules.json")?;
        Ok(RulesSnapshot {
            identity,
            rules: parsed.rules,
        })
    }

    fn write_rules(&self, rules: &[StoredRule]) -> Result<(), RuntimeError> {
        let file = RulesFileRef {
            schema: SCHEMA_VERSION,
            rules,
        };
        fs_util::atomic_replace(&self.rules_path(), to_pretty_json(&file)?.as_bytes())
    }

    #[cfg(test)]
    pub(super) fn rules_cache_misses(&self) -> usize {
        lock_unpoisoned(&self.rules_state).cache_misses
    }
}

impl RulesState {
    fn cache_miss(&mut self) {
        #[cfg(test)]
        {
            self.cache_misses += 1;
        }
    }
}
