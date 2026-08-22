use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SkillLoader {
    skills: BTreeMap<String, SkillEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillEntry {
    description: String,
    body: String,
    path: PathBuf,
    model_invocable: bool,
}

/// A loaded skill, without its body.
///
/// Name and description are what a host needs to show a skill set to a person
/// — in a client UI, as protocol commands, in a run's log, or in a test
/// asserting the expected skills loaded. The body stays behind `load_skill`,
/// which is what keeps skills cheap in context: descriptions are always
/// present, bodies arrive only when asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// Whether the model may reach this skill.
    ///
    /// `false` when the `SKILL.md` frontmatter set `disable-model-invocation`:
    /// the skill is not listed to the model and `load_skill` refuses it, while
    /// a host driving skills itself still sees it here and can run it. That is
    /// the whole point of the flag — a skill a person invokes deliberately,
    /// never one a model reaches for on its own.
    pub model_invocable: bool,
    /// The `SKILL.md` this came from. With several roots registered, this is
    /// how a host tells which one won.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillLoadError {
    #[error("failed to read skills directory {path}: {message}")]
    ReadDir { path: PathBuf, message: String },
    #[error("failed to read skill file {path}: {message}")]
    ReadFile { path: PathBuf, message: String },
    #[error("invalid skill frontmatter in {path}: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },
    #[error("duplicate skill name '{name}' in {first_path} and {second_path}")]
    DuplicateSkillName {
        name: String,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    /// Keeps a skill out of the model's reach.
    ///
    /// Both spellings are accepted because the frontmatter convention uses
    /// hyphens and Rust callers reach for underscores; a skill author should
    /// not have to know which one this parser preferred.
    #[serde(
        default,
        rename = "disable-model-invocation",
        alias = "disable_model_invocation"
    )]
    disable_model_invocation: bool,
}

impl SkillLoader {
    pub(crate) fn from_dir(path: impl AsRef<Path>) -> Result<Self, SkillLoadError> {
        let root = path.as_ref().to_path_buf();
        let mut files = Vec::new();
        collect_skill_files(&root, &mut files)?;
        files.sort();

        let mut skills = BTreeMap::new();
        let mut skill_paths = BTreeMap::new();

        for file in files {
            let raw = fs::read_to_string(&file).map_err(|error| SkillLoadError::ReadFile {
                path: file.clone(),
                message: error.to_string(),
            })?;
            let (meta, body) = parse_skill_file(&file, &raw)?;

            let fallback_name = file
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .unwrap_or("skill");
            let name = meta
                .name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(fallback_name)
                .to_string();

            if let Some(first_path) = skill_paths.insert(name.clone(), file.clone()) {
                return Err(SkillLoadError::DuplicateSkillName {
                    name,
                    first_path,
                    second_path: file,
                });
            }

            let description = meta.description.unwrap_or_default().trim().to_string();
            skills.insert(
                name,
                SkillEntry {
                    description,
                    body,
                    path: file,
                    model_invocable: !meta.disable_model_invocation,
                },
            );
        }

        Ok(Self { skills })
    }

    /// Folds in skills from a lower-precedence root.
    ///
    /// A name already defined here wins, so roots registered earlier shadow
    /// later ones — the same rule `PATH` uses, and the one that lets a project
    /// override a personal skill by name. Within a single root a repeated name
    /// is still [`SkillLoadError::DuplicateSkillName`], because there it is a
    /// mistake rather than an intent.
    pub(crate) fn merge_weaker(&mut self, weaker: SkillLoader) {
        for (name, entry) in weaker.skills {
            self.skills.entry(name).or_insert(entry);
        }
    }

    /// Every loaded skill, name-ordered, without bodies.
    pub(crate) fn infos(&self) -> Vec<SkillInfo> {
        self.skills
            .iter()
            .map(|(name, entry)| SkillInfo {
                name: name.clone(),
                description: entry.description.clone(),
                model_invocable: entry.model_invocable,
                path: entry.path.clone(),
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
            .skills
            .iter()
            .filter(|(_, skill)| skill.model_invocable)
            .collect::<Vec<_>>();
        if invocable.is_empty() {
            return String::new();
        }

        let mut lines = vec!["Skills available:".to_string()];
        for (name, skill) in invocable {
            lines.push(format!("  - {name}: {}", skill.description));
        }
        lines.push(
            "Use the load_skill tool only when one of these skills is relevant to the task."
                .to_string(),
        );
        lines.join("\n")
    }

    pub(crate) fn get_content(&self, name: &str) -> Result<String, String> {
        let Some(skill) = self.skills.get(name) else {
            return Err(format!("Unknown skill '{name}'"));
        };
        if !skill.model_invocable {
            return Err(format!("Skill '{name}' cannot be invoked by the model"));
        }

        let body = skill.body.trim_end_matches(['\n', '\r']);
        Ok(format!("<skill name=\"{name}\">\n{body}\n</skill>"))
    }
}

fn collect_skill_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), SkillLoadError> {
    let entries = fs::read_dir(path).map_err(|error| SkillLoadError::ReadDir {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| SkillLoadError::ReadDir {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(|error| SkillLoadError::ReadDir {
            path: entry_path.clone(),
            message: error.to_string(),
        })?;

        if file_type.is_dir() {
            collect_skill_files(&entry_path, files)?;
        } else if file_type.is_file() && entry.file_name() == "SKILL.md" {
            files.push(entry_path);
        }
    }

    Ok(())
}

fn parse_skill_file(path: &Path, raw: &str) -> Result<(SkillFrontmatter, String), SkillLoadError> {
    let Some(opening_len) = raw
        .strip_prefix("---\r\n")
        .map(|_| 5)
        .or_else(|| raw.strip_prefix("---\n").map(|_| 4))
    else {
        return Ok((SkillFrontmatter::default(), raw.to_string()));
    };

    let rest = &raw[opening_len..];
    let mut cursor = 0usize;
    for segment in rest.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\n', '\r']);
        if line == "---" {
            let frontmatter = &rest[..cursor];
            let body = &rest[cursor + segment.len()..];
            let meta = serde_yaml_ng::from_str(frontmatter).map_err(|error| {
                SkillLoadError::InvalidFrontmatter {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                }
            })?;
            return Ok((meta, body.to_string()));
        }
        cursor += segment.len();
    }

    if rest[cursor..].trim_end_matches('\r') == "---" {
        let frontmatter = &rest[..cursor];
        let meta = serde_yaml_ng::from_str(frontmatter).map_err(|error| {
            SkillLoadError::InvalidFrontmatter {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        })?;
        return Ok((meta, String::new()));
    }

    Err(SkillLoadError::InvalidFrontmatter {
        path: path.to_path_buf(),
        message: "missing closing frontmatter delimiter".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{SkillLoadError, SkillLoader};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn a_skill_that_disables_model_invocation_is_neither_listed_nor_loadable() {
        // Listing it and then refusing it would be an invitation followed by a
        // refusal, so it is left out of the model's list entirely.
        let root = temp_skills_dir("disabled");
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

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        let descriptions = loader.get_descriptions();
        assert!(descriptions.contains("git: Git helpers"), "{descriptions}");
        assert!(!descriptions.contains("release"), "{descriptions}");

        let refused = loader
            .get_content("release")
            .expect_err("the model may not load it");
        assert!(
            refused.contains("cannot be invoked by the model"),
            "{refused}"
        );
        assert!(loader.get_content("git").is_ok());

        // A host still sees it, which is what makes it invocable by a person.
        let infos = loader.infos();
        let release = infos
            .iter()
            .find(|info| info.name == "release")
            .expect("still loaded");
        assert!(!release.model_invocable);
    }

    #[test]
    fn every_skill_disabled_leaves_no_list_at_all() {
        let root = temp_skills_dir("all-disabled");
        write_skill(
            &root,
            "release",
            "---\nname: release\ndescription: cuts a release\ndisable_model_invocation: true\n---\nSteps\n",
        );

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        assert_eq!(
            loader.get_descriptions(),
            "",
            "an empty list must not become a header with nothing under it"
        );
    }

    #[test]
    fn parses_frontmatter_and_strips_it_from_content() {
        let root = temp_skills_dir("frontmatter");
        write_skill(
            &root,
            "git",
            "---\nname: git\ndescription: Git helpers\n---\nStep 1\nStep 2\n",
        );

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        assert_eq!(
            loader.get_descriptions(),
            "Skills available:\n  - git: Git helpers\nUse the load_skill tool only when one of these skills is relevant to the task."
        );
        assert_eq!(
            loader.get_content("git").expect("git skill"),
            "<skill name=\"git\">\nStep 1\nStep 2\n</skill>"
        );
    }

    #[test]
    fn falls_back_to_directory_name_when_name_is_missing() {
        let root = temp_skills_dir("fallback-name");
        write_skill(
            &root,
            "pdf",
            "---\ndescription: Process PDFs\n---\nRead pages\n",
        );

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        assert!(loader.get_descriptions().contains("  - pdf: Process PDFs"));
        assert!(loader.get_content("pdf").is_ok());
    }

    #[test]
    fn renders_descriptions_in_sorted_order() {
        let root = temp_skills_dir("sorted");
        write_skill(
            &root,
            "b-skill",
            "---\nname: zebra\ndescription: Last\n---\nB\n",
        );
        write_skill(
            &root,
            "a-skill",
            "---\nname: alpha\ndescription: First\n---\nA\n",
        );

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        assert_eq!(
            loader.get_descriptions(),
            "Skills available:\n  - alpha: First\n  - zebra: Last\nUse the load_skill tool only when one of these skills is relevant to the task."
        );
    }

    #[test]
    fn rejects_duplicate_skill_names() {
        let root = temp_skills_dir("duplicate");
        write_skill(&root, "one", "---\nname: shared\n---\nA\n");
        write_skill(&root, "two", "---\nname: shared\n---\nB\n");

        let error = SkillLoader::from_dir(&root).expect_err("duplicate error");

        assert!(matches!(
            error,
            SkillLoadError::DuplicateSkillName { ref name, .. } if name == "shared"
        ));
    }

    #[test]
    fn rejects_malformed_frontmatter() {
        let root = temp_skills_dir("invalid-frontmatter");
        write_skill(&root, "broken", "---\nname: [oops\n---\nBody\n");

        let error = SkillLoader::from_dir(&root).expect_err("frontmatter error");

        assert!(matches!(error, SkillLoadError::InvalidFrontmatter { .. }));
        assert!(error.to_string().contains("invalid skill frontmatter"));
    }

    #[test]
    fn a_weaker_root_only_fills_in_names_the_stronger_one_lacks() {
        let strong = temp_skills_dir("merge-strong");
        write_skill(&strong, "review", "---\nname: review\n---\nProject rules\n");
        let weak = temp_skills_dir("merge-weak");
        write_skill(&weak, "review", "---\nname: review\n---\nPersonal rules\n");
        write_skill(&weak, "deploy", "---\nname: deploy\n---\nPersonal deploy\n");

        let mut loader = SkillLoader::from_dir(&strong).expect("strong root loads");
        loader.merge_weaker(SkillLoader::from_dir(&weak).expect("weak root loads"));

        assert!(
            loader
                .get_content("review")
                .expect("review present")
                .contains("Project rules"),
            "the stronger root must win a name collision"
        );
        assert!(
            loader.get_content("deploy").is_ok(),
            "a name only the weaker root defines must still load"
        );
    }

    #[test]
    fn merging_reports_which_file_each_skill_came_from() {
        let strong = temp_skills_dir("merge-infos-strong");
        write_skill(
            &strong,
            "review",
            "---\nname: review\ndescription: D1\n---\nA\n",
        );
        let weak = temp_skills_dir("merge-infos-weak");
        write_skill(
            &weak,
            "deploy",
            "---\nname: deploy\ndescription: D2\n---\nB\n",
        );

        let mut loader = SkillLoader::from_dir(&strong).expect("loads");
        loader.merge_weaker(SkillLoader::from_dir(&weak).expect("loads"));
        let infos = loader.infos();

        assert_eq!(infos.len(), 2);
        // Name-ordered, so `deploy` precedes `review`.
        assert_eq!(infos[0].name, "deploy");
        assert_eq!(infos[0].description, "D2");
        assert!(infos[0].path.starts_with(&weak));
        assert_eq!(infos[1].name, "review");
        assert_eq!(infos[1].description, "D1");
        assert!(infos[1].path.starts_with(&strong));
    }

    #[test]
    fn infos_omit_bodies() {
        let root = temp_skills_dir("infos-no-body");
        write_skill(
            &root,
            "one",
            "---\nname: one\ndescription: short\n---\nSECRET BODY\n",
        );

        let infos = SkillLoader::from_dir(&root).expect("loads").infos();

        let rendered = format!("{infos:?}");
        assert!(!rendered.contains("SECRET BODY"));
    }

    fn temp_skills_dir(label: &str) -> PathBuf {
        let unique = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mentra-skill-tests-{label}-{timestamp}-{unique}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_skill(root: &Path, name: &str, content: &str) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        fs::write(skill_dir.join("SKILL.md"), content).expect("write skill");
    }
}
