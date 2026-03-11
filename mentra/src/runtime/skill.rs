use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{Display, Formatter},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SkillLoader {
    skills: BTreeMap<String, SkillEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillEntry {
    description: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillLoadError {
    ReadDir {
        path: PathBuf,
        message: String,
    },
    ReadFile {
        path: PathBuf,
        message: String,
    },
    InvalidFrontmatter {
        path: PathBuf,
        message: String,
    },
    DuplicateSkillName {
        name: String,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

impl Display for SkillLoadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillLoadError::ReadDir { path, message } => {
                write!(
                    f,
                    "Failed to read skills directory {}: {message}",
                    path.display()
                )
            }
            SkillLoadError::ReadFile { path, message } => {
                write!(f, "Failed to read skill file {}: {message}", path.display())
            }
            SkillLoadError::InvalidFrontmatter { path, message } => write!(
                f,
                "Invalid skill frontmatter in {}: {message}",
                path.display()
            ),
            SkillLoadError::DuplicateSkillName {
                name,
                first_path,
                second_path,
            } => write!(
                f,
                "Duplicate skill name '{name}' in {} and {}",
                first_path.display(),
                second_path.display()
            ),
        }
    }
}

impl Error for SkillLoadError {}

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
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
            skills.insert(name, SkillEntry { description, body });
        }

        Ok(Self { skills })
    }

    pub(crate) fn get_descriptions(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut lines = vec!["Skills available:".to_string()];
        for (name, skill) in &self.skills {
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
        assert!(error.to_string().contains("Invalid skill frontmatter"));
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
