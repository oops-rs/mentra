use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;

use crate::runtime::{RuntimeHandle, RuntimeHookEvent};

use super::schema::{FileOperation, InsertPosition};

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

    pub(crate) fn apply_operation(&mut self, operation: FileOperation) -> Result<String, String> {
        match operation {
            FileOperation::Read {
                path,
                offset,
                limit,
            } => self.read(path, offset.unwrap_or(1), limit.unwrap_or(2000)),
            FileOperation::List { path, depth, limit } => {
                self.list(path, depth.unwrap_or(1), limit.unwrap_or(200))
            }
            FileOperation::Search {
                path,
                pattern,
                limit,
            } => self.search(path, &pattern, limit.unwrap_or(200)),
            FileOperation::Create { path, content } => self.create(path, content),
            FileOperation::Set { path, content } => self.set(path, content),
            FileOperation::Replace {
                path,
                old,
                new,
                replace_all,
                expected_replacements,
            } => self.replace(
                path,
                &old,
                &new,
                replace_all.unwrap_or(false),
                expected_replacements.unwrap_or(1),
            ),
            FileOperation::Insert {
                path,
                anchor,
                position,
                content,
                occurrence,
            } => self.insert(path, &anchor, position, &content, occurrence),
            FileOperation::Move { from, to } => self.move_path(from, to),
            FileOperation::Delete { path } => self.delete(path),
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

    fn read(&mut self, path: String, offset: usize, limit: usize) -> Result<String, String> {
        if offset == 0 {
            return Err("read offset must be at least 1".to_string());
        }

        let path = self.resolve_path(&path)?;
        let path = self.authorize_read(&path, "files_read")?;
        let content = self.load_text_file(&path)?;
        let lines = content.lines().collect::<Vec<_>>();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());
        let numbered = lines[start..end]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("L{}: {}", start + index + 1, line))
            .collect::<Vec<_>>();

        let body = if numbered.is_empty() {
            "(no lines)".to_string()
        } else {
            numbered.join("\n")
        };
        Ok(format!("read {}\n{}", self.display_path(&path), body))
    }

    fn list(&self, path: String, depth: usize, limit: usize) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_read(&path, "files_list")?;
        let kind = self.entry_kind(&path)?;
        if kind == EntryKind::Missing {
            return Err(format!(
                "Path '{}' does not exist",
                self.display_path(&path)
            ));
        }

        let mut entries = Vec::new();
        match kind {
            EntryKind::File => {
                entries.push(format!("[file] {}", self.display_relative_to(&path, &path)));
            }
            EntryKind::Dir => {
                let mut visited = BTreeSet::new();
                self.collect_list_entries(
                    &path,
                    &path,
                    1,
                    depth,
                    limit,
                    &mut visited,
                    &mut entries,
                )?
            }
            EntryKind::Missing => unreachable!(),
        }

        let body = if entries.is_empty() {
            "(no entries)".to_string()
        } else {
            entries.join("\n")
        };
        Ok(format!("list {}\n{}", self.display_path(&path), body))
    }

    fn search(&self, path: String, pattern: &str, limit: usize) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_read(&path, "files_search")?;
        let regex =
            Regex::new(pattern).map_err(|error| format!("Invalid regex pattern: {error}"))?;
        let kind = self.entry_kind(&path)?;
        if kind == EntryKind::Missing {
            return Err(format!(
                "Path '{}' does not exist",
                self.display_path(&path)
            ));
        }

        let mut matches = Vec::new();
        match kind {
            EntryKind::File => self.search_file(&path, &regex, limit, &mut matches)?,
            EntryKind::Dir => {
                let mut visited = BTreeSet::new();
                self.collect_search_matches(&path, &regex, limit, &mut visited, &mut matches)?
            }
            EntryKind::Missing => unreachable!(),
        }

        let body = if matches.is_empty() {
            "(no matches)".to_string()
        } else {
            matches.join("\n")
        };
        Ok(format!(
            "search {} /{pattern}/\n{}",
            self.display_path(&path),
            body
        ))
    }

    fn create(&mut self, path: String, content: String) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        match self.entry_kind(&path)? {
            EntryKind::Missing => {
                self.overlay
                    .insert(path.clone(), OverlayEntry::File(content.into_bytes()));
                Ok(format!("create {}", self.display_path(&path)))
            }
            EntryKind::File | EntryKind::Dir => Err(format!(
                "Path '{}' already exists",
                self.display_path(&path)
            )),
        }
    }

    fn set(&mut self, path: String, content: String) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        if self.entry_kind(&path)? != EntryKind::File {
            return Err(format!(
                "Path '{}' does not exist as a file",
                self.display_path(&path)
            ));
        }

        self.overlay
            .insert(path.clone(), OverlayEntry::File(content.into_bytes()));
        Ok(format!("set {}", self.display_path(&path)))
    }

    fn replace(
        &mut self,
        path: String,
        old: &str,
        new: &str,
        replace_all: bool,
        expected_replacements: usize,
    ) -> Result<String, String> {
        if old.is_empty() {
            return Err("replace old text must not be empty".to_string());
        }

        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        let content = self.load_text_file(&path)?;
        let actual_replacements = content.match_indices(old).count();
        if actual_replacements != expected_replacements {
            return Err(format!(
                "Expected {expected_replacements} replacement(s) in '{}', found {actual_replacements}",
                self.display_path(&path)
            ));
        }

        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };

        self.overlay
            .insert(path.clone(), OverlayEntry::File(updated.into_bytes()));
        Ok(format!(
            "replace {} ({actual_replacements} replacement{})",
            self.display_path(&path),
            if actual_replacements == 1 { "" } else { "s" }
        ))
    }

    fn insert(
        &mut self,
        path: String,
        anchor: &str,
        position: InsertPosition,
        content: &str,
        occurrence: Option<usize>,
    ) -> Result<String, String> {
        if anchor.is_empty() {
            return Err("insert anchor must not be empty".to_string());
        }

        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        let current = self.load_text_file(&path)?;
        let locations = current
            .match_indices(anchor)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if locations.is_empty() {
            return Err(format!(
                "Anchor '{anchor}' was not found in '{}'",
                self.display_path(&path)
            ));
        }

        let insert_at = match occurrence {
            Some(occurrence) => {
                if occurrence == 0 {
                    return Err("insert occurrence must be at least 1".to_string());
                }
                locations.get(occurrence - 1).copied().ok_or_else(|| {
                    format!(
                        "Anchor occurrence {occurrence} was not found in '{}'",
                        self.display_path(&path)
                    )
                })?
            }
            None if locations.len() == 1 => locations[0],
            None => {
                return Err(format!(
                    "Anchor '{anchor}' is ambiguous in '{}' ({})",
                    self.display_path(&path),
                    locations.len()
                ));
            }
        };

        let insert_at = match position {
            InsertPosition::Before => insert_at,
            InsertPosition::After => insert_at + anchor.len(),
        };
        let updated = format!(
            "{}{}{}",
            &current[..insert_at],
            content,
            &current[insert_at..]
        );
        self.overlay
            .insert(path.clone(), OverlayEntry::File(updated.into_bytes()));
        Ok(format!("insert {}", self.display_path(&path)))
    }

    fn move_path(&mut self, from: String, to: String) -> Result<String, String> {
        let from = self.resolve_path(&from)?;
        let to = self.resolve_path(&to)?;
        let from = self.authorize_write(&from, "files_write")?;
        let to = self.authorize_write(&to, "files_write")?;

        if self.entry_kind(&from)? != EntryKind::File {
            return Err(format!(
                "Source '{}' does not exist as a file",
                self.display_path(&from)
            ));
        }
        if self.entry_kind(&to)? != EntryKind::Missing {
            return Err(format!(
                "Destination '{}' already exists",
                self.display_path(&to)
            ));
        }

        let bytes = self.load_file_bytes(&from)?;
        self.overlay.insert(from.clone(), OverlayEntry::Deleted);
        self.overlay.insert(to.clone(), OverlayEntry::File(bytes));
        Ok(format!(
            "move {} -> {}",
            self.display_path(&from),
            self.display_path(&to)
        ))
    }

    fn delete(&mut self, path: String) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        if self.entry_kind(&path)? != EntryKind::File {
            return Err(format!(
                "Path '{}' does not exist as a file",
                self.display_path(&path)
            ));
        }

        self.overlay.insert(path.clone(), OverlayEntry::Deleted);
        Ok(format!("delete {}", self.display_path(&path)))
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
            .policy
            .authorize_file_read(&self.base_dir, path)
            .map_err(|detail| {
                let _ = self
                    .runtime
                    .emit_hook(RuntimeHookEvent::AuthorizationDenied {
                        agent_id: self.agent_id.clone(),
                        action: action.to_string(),
                        detail: detail.clone(),
                    });
                detail
            })
    }

    fn authorize_write(&self, path: &Path, action: &str) -> Result<PathBuf, String> {
        self.runtime
            .policy
            .authorize_file_write(&self.base_dir, path)
            .map_err(|detail| {
                let _ = self
                    .runtime
                    .emit_hook(RuntimeHookEvent::AuthorizationDenied {
                        agent_id: self.agent_id.clone(),
                        action: action.to_string(),
                        detail: detail.clone(),
                    });
                detail
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
        root: &Path,
        dir: &Path,
        current_depth: usize,
        max_depth: usize,
        limit: usize,
        visited: &mut BTreeSet<PathBuf>,
        entries: &mut Vec<String>,
    ) -> Result<(), String> {
        if current_depth > max_depth || entries.len() >= limit {
            return Ok(());
        }
        if !self.mark_directory_visited(dir, visited)? {
            return Ok(());
        }

        for child_name in self.child_names(dir)? {
            if entries.len() >= limit {
                break;
            }

            let child = dir.join(&child_name);
            match self.entry_kind(&child)? {
                EntryKind::File => {
                    entries.push(format!("[file] {}", self.display_relative_to(root, &child)))
                }
                EntryKind::Dir => {
                    entries.push(format!("[dir] {}", self.display_relative_to(root, &child)));
                    self.collect_list_entries(
                        root,
                        &child,
                        current_depth + 1,
                        max_depth,
                        limit,
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
                rendered
            }
        } else {
            path.display().to_string()
        }
    }

    fn display_relative_to(&self, root: &Path, path: &Path) -> String {
        if path == root {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string())
        } else {
            path.strip_prefix(root)
                .map(|relative| relative.display().to_string())
                .unwrap_or_else(|_| self.display_path(path))
        }
    }
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
