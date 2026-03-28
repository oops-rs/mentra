#[path = "workspace/operations.rs"]
mod operations;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;

use crate::runtime::{RuntimeHandle, RuntimeHookEvent};

#[derive(Debug, Clone)]
enum OverlayEntry {
    File(Vec<u8>),
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Missing,
}

#[derive(Debug, Clone, Copy)]
struct ListTraversal<'a> {
    root: &'a Path,
    max_depth: usize,
    limit: usize,
}

#[derive(Debug, Clone)]
enum OriginalState {
    Missing,
    File(Vec<u8>),
}

pub(crate) struct WorkspaceEditor {
    agent_id: String,
    runtime: RuntimeHandle,
    base_dir: PathBuf,
    working_directory: PathBuf,
    overlay: BTreeMap<PathBuf, OverlayEntry>,
}

impl WorkspaceEditor {
    pub(crate) fn new(
        agent_id: String,
        runtime: RuntimeHandle,
        base_dir: PathBuf,
        working_directory: PathBuf,
    ) -> Self {
        let base_dir = canonicalize_existing_path(base_dir);
        let working_directory = canonicalize_existing_path(working_directory);
        Self {
            agent_id,
            runtime,
            base_dir,
            working_directory,
            overlay: BTreeMap::new(),
        }
    }

    pub(crate) fn commit(&self) -> Result<(), String> {
        if self.overlay.is_empty() {
            return Ok(());
        }

        let originals = self
            .overlay
            .keys()
            .map(|path| Ok((path.clone(), self.capture_original_state(path)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()?;

        let file_writes = self
            .overlay
            .iter()
            .filter_map(|(path, entry)| match entry {
                OverlayEntry::File(bytes) => Some((path, bytes.as_slice())),
                OverlayEntry::Deleted => None,
            })
            .collect::<Vec<_>>();
        let deletes = self
            .overlay
            .iter()
            .filter_map(|(path, entry)| match entry {
                OverlayEntry::Deleted => Some(path),
                OverlayEntry::File(_) => None,
            })
            .collect::<Vec<_>>();

        let result = (|| -> Result<(), String> {
            for (path, bytes) in file_writes {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("Failed to create directory '{}': {error}", parent.display())
                    })?;
                }
                let temp_path = temporary_path(path);
                fs::write(&temp_path, bytes).map_err(|error| {
                    format!("Failed to write '{}': {error}", temp_path.display())
                })?;
                replace_file(&temp_path, path)?;
            }

            for path in deletes {
                if path.exists() {
                    fs::remove_file(path).map_err(|error| {
                        format!("Failed to delete '{}': {error}", path.display())
                    })?;
                }
            }

            Ok(())
        })();

        if let Err(error) = result {
            let _ = self.rollback(&originals);
            return Err(error);
        }

        Ok(())
    }

    fn rollback(&self, originals: &BTreeMap<PathBuf, OriginalState>) -> Result<(), String> {
        for (path, original) in originals {
            match original {
                OriginalState::Missing => {
                    if path.exists() {
                        fs::remove_file(path).map_err(|error| {
                            format!("Failed to roll back '{}': {error}", path.display())
                        })?;
                    }
                }
                OriginalState::File(bytes) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(|error| {
                            format!(
                                "Failed to recreate directory '{}' during rollback: {error}",
                                parent.display()
                            )
                        })?;
                    }
                    let temp_path = temporary_path(path);
                    fs::write(&temp_path, bytes).map_err(|error| {
                        format!(
                            "Failed to write rollback temp file '{}': {error}",
                            temp_path.display()
                        )
                    })?;
                    replace_file(&temp_path, path)?;
                }
            }
        }
        Ok(())
    }

    fn resolve_path(&self, raw: &str) -> Result<PathBuf, String> {
        let candidate = PathBuf::from(raw);
        let path = if candidate.is_absolute() {
            candidate
        } else {
            self.working_directory.join(candidate)
        };
        normalize_path(path)
    }

    fn authorize_read(&self, path: &Path, action: &str) -> Result<PathBuf, String> {
        self.runtime
            .execution
            .policy
            .authorize_file_read(&self.base_dir, path)
            .inspect_err(|detail: &String| {
                let _ = self
                    .runtime
                    .emit_hook(RuntimeHookEvent::AuthorizationDenied {
                        agent_id: self.agent_id.clone(),
                        action: action.to_string(),
                        detail: detail.clone(),
                    });
            })
    }

    fn authorize_write(&self, path: &Path, action: &str) -> Result<PathBuf, String> {
        self.runtime
            .execution
            .policy
            .authorize_file_write(&self.base_dir, path)
            .inspect_err(|detail: &String| {
                let _ = self
                    .runtime
                    .emit_hook(RuntimeHookEvent::AuthorizationDenied {
                        agent_id: self.agent_id.clone(),
                        action: action.to_string(),
                        detail: detail.clone(),
                    });
            })
    }

    fn entry_kind(&self, path: &Path) -> Result<EntryKind, String> {
        if let Some(entry) = self.overlay.get(path) {
            return Ok(match entry {
                OverlayEntry::File(_) => EntryKind::File,
                OverlayEntry::Deleted => EntryKind::Missing,
            });
        }

        if self.has_live_descendant(path) {
            return Ok(EntryKind::Dir);
        }

        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(EntryKind::Dir),
            Ok(metadata) if metadata.is_file() => Ok(EntryKind::File),
            Ok(_) => Err(format!(
                "Path '{}' is not a regular file or directory",
                self.display_path(path)
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(EntryKind::Missing),
            Err(error) => Err(format!(
                "Failed to inspect '{}': {error}",
                self.display_path(path)
            )),
        }
    }

    fn has_live_descendant(&self, path: &Path) -> bool {
        self.overlay.iter().any(|(candidate, entry)| match entry {
            OverlayEntry::File(_) => candidate.starts_with(path) && candidate != path,
            OverlayEntry::Deleted => false,
        })
    }

    fn child_names(&self, dir: &Path) -> Result<Vec<String>, String> {
        let mut names = BTreeSet::new();

        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        format!(
                            "Failed to read directory '{}': {error}",
                            self.display_path(dir)
                        )
                    })?;
                    names.insert(entry.file_name().to_string_lossy().into_owned());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to read directory '{}': {error}",
                    self.display_path(dir)
                ));
            }
        }

        for path in self.overlay.keys() {
            if let Ok(relative) = path.strip_prefix(dir)
                && let Some(component) = relative.components().next()
                && let Component::Normal(name) = component
            {
                names.insert(name.to_string_lossy().into_owned());
            }
        }

        let filtered = names
            .into_iter()
            .filter(|name| {
                let child = dir.join(name);
                !matches!(self.entry_kind(&child), Ok(EntryKind::Missing))
            })
            .collect::<Vec<_>>();
        Ok(filtered)
    }

    fn collect_list_entries(
        &self,
        dir: &Path,
        current_depth: usize,
        traversal: ListTraversal<'_>,
        visited: &mut BTreeSet<PathBuf>,
        entries: &mut Vec<String>,
    ) -> Result<(), String> {
        if current_depth > traversal.max_depth || entries.len() >= traversal.limit {
            return Ok(());
        }
        if !self.mark_directory_visited(dir, visited)? {
            return Ok(());
        }

        for child_name in self.child_names(dir)? {
            if entries.len() >= traversal.limit {
                break;
            }

            let child = dir.join(&child_name);
            match self.entry_kind(&child)? {
                EntryKind::File => entries.push(format!(
                    "[file] {}",
                    self.display_relative_to(traversal.root, &child)
                )),
                EntryKind::Dir => {
                    entries.push(format!(
                        "[dir] {}",
                        self.display_relative_to(traversal.root, &child)
                    ));
                    self.collect_list_entries(
                        &child,
                        current_depth + 1,
                        traversal,
                        visited,
                        entries,
                    )?;
                }
                EntryKind::Missing => {}
            }
        }

        Ok(())
    }

    fn collect_search_matches(
        &self,
        dir: &Path,
        regex: &Regex,
        limit: usize,
        visited: &mut BTreeSet<PathBuf>,
        matches: &mut Vec<String>,
    ) -> Result<(), String> {
        if !self.mark_directory_visited(dir, visited)? {
            return Ok(());
        }

        for child_name in self.child_names(dir)? {
            if matches.len() >= limit {
                break;
            }

            let child = dir.join(&child_name);
            match self.entry_kind(&child)? {
                EntryKind::File => self.search_file(&child, regex, limit, matches)?,
                EntryKind::Dir => {
                    self.collect_search_matches(&child, regex, limit, visited, matches)?
                }
                EntryKind::Missing => {}
            }
        }
        Ok(())
    }

    fn mark_directory_visited(
        &self,
        dir: &Path,
        visited: &mut BTreeSet<PathBuf>,
    ) -> Result<bool, String> {
        let key = match fs::canonicalize(dir) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => dir.to_path_buf(),
            Err(error) => {
                return Err(format!(
                    "Failed to resolve directory '{}': {error}",
                    self.display_path(dir)
                ));
            }
        };
        Ok(visited.insert(key))
    }

    fn search_file(
        &self,
        path: &Path,
        regex: &Regex,
        limit: usize,
        matches: &mut Vec<String>,
    ) -> Result<(), String> {
        let content = self.load_text_file(path)?;
        for (index, line) in content.lines().enumerate() {
            if matches.len() >= limit {
                break;
            }
            if regex.is_match(line) {
                matches.push(format!(
                    "{}:{}: {}",
                    self.display_path(path),
                    index + 1,
                    line
                ));
            }
        }
        Ok(())
    }

    fn load_text_file(&self, path: &Path) -> Result<String, String> {
        let bytes = self.load_file_bytes(path)?;
        String::from_utf8(bytes)
            .map_err(|_| format!("Path '{}' is not valid UTF-8 text", self.display_path(path)))
    }

    fn load_file_bytes(&self, path: &Path) -> Result<Vec<u8>, String> {
        if let Some(entry) = self.overlay.get(path) {
            return match entry {
                OverlayEntry::File(bytes) => Ok(bytes.clone()),
                OverlayEntry::Deleted => {
                    Err(format!("Path '{}' does not exist", self.display_path(path)))
                }
            };
        }

        match fs::read(path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("Path '{}' does not exist", self.display_path(path)))
            }
            Err(error) => Err(format!(
                "Failed to read '{}': {error}",
                self.display_path(path)
            )),
        }
    }

    fn capture_original_state(&self, path: &Path) -> Result<OriginalState, String> {
        match fs::read(path) {
            Ok(bytes) => Ok(OriginalState::File(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(OriginalState::Missing)
            }
            Err(error) => Err(format!(
                "Failed to snapshot '{}': {error}",
                self.display_path(path)
            )),
        }
    }

    fn display_path(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.working_directory) {
            let rendered = relative.display().to_string();
            if rendered.is_empty() {
                ".".to_string()
            } else {
                normalize_display_path(rendered)
            }
        } else {
            normalize_display_path(path.display().to_string())
        }
    }

    fn display_relative_to(&self, root: &Path, path: &Path) -> String {
        if path == root {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        } else {
            path.strip_prefix(root)
                .map(|relative| normalize_display_path(relative.display().to_string()))
                .unwrap_or_else(|_| self.display_path(path))
        }
    }
}

fn normalize_display_path(path: String) -> String {
    path.replace('\\', "/")
}

fn normalize_path(path: PathBuf) -> Result<PathBuf, String> {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.is_absolute() {
                    return Err(format!(
                        "Path '{}' escapes the filesystem root",
                        path.display()
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    if !normalized.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    Ok(normalized)
}

fn temporary_path(path: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!(".{file_name}.mentra-tmp-{unique}"))
}

fn canonicalize_existing_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn replace_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("Failed to replace existing '{}': {error}", path.display()))?;
    }

    fs::rename(temp_path, path).map_err(|error| {
        format!(
            "Failed to rename '{}' into '{}': {error}",
            temp_path.display(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_rejects_parent_past_root() {
        let mut path = std::env::temp_dir();
        for _ in 0..10 {
            path.push("..");
        }
        path.push("escape");
        let error = normalize_path(path).expect_err("path should be rejected");
        assert!(error.contains("escapes the filesystem root"));
    }
}
