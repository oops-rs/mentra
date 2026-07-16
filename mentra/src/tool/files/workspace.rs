#[path = "workspace/edit.rs"]
mod edit;
#[path = "workspace/operations.rs"]
mod operations;

pub(crate) use edit::{EditOutcome, TextEdit};

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

#[derive(Debug, Clone)]
enum OriginalState {
    Missing,
    File(Vec<u8>),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SearchOptions {
    pub(crate) file_glob: Option<String>,
    pub(crate) ignore_case: bool,
    pub(crate) literal: bool,
    pub(crate) context: usize,
    pub(crate) multiline: bool,
    pub(crate) max_line_chars: Option<usize>,
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

        Ok(names.into_iter().collect())
    }

    fn walk_entries<F>(
        &self,
        dir: &Path,
        current_depth: usize,
        max_depth: usize,
        action: &str,
        visited: &mut BTreeSet<PathBuf>,
        visit: &mut F,
    ) -> Result<bool, String>
    where
        F: FnMut(&Path, EntryKind) -> Result<bool, String>,
    {
        if current_depth > max_depth {
            return Ok(true);
        }
        if !self.mark_directory_visited(dir, visited)? {
            return Ok(true);
        }

        for child_name in self.child_names(dir)? {
            let child = dir.join(&child_name);
            // The traversal root was authorized before recursion starts, but a
            // descendant may be a symlink whose target leaves every allowed
            // read root. Reauthorize each child immediately before inspecting
            // or following it so recursion cannot cross that boundary. Doing
            // this lazily also preserves the existing limit short-circuit.
            self.authorize_read(&child, action)?;
            let kind = self.entry_kind(&child)?;
            if kind == EntryKind::Missing {
                continue;
            }
            if !visit(&child, kind)? {
                return Ok(false);
            }
            if kind == EntryKind::Dir
                && !self.walk_entries(
                    &child,
                    current_depth + 1,
                    max_depth,
                    action,
                    visited,
                    visit,
                )?
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn collect_search_matches(
        &self,
        root: &Path,
        regex: &Regex,
        options: &SearchOptions,
        limit: usize,
        matches: &mut Vec<String>,
    ) -> Result<(), String> {
        let mut visited = BTreeSet::new();
        self.walk_entries(
            root,
            1,
            usize::MAX,
            "files_search",
            &mut visited,
            &mut |child, kind| {
                if kind == EntryKind::File
                    && self.path_matches_glob(root, child, options.file_glob.as_deref())
                {
                    self.search_file(child, regex, options, limit, matches)?;
                }
                Ok(matches.len() < limit)
            },
        )?;
        Ok(())
    }

    fn collect_glob_matches(
        &self,
        root: &Path,
        pattern: &str,
        limit: usize,
        matches: &mut Vec<String>,
    ) -> Result<(), String> {
        let mut visited = BTreeSet::new();
        self.walk_entries(
            root,
            1,
            usize::MAX,
            "files_glob",
            &mut visited,
            &mut |child, kind| {
                if kind == EntryKind::File && self.path_matches_glob(root, child, Some(pattern)) {
                    matches.push(self.display_relative_to(root, child));
                }
                Ok(matches.len() < limit)
            },
        )?;
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
        options: &SearchOptions,
        limit: usize,
        matches: &mut Vec<String>,
    ) -> Result<(), String> {
        let content = self.load_text_file(path)?;
        let lines = content.lines().collect::<Vec<_>>();
        if lines.is_empty() {
            return Ok(());
        }
        let mut matched_lines = BTreeSet::new();

        if options.multiline {
            let line_starts = line_start_offsets(&content);
            let last_line = lines.len() - 1;
            for found in regex.find_iter(&content) {
                let start_line = line_index_at(&line_starts, found.start()).min(last_line);
                let end_position = found.end().saturating_sub(1).max(found.start());
                let end_line = line_index_at(&line_starts, end_position).min(last_line);
                matched_lines.extend(start_line..=end_line);
            }
        } else {
            matched_lines.extend(
                lines
                    .iter()
                    .enumerate()
                    .filter_map(|(index, line)| regex.is_match(line).then_some(index)),
            );
        }

        let mut rendered_lines = BTreeSet::new();
        for index in &matched_lines {
            let start = index.saturating_sub(options.context);
            let end = index
                .saturating_add(options.context)
                .saturating_add(1)
                .min(lines.len());
            rendered_lines.extend(start..end);
        }

        for index in rendered_lines {
            if matches.len() >= limit {
                break;
            }
            let separator = if matched_lines.contains(&index) {
                ':'
            } else {
                '-'
            };
            matches.push(format!(
                "{}:{}{} {}",
                self.display_path(path),
                index + 1,
                separator,
                options.max_line_chars.map_or_else(
                    || lines[index].to_string(),
                    |max_chars| cap_search_line(lines[index], max_chars),
                )
            ));
        }
        Ok(())
    }

    fn path_matches_glob(&self, root: &Path, path: &Path, pattern: Option<&str>) -> bool {
        let Some(pattern) = pattern else {
            return true;
        };
        let relative = self.display_relative_to(root, path);
        glob_match::glob_match(pattern, &relative)
            || (!pattern.contains('/')
                && path
                    .file_name()
                    .is_some_and(|name| glob_match::glob_match(pattern, &name.to_string_lossy())))
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

fn line_start_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(
        content
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    offsets
}

fn line_index_at(line_starts: &[usize], byte_offset: usize) -> usize {
    line_starts
        .partition_point(|start| *start <= byte_offset)
        .saturating_sub(1)
}

fn cap_search_line(line: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = line.chars();
    let retained = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_none() {
        retained
    } else {
        let mut capped = retained.chars().take(max_chars - 1).collect::<String>();
        capped.push('…');
        capped
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

    fn test_editor(label: &str) -> (PathBuf, WorkspaceEditor) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("duration")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mentra-workspace-{label}-{unique}"));
        fs::create_dir_all(&root).expect("create test workspace");
        let editor = WorkspaceEditor::new(
            "agent".to_string(),
            RuntimeHandle::new(false),
            root.clone(),
            root.clone(),
        );
        (root, editor)
    }

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

    #[test]
    fn glob_walks_nested_files_with_workspace_relative_patterns() {
        let (root, editor) = test_editor("glob");
        fs::create_dir_all(root.join("src/nested")).expect("create nested directory");
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").expect("write lib");
        fs::write(root.join("src/nested/mod.rs"), "pub mod nested;\n").expect("write module");
        fs::write(root.join("src/note.txt"), "not rust\n").expect("write note");

        let output = editor.glob(".".to_string(), "**/*.rs", 20).expect("glob");

        assert!(output.contains("src/lib.rs"));
        assert!(output.contains("src/nested/mod.rs"));
        assert!(!output.contains("note.txt"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_limits_short_circuit_before_unvisited_descendants() {
        use std::os::unix::fs::symlink;

        let (root, editor) = test_editor("walk-limit");
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().expect("root name").to_string_lossy()
        ));
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(root.join("a.txt"), "match\n").expect("write first file");
        symlink(&outside, root.join("z_escape")).expect("create escape symlink");

        let list = editor.list(".".to_string(), 1, 1).expect("limited list");
        assert!(list.contains("[file] a.txt"));
        let search = editor
            .search(".".to_string(), "match", 1)
            .expect("limited search");
        assert!(search.contains("a.txt:1: match"));

        fs::remove_dir_all(root).expect("remove test workspace");
        fs::remove_dir_all(outside).expect("remove outside directory");
    }

    #[test]
    fn grep_supports_multiline_regex_and_context() {
        let (root, editor) = test_editor("multiline-grep");
        fs::write(
            root.join("multi.txt"),
            "before\nBEGIN\nmiddle\nEND\nafter\n",
        )
        .expect("write multiline file");

        let output = editor
            .grep(
                "multi.txt".to_string(),
                "BEGIN.*END",
                SearchOptions {
                    multiline: true,
                    context: 1,
                    ..Default::default()
                },
                20,
            )
            .expect("grep");

        assert!(output.contains("multi.txt:1- before"));
        assert!(output.contains("multi.txt:2: BEGIN"));
        assert!(output.contains("multi.txt:3: middle"));
        assert!(output.contains("multi.txt:4: END"));
        assert!(output.contains("multi.txt:5- after"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn grep_caps_each_physical_line_at_500_unicode_characters() {
        let (root, editor) = test_editor("grep-line-cap");
        fs::write(root.join("long.txt"), format!("{}\n", "界".repeat(600)))
            .expect("write long line");

        let output = editor
            .grep(
                "long.txt".to_string(),
                "界",
                SearchOptions {
                    max_line_chars: Some(500),
                    ..Default::default()
                },
                20,
            )
            .expect("grep");
        let rendered = output
            .lines()
            .find_map(|line| line.strip_prefix("long.txt:1: "))
            .expect("rendered match");

        assert_eq!(rendered.chars().count(), 500);
        assert!(rendered.ends_with('…'));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn legacy_batched_search_keeps_uncapped_matching_lines() {
        let (root, editor) = test_editor("legacy-search-line");
        fs::write(root.join("long.txt"), format!("{}\n", "界".repeat(600)))
            .expect("write long line");

        let output = editor
            .search("long.txt".to_string(), "界", 20)
            .expect("search");
        let rendered = output
            .lines()
            .find_map(|line| line.strip_prefix("long.txt:1: "))
            .expect("rendered match");

        assert_eq!(rendered.chars().count(), 600);
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn multiline_grep_bounds_zero_width_match_at_trailing_newline() {
        let (root, editor) = test_editor("grep-zero-width");
        fs::write(root.join("line.txt"), "line\n").expect("write line");

        let output = editor
            .grep(
                "line.txt".to_string(),
                "$",
                SearchOptions {
                    multiline: true,
                    ..Default::default()
                },
                20,
            )
            .expect("grep");

        assert!(output.contains("line.txt:1: line"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn grep_combines_literal_case_insensitive_and_file_glob_options() {
        let (root, editor) = test_editor("grep-options");
        fs::write(root.join("code.rs"), "Needle.[x]\n").expect("write Rust file");
        fs::write(root.join("note.txt"), "Needle.[x]\n").expect("write text file");

        let output = editor
            .grep(
                ".".to_string(),
                "needle.[X]",
                SearchOptions {
                    file_glob: Some("*.rs".to_string()),
                    ignore_case: true,
                    literal: true,
                    ..Default::default()
                },
                20,
            )
            .expect("grep");

        assert!(output.contains("code.rs:1: Needle.[x]"));
        assert!(!output.contains("note.txt"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn edit_restores_bom_and_crlf_and_reports_diff_metadata() {
        let (root, mut editor) = test_editor("edit-crlf");
        fs::write(
            root.join("note.txt"),
            b"\xEF\xBB\xBFfirst\r\nsecond\r\nthird\r\n",
        )
        .expect("write CRLF document");

        let outcome = editor
            .edit(
                "note.txt".to_string(),
                vec![TextEdit {
                    old_string: "second\r\n".to_string(),
                    new_string: "changed\r\n".to_string(),
                }],
                false,
            )
            .expect("edit");
        editor.commit().expect("commit edit");

        assert_eq!(outcome.replacement_count, 1);
        assert_eq!(outcome.first_changed_line, 2);
        assert!(outcome.diff.contains("-second"));
        assert!(outcome.diff.contains("+changed"));
        assert!(outcome.patch.contains("--- note.txt"));
        assert_eq!(
            fs::read(root.join("note.txt")).expect("read edited document"),
            b"\xEF\xBB\xBFfirst\r\nchanged\r\nthird\r\n"
        );
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn fuzzy_edit_normalizes_unicode_but_preserves_unchanged_original_lines() {
        let (root, mut editor) = test_editor("edit-fuzzy");
        fs::write(
            root.join("note.txt"),
            "let label = “Ａ—value”;  \nlet count = 1;\n",
        )
        .expect("write fuzzy document");

        editor
            .edit(
                "note.txt".to_string(),
                vec![TextEdit {
                    old_string: "let label = \"A-value\";\nlet count = 1;".to_string(),
                    new_string: "let label = \"A-value\";\nlet count = 2;".to_string(),
                }],
                false,
            )
            .expect("fuzzy edit");
        editor.commit().expect("commit edit");

        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read edited document"),
            "let label = “Ａ—value”;  \nlet count = 2;\n"
        );
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn multi_edit_matches_against_original_content_and_rejects_overlap() {
        let (root, mut editor) = test_editor("edit-original");
        fs::write(root.join("note.txt"), "alpha beta\n").expect("write document");

        editor
            .edit(
                "note.txt".to_string(),
                vec![
                    TextEdit {
                        old_string: "alpha".to_string(),
                        new_string: "beta".to_string(),
                    },
                    TextEdit {
                        old_string: "beta".to_string(),
                        new_string: "gamma".to_string(),
                    },
                ],
                false,
            )
            .expect("non-overlapping edit");
        editor.commit().expect("commit edit");
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read edited document"),
            "beta gamma\n"
        );

        fs::write(root.join("overlap.txt"), "abcdef\n").expect("write overlap document");
        let error = editor
            .edit(
                "overlap.txt".to_string(),
                vec![
                    TextEdit {
                        old_string: "abcd".to_string(),
                        new_string: "one".to_string(),
                    },
                    TextEdit {
                        old_string: "cdef".to_string(),
                        new_string: "two".to_string(),
                    },
                ],
                false,
            )
            .expect_err("overlap must fail");
        assert!(error.contains("overlap"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn edit_rejects_ambiguous_and_no_op_replacements() {
        let (root, mut editor) = test_editor("edit-guards");
        fs::write(root.join("note.txt"), "same same\n").expect("write document");

        let ambiguous = editor
            .edit(
                "note.txt".to_string(),
                vec![TextEdit {
                    old_string: "same".to_string(),
                    new_string: "changed".to_string(),
                }],
                false,
            )
            .expect_err("ambiguous edit must fail");
        assert!(ambiguous.contains("not unique"));

        let no_op = editor
            .edit(
                "note.txt".to_string(),
                vec![TextEdit {
                    old_string: "same".to_string(),
                    new_string: "same".to_string(),
                }],
                true,
            )
            .expect_err("no-op edit must fail");
        assert!(no_op.contains("no-op"));
        fs::remove_dir_all(root).expect("remove test workspace");
    }

    #[test]
    fn legacy_batched_replace_keeps_its_first_match_contract() {
        let (root, mut editor) = test_editor("legacy-replace");
        fs::write(root.join("note.txt"), "same same\n").expect("write document");

        let summary = editor
            .replace("note.txt".to_string(), "same", "changed", false, 2)
            .expect("legacy replace");
        editor.commit().expect("commit replace");

        assert_eq!(summary, "replace note.txt (2 replacements)");
        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("read document"),
            "changed same\n"
        );
        fs::remove_dir_all(root).expect("remove test workspace");
    }
}
