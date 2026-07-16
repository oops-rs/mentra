use regex::RegexBuilder;

use super::{BTreeSet, EntryKind, OverlayEntry, SearchOptions, TextEdit, WorkspaceEditor};
use crate::tool::files::schema::{FileOperation, InsertPosition};

impl WorkspaceEditor {
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

    pub(crate) fn read(
        &mut self,
        path: String,
        offset: usize,
        limit: usize,
    ) -> Result<String, String> {
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

    pub(crate) fn list(&self, path: String, depth: usize, limit: usize) -> Result<String, String> {
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
                if limit > 0 {
                    self.walk_entries(
                        &path,
                        1,
                        depth,
                        "files_list",
                        &mut visited,
                        &mut |child, kind| {
                            let label = match kind {
                                EntryKind::File => "file",
                                EntryKind::Dir => "dir",
                                EntryKind::Missing => return Ok(true),
                            };
                            entries.push(format!(
                                "[{label}] {}",
                                self.display_relative_to(&path, child)
                            ));
                            Ok(entries.len() < limit)
                        },
                    )?;
                }
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

    pub(crate) fn search(
        &self,
        path: String,
        pattern: &str,
        limit: usize,
    ) -> Result<String, String> {
        self.grep(path, pattern, SearchOptions::default(), limit)
    }

    pub(crate) fn grep(
        &self,
        path: String,
        pattern: &str,
        options: SearchOptions,
        limit: usize,
    ) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_read(&path, "files_search")?;
        let expression = if options.literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let regex = RegexBuilder::new(&expression)
            .case_insensitive(options.ignore_case)
            .multi_line(options.multiline)
            .dot_matches_new_line(options.multiline)
            .build()
            .map_err(|error| format!("Invalid regex pattern: {error}"))?;
        let kind = self.entry_kind(&path)?;
        if kind == EntryKind::Missing {
            return Err(format!(
                "Path '{}' does not exist",
                self.display_path(&path)
            ));
        }

        let mut matches = Vec::new();
        match kind {
            EntryKind::File => {
                if self.path_matches_glob(&path, &path, options.file_glob.as_deref()) {
                    self.search_file(&path, &regex, &options, limit, &mut matches)?;
                }
            }
            EntryKind::Dir => {
                self.collect_search_matches(&path, &regex, &options, limit, &mut matches)?
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

    pub(crate) fn glob(&self, path: String, pattern: &str, limit: usize) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_read(&path, "files_glob")?;
        let kind = self.entry_kind(&path)?;
        if kind == EntryKind::Missing {
            return Err(format!(
                "Path '{}' does not exist",
                self.display_path(&path)
            ));
        }

        let mut matches = Vec::new();
        if limit > 0 {
            match kind {
                EntryKind::File => {
                    if self.path_matches_glob(&path, &path, Some(pattern)) {
                        matches.push(self.display_path(&path));
                    }
                }
                EntryKind::Dir => {
                    self.collect_glob_matches(&path, pattern, limit, &mut matches)?;
                }
                EntryKind::Missing => unreachable!(),
            }
        }

        let body = if matches.is_empty() {
            "(no matches)".to_string()
        } else {
            matches.join("\n")
        };
        Ok(format!(
            "glob {} /{pattern}/\n{}",
            self.display_path(&path),
            body
        ))
    }

    pub(crate) fn create(&mut self, path: String, content: String) -> Result<String, String> {
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

    pub(crate) fn set(&mut self, path: String, content: String) -> Result<String, String> {
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

    pub(crate) fn write(&mut self, path: String, content: String) -> Result<String, String> {
        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        match self.entry_kind(&path)? {
            EntryKind::Missing | EntryKind::File => {
                let byte_count = content.len();
                self.overlay
                    .insert(path.clone(), OverlayEntry::File(content.into_bytes()));
                Ok(format!(
                    "Wrote {byte_count} byte(s) to {}",
                    self.display_path(&path)
                ))
            }
            EntryKind::Dir => Err(format!(
                "Path '{}' is a directory",
                self.display_path(&path)
            )),
        }
    }

    pub(crate) fn replace(
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
        let outcome = self.edit_with_expected(
            path,
            vec![TextEdit {
                old_string: old.to_string(),
                new_string: new.to_string(),
            }],
            replace_all,
            Some(&[expected_replacements]),
        )?;
        Ok(format!(
            "replace {} ({actual_replacements} replacement{})",
            outcome.display_path,
            if outcome.replacement_count == 1 {
                ""
            } else {
                "s"
            },
            actual_replacements = outcome.replacement_count,
        ))
    }

    pub(crate) fn insert(
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

    pub(crate) fn move_path(&mut self, from: String, to: String) -> Result<String, String> {
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

    pub(crate) fn delete(&mut self, path: String) -> Result<String, String> {
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
}
