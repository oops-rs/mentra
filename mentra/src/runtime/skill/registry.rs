//! The ordered set of skill roots a runtime holds.
//!
//! Roots stay separate here rather than being folded into one map at
//! registration time. Keeping the boundary is what makes a root removable at
//! all, and what makes removal *restorative*: a name a stronger root won is
//! still present in the weaker root's loader, so dropping the winner reveals
//! it again instead of deleting the name.
//!
//! Precedence is registration order, earliest first — the rule
//! [`SkillRegistry::resolve`] applies on every lookup.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::{SkillEntry, SkillInfo, SkillLoadError, SkillLoader, render_skill, skill_root_key};

/// Returned when nothing is registered at all, to separate "this runtime has
/// no skills" from "that name is not one of them".
const NO_LOADER: &str = "Skill loader is not available";

/// One registered root: where a host said its skills live, and what loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRoot {
    /// The path the host passed, kept verbatim.
    ///
    /// This is what [`SkillInfo::root`] reports, so a host recognizes it and
    /// can hand it straight back to `unregister_skills_dir`.
    registered: PathBuf,
    /// The identity a root is matched by: its canonical path where the
    /// filesystem can resolve one, so two spellings of the same directory are
    /// one root rather than two.
    key: PathBuf,
    loader: SkillLoader,
}

impl SkillRoot {
    /// Loads a root without touching any registry.
    ///
    /// Splitting the load from the commit is what makes registration
    /// all-or-nothing: a caller loads every root first and only then hands the
    /// batch over, so a failure anywhere leaves the registry untouched.
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        let path = path.as_ref();
        let loader = SkillLoader::from_dir(path)?;
        Ok(Self {
            registered: path.to_path_buf(),
            key: skill_root_key(path),
            loader,
        })
    }

    /// Whether `path` names this root.
    ///
    /// Canonical paths first, so `a/b/../b` and a symlinked parent still
    /// match. The verbatim comparison behind it covers the root whose
    /// directory has since been deleted — exactly when a host is cleaning up
    /// after a workspace and canonicalization can no longer answer.
    fn matches(&self, path: &Path, key: &Path) -> bool {
        self.key == key || self.registered == path
    }
}

/// Every skill root on a runtime, strongest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SkillRegistry {
    roots: Vec<SkillRoot>,
}

impl SkillRegistry {
    /// Whether any root is registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Commits already-loaded roots in one step.
    ///
    /// A root that is already registered is reloaded *in place*, keeping the
    /// precedence it had. One entry per root is what lets a single
    /// [`remove`](Self::remove) drop it: stacking a second, fully shadowed
    /// copy would leave residue behind the first removal.
    pub(crate) fn insert(&mut self, roots: Vec<SkillRoot>) {
        for root in roots {
            match self
                .roots
                .iter_mut()
                .find(|existing| existing.matches(&root.registered, &root.key))
            {
                Some(existing) => *existing = root,
                None => self.roots.push(root),
            }
        }
    }

    /// Drops the root `path` names, reporting whether one was there.
    ///
    /// Every skill that root contributed goes with it, and any name it had
    /// shadowed resolves to the weaker root again.
    pub(crate) fn remove(&mut self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        let key = skill_root_key(path);
        let before = self.roots.len();
        self.roots.retain(|root| !root.matches(path, &key));
        self.roots.len() != before
    }

    /// The root and entry a name resolves to, or `None` if no root defines it.
    ///
    /// First root wins: roots are registered strongest first, so a workspace
    /// skill shadows a personal one of the same name. The shadowed entry is
    /// not lost, only outranked.
    fn resolve(&self, name: &str) -> Option<(&SkillRoot, &SkillEntry)> {
        self.roots
            .iter()
            .find_map(|root| root.loader.entry(name).map(|entry| (root, entry)))
    }

    /// Every visible skill, name-ordered, with the root it resolved from.
    fn visible(&self) -> BTreeMap<&str, (&SkillRoot, &SkillEntry)> {
        let mut visible = BTreeMap::new();
        for root in &self.roots {
            for (name, entry) in root.loader.entries() {
                visible.entry(name.as_str()).or_insert((root, entry));
            }
        }
        visible
    }

    /// Every loaded skill, name-ordered, without bodies.
    pub(crate) fn infos(&self) -> Vec<SkillInfo> {
        self.visible()
            .into_iter()
            .map(|(name, (root, entry))| SkillInfo {
                name: name.to_string(),
                description: entry.description.clone(),
                model_invocable: entry.model_invocable,
                path: entry.path.clone(),
                root: root.registered.clone(),
            })
            .collect()
    }

    /// The skill list shown to the model.
    ///
    /// Skills whose frontmatter disabled model invocation are left out: naming
    /// one here and refusing it in `load_skill` would be an invitation
    /// followed by a refusal.
    pub(crate) fn get_descriptions(&self) -> String {
        let invocable = self
            .visible()
            .into_iter()
            .filter(|(_, (_, entry))| entry.model_invocable)
            .collect::<Vec<_>>();
        if invocable.is_empty() {
            return String::new();
        }

        let mut lines = vec!["Skills available:".to_string()];
        for (name, (_, entry)) in invocable {
            lines.push(format!("  - {name}: {}", entry.description));
        }
        lines.push(
            "Use the load_skill tool only when one of these skills is relevant to the task."
                .to_string(),
        );
        lines.join("\n")
    }

    /// Returns a skill's body regardless of whether the model may invoke it.
    ///
    /// The host-side counterpart to [`get_content`](Self::get_content): a
    /// skill marked `disable-model-invocation` is refused there and returned
    /// here, which is the whole point of the flag.
    pub(crate) fn get_body(&self, name: &str) -> Result<String, String> {
        let (_, entry) = self.lookup(name)?;
        Ok(render_skill(name, &entry.body))
    }

    /// Returns a skill's body for the model, refusing one it may not invoke.
    pub(crate) fn get_content(&self, name: &str) -> Result<String, String> {
        let (_, entry) = self.lookup(name)?;
        if !entry.model_invocable {
            return Err(format!("Skill '{name}' cannot be invoked by the model"));
        }
        Ok(render_skill(name, &entry.body))
    }

    fn lookup(&self, name: &str) -> Result<(&SkillRoot, &SkillEntry), String> {
        if self.roots.is_empty() {
            return Err(NO_LOADER.to_string());
        }
        self.resolve(name)
            .ok_or_else(|| format!("Unknown skill '{name}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::skill::tests::{temp_skills_dir, write_skill};

    fn registry(paths: &[&Path]) -> SkillRegistry {
        let roots = paths
            .iter()
            .map(|path| SkillRoot::load(path).expect("root loads"))
            .collect();
        let mut registry = SkillRegistry::default();
        registry.insert(roots);
        registry
    }

    #[test]
    fn the_earliest_root_wins_a_name_and_the_others_still_load() {
        let strong = temp_skills_dir("registry-strong");
        write_skill(&strong, "review", "---\nname: review\n---\nProject rules\n");
        let weak = temp_skills_dir("registry-weak");
        write_skill(&weak, "review", "---\nname: review\n---\nPersonal rules\n");
        write_skill(&weak, "deploy", "---\nname: deploy\n---\nPersonal deploy\n");

        let registry = registry(&[&strong, &weak]);

        assert!(
            registry
                .get_content("review")
                .expect("review resolves")
                .contains("Project rules")
        );
        assert!(registry.get_content("deploy").is_ok());
    }

    #[test]
    fn removing_the_winner_reveals_the_skill_it_shadowed() {
        // The property a destructive merge cannot express: shadowing outranks
        // a skill, it does not delete it.
        let strong = temp_skills_dir("registry-restore-strong");
        write_skill(&strong, "review", "---\nname: review\n---\nProject rules\n");
        let weak = temp_skills_dir("registry-restore-weak");
        write_skill(&weak, "review", "---\nname: review\n---\nPersonal rules\n");

        let mut registry = registry(&[&strong, &weak]);
        assert!(registry.remove(&strong));

        assert!(
            registry
                .get_content("review")
                .expect("the weaker root answers now")
                .contains("Personal rules")
        );
        assert_eq!(registry.infos()[0].root, weak);
    }

    #[test]
    fn removing_the_last_root_leaves_nothing_reachable() {
        let root = temp_skills_dir("registry-empty");
        write_skill(&root, "one", "---\nname: one\n---\nBody\n");

        let mut registry = registry(&[&root]);
        assert!(registry.remove(&root));

        assert!(registry.is_empty());
        assert!(registry.infos().is_empty());
        assert_eq!(registry.get_descriptions(), "");
        assert_eq!(registry.get_content("one"), Err(NO_LOADER.to_string()));
        assert!(!registry.remove(&root), "a second removal finds nothing");
    }

    #[test]
    fn an_unknown_name_is_unknown_rather_than_unavailable() {
        let root = temp_skills_dir("registry-unknown");
        write_skill(&root, "one", "---\nname: one\n---\nBody\n");

        let registry = registry(&[&root]);

        assert_eq!(
            registry.get_body("two"),
            Err("Unknown skill 'two'".to_string())
        );
    }

    #[test]
    fn a_root_is_matched_by_canonical_path() {
        let root = temp_skills_dir("registry-canonical");
        write_skill(&root, "one", "---\nname: one\n---\nBody\n");

        let mut registry = registry(&[&root]);

        assert!(registry.remove(root.join("one").join("..")));
    }

    #[test]
    fn re_registering_a_root_replaces_its_entry_in_place() {
        let strong = temp_skills_dir("registry-reload-strong");
        write_skill(&strong, "review", "---\nname: review\n---\nFirst\n");
        let weak = temp_skills_dir("registry-reload-weak");
        write_skill(&weak, "review", "---\nname: review\n---\nPersonal\n");

        let mut registry = registry(&[&strong, &weak]);
        write_skill(&strong, "review", "---\nname: review\n---\nSecond\n");
        registry.insert(vec![SkillRoot::load(&strong).expect("reloads")]);

        assert_eq!(registry.roots.len(), 2, "no stacked copy of the same root");
        assert!(
            registry
                .get_content("review")
                .expect("review resolves")
                .contains("Second"),
            "the reloaded root kept its precedence"
        );
        assert!(registry.remove(&strong));
        assert!(
            registry
                .get_content("review")
                .expect("review resolves")
                .contains("Personal")
        );
    }

    #[test]
    fn infos_name_the_file_and_the_root_without_carrying_bodies() {
        let strong = temp_skills_dir("registry-infos-strong");
        write_skill(
            &strong,
            "review",
            "---\nname: review\ndescription: D1\n---\nSECRET BODY\n",
        );
        let weak = temp_skills_dir("registry-infos-weak");
        write_skill(
            &weak,
            "deploy",
            "---\nname: deploy\ndescription: D2\n---\nB\n",
        );

        let infos = registry(&[&strong, &weak]).infos();

        assert_eq!(infos.len(), 2);
        // Name-ordered, so `deploy` precedes `review`.
        assert_eq!(infos[0].name, "deploy");
        assert_eq!(infos[0].description, "D2");
        assert_eq!(infos[0].path, weak.join("deploy").join("SKILL.md"));
        assert_eq!(infos[0].root, weak);
        assert_eq!(infos[1].name, "review");
        assert_eq!(infos[1].root, strong);
        assert!(
            !format!("{infos:?}").contains("SECRET BODY"),
            "descriptions stay cheap: bodies arrive only through load_skill"
        );
    }

    #[test]
    fn descriptions_leave_out_skills_the_model_may_not_invoke() {
        let root = temp_skills_dir("registry-descriptions");
        write_skill(
            &root,
            "release",
            "---\nname: release\ndescription: cuts a release\ndisable-model-invocation: true\n---\nSteps\n",
        );
        write_skill(
            &root,
            "git",
            "---\nname: git\ndescription: Git helpers\n---\nStep 1\n",
        );

        let registry = registry(&[&root]);

        assert_eq!(
            registry.get_descriptions(),
            "Skills available:\n  - git: Git helpers\nUse the load_skill tool only when one of these skills is relevant to the task."
        );
        assert!(registry.get_content("release").is_err());
        assert!(registry.get_body("release").is_ok());
    }
}
