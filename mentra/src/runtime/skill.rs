mod registry;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

pub(crate) use registry::{SkillRegistry, SkillRoot};

/// Returns the identity Mentra uses to match a registered skills root.
///
/// An existing path is canonicalized, so different spellings and symlinks to
/// the same directory share one identity. When the filesystem cannot resolve
/// the path, it is returned unchanged; this preserves the registry's fallback
/// for a root that has already been deleted.
///
/// A host that counts several holders of one root should capture this key
/// while the root exists and retain it for release rather than recomputing it
/// after deletion. The returned key can be passed to
/// [`Runtime::unregister_skills_dir`](crate::Runtime::unregister_skills_dir).
/// This is registry identity only, not a filesystem authorization check.
#[must_use]
pub fn skill_root_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Every skill found under one root, keyed by name.
///
/// A loader is the parse result for a single directory and nothing more. Which
/// root a skill came from, and which root wins a name two roots both define,
/// is [`SkillRegistry`]'s to answer — keeping the two apart is what makes a
/// root removable.
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
///
/// Fields are added here as the runtime learns to say more about a skill, so
/// this is `#[non_exhaustive]`: match it with `..` and read it rather than
/// building one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The registered skills root this skill was loaded from, exactly as it
    /// was passed to `register_skills_dir`.
    ///
    /// A host that registers one root per workspace uses this to attribute a
    /// skill back to its workspace, and to hand the same path to
    /// `unregister_skills_dir` when that workspace closes.
    pub root: PathBuf,
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

    /// The entry a name resolves to within this one root.
    fn entry(&self, name: &str) -> Option<&SkillEntry> {
        self.skills.get(name)
    }

    /// Every entry this root defines, name-ordered.
    fn entries(&self) -> impl Iterator<Item = (&String, &SkillEntry)> {
        self.skills.iter()
    }
}

fn render_skill(name: &str, body: &str) -> String {
    let body = body.trim_end_matches(['\n', '\r']);
    format!("<skill name=\"{name}\">\n{body}\n</skill>")
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
pub(crate) mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{SkillLoadError, SkillLoader};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn frontmatter_decides_whether_the_model_may_reach_a_skill() {
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

        assert!(!loader.entry("release").expect("loaded").model_invocable);
        assert!(loader.entry("git").expect("loaded").model_invocable);
    }

    #[test]
    fn the_underscore_spelling_of_the_flag_is_accepted_too() {
        // A skill author should not have to know which spelling this parser
        // preferred.
        let root = temp_skills_dir("all-disabled");
        write_skill(
            &root,
            "release",
            "---\nname: release\ndescription: cuts a release\ndisable_model_invocation: true\n---\nSteps\n",
        );

        let loader = SkillLoader::from_dir(&root).expect("load skills");

        assert!(!loader.entry("release").expect("loaded").model_invocable);
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

        let entry = loader.entry("git").expect("git skill");
        assert_eq!(entry.description, "Git helpers");
        assert_eq!(entry.body, "Step 1\nStep 2\n");
        assert_eq!(entry.path, root.join("git").join("SKILL.md"));
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

        assert!(loader.entry("pdf").is_some());
    }

    #[test]
    fn entries_are_name_ordered_regardless_of_directory_names() {
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

        let names: Vec<&str> = loader.entries().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
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

    pub(crate) fn temp_skills_dir(label: &str) -> PathBuf {
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

    pub(crate) fn write_skill(root: &Path, name: &str, content: &str) {
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        fs::write(skill_dir.join("SKILL.md"), content).expect("write skill");
    }
}
