use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

use super::{EntryKind, OverlayEntry, WorkspaceEditor};

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextEdit {
    pub(crate) old_string: String,
    pub(crate) new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditOutcome {
    pub(crate) display_path: String,
    pub(crate) replacement_count: usize,
    pub(crate) diff: String,
    pub(crate) patch: String,
    pub(crate) first_changed_line: usize,
}

#[derive(Debug, Clone)]
struct PlannedReplacement {
    start: usize,
    end: usize,
    replacement: String,
    edit_index: usize,
}

#[derive(Debug, Clone, Copy)]
struct LineSpan<'a> {
    start: usize,
    content_end: usize,
    full_end: usize,
    text: &'a str,
}

#[derive(Debug, Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
}

impl WorkspaceEditor {
    pub(crate) fn edit(
        &mut self,
        path: String,
        edits: Vec<TextEdit>,
        replace_all: bool,
    ) -> Result<EditOutcome, String> {
        if edits.is_empty() {
            return Err("At least one edit is required".to_string());
        }

        let path = self.resolve_path(&path)?;
        let path = self.authorize_write(&path, "files_write")?;
        if self.entry_kind(&path)? != EntryKind::File {
            return Err(format!(
                "Path '{}' does not exist as a file",
                self.display_path(&path)
            ));
        }

        let bytes = self.load_file_bytes(&path)?;
        let has_bom = bytes.starts_with(UTF8_BOM);
        let text_bytes = if has_bom {
            &bytes[UTF8_BOM.len()..]
        } else {
            &bytes
        };
        let source = std::str::from_utf8(text_bytes).map_err(|_| {
            format!(
                "Path '{}' is not valid UTF-8 text",
                self.display_path(&path)
            )
        })?;
        let line_ending = if source.contains("\r\n") {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        let original = source.replace("\r\n", "\n");

        let mut replacements = Vec::new();
        for (edit_index, edit) in edits.iter().enumerate() {
            validate_edit(edit, edit_index)?;
            let mut ranges = exact_ranges(&original, &edit.old_string);
            let fuzzy = ranges.is_empty();
            if fuzzy {
                ranges = fuzzy_ranges(&original, &edit.old_string);
            }

            if ranges.is_empty() {
                return Err(format!(
                    "Edit {} old_string was not found in '{}'",
                    edit_index + 1,
                    self.display_path(&path)
                ));
            }
            if !replace_all && ranges.len() != 1 {
                return Err(format!(
                    "Edit {} old_string is not unique in '{}' ({} matches); set replace_all to replace every match",
                    edit_index + 1,
                    self.display_path(&path),
                    ranges.len()
                ));
            }

            let selected = if replace_all {
                ranges.as_slice()
            } else {
                &ranges[..1]
            };
            for &(start, end) in selected {
                let replacement = if fuzzy {
                    overlay_fuzzy_replacement(
                        &original[start..end],
                        &edit.old_string,
                        &edit.new_string,
                    )
                } else {
                    edit.new_string.clone()
                };
                replacements.push(PlannedReplacement {
                    start,
                    end,
                    replacement,
                    edit_index,
                });
            }
        }

        replacements.sort_by_key(|replacement| (replacement.start, replacement.end));
        for pair in replacements.windows(2) {
            if pair[1].start < pair[0].end {
                return Err(format!(
                    "Edits {} and {} overlap in '{}'",
                    pair[0].edit_index + 1,
                    pair[1].edit_index + 1,
                    self.display_path(&path)
                ));
            }
        }

        let mut updated = original.clone();
        for replacement in replacements.iter().rev() {
            updated.replace_range(replacement.start..replacement.end, &replacement.replacement);
        }
        if updated == original {
            return Err(format!(
                "Edits would not change '{}'",
                self.display_path(&path)
            ));
        }

        let display_path = self.display_path(&path);
        let first_changed_line = first_changed_line(&original, &updated);
        let (diff, patch) = build_diffs(&display_path, &original, &updated);
        let restored = restore_document(&updated, line_ending, has_bom);
        self.overlay.insert(path, OverlayEntry::File(restored));

        Ok(EditOutcome {
            display_path,
            replacement_count: replacements.len(),
            diff,
            patch,
            first_changed_line,
        })
    }
}

fn validate_edit(edit: &TextEdit, edit_index: usize) -> Result<(), String> {
    if edit.old_string.is_empty() {
        return Err(format!(
            "Edit {} old_string must not be empty",
            edit_index + 1
        ));
    }
    if edit.old_string == edit.new_string {
        return Err(format!(
            "Edit {} is a no-op because old_string and new_string are identical",
            edit_index + 1
        ));
    }
    Ok(())
}

fn exact_ranges(content: &str, needle: &str) -> Vec<(usize, usize)> {
    content
        .match_indices(needle)
        .map(|(start, _)| (start, start + needle.len()))
        .collect()
}

fn fuzzy_ranges(content: &str, needle: &str) -> Vec<(usize, usize)> {
    let content_lines = line_spans(content);
    let needle_has_trailing_newline = needle.ends_with('\n');
    let mut needle_lines = needle.split('\n').collect::<Vec<_>>();
    if needle_has_trailing_newline {
        needle_lines.pop();
    }
    if needle_lines.is_empty() || needle_lines.len() > content_lines.len() {
        return Vec::new();
    }
    let normalized_needle = needle_lines
        .iter()
        .map(|line| normalize_fuzzy_line(line))
        .collect::<Vec<_>>();

    content_lines
        .windows(normalized_needle.len())
        .enumerate()
        .filter_map(|(index, window)| {
            let equal = window
                .iter()
                .zip(&normalized_needle)
                .all(|(line, needle)| normalize_fuzzy_line(line.text) == *needle);
            if !equal {
                return None;
            }
            let last = window.last()?;
            if needle_has_trailing_newline && last.full_end == last.content_end {
                return None;
            }
            Some((
                content_lines[index].start,
                if needle_has_trailing_newline {
                    last.full_end
                } else {
                    last.content_end
                },
            ))
        })
        .collect()
}

fn line_spans(content: &str) -> Vec<LineSpan<'_>> {
    if content.is_empty() {
        return vec![LineSpan {
            start: 0,
            content_end: 0,
            full_end: 0,
            text: "",
        }];
    }

    let mut spans = Vec::new();
    let mut start = 0;
    for segment in content.split_inclusive('\n') {
        let full_end = start + segment.len();
        let text = segment.strip_suffix('\n').unwrap_or(segment);
        let content_end = start + text.len();
        spans.push(LineSpan {
            start,
            content_end,
            full_end,
            text,
        });
        start = full_end;
    }
    if start < content.len() {
        let text = &content[start..];
        spans.push(LineSpan {
            start,
            content_end: content.len(),
            full_end: content.len(),
            text,
        });
    }
    spans
}

fn normalize_fuzzy_line(line: &str) -> String {
    line.nfkc()
        .map(|character| match character {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            other => other,
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn overlay_fuzzy_replacement(matched: &str, old: &str, new: &str) -> String {
    let matched_lines = matched.split('\n').collect::<Vec<_>>();
    let old_lines = old.split('\n').collect::<Vec<_>>();
    let new_lines = new.split('\n').collect::<Vec<_>>();
    if matched_lines.len() != old_lines.len() || old_lines.len() != new_lines.len() {
        return new.to_string();
    }

    matched_lines
        .iter()
        .zip(old_lines.iter().zip(new_lines.iter()))
        .map(|(matched_line, (old_line, new_line))| {
            if normalize_fuzzy_line(old_line) == normalize_fuzzy_line(new_line) {
                (*matched_line).to_string()
            } else {
                (*new_line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn restore_document(content: &str, line_ending: LineEnding, has_bom: bool) -> Vec<u8> {
    let restored = match line_ending {
        LineEnding::Lf => content.to_string(),
        LineEnding::CrLf => content.replace('\n', "\r\n"),
    };
    let mut bytes = Vec::with_capacity(restored.len() + usize::from(has_bom) * UTF8_BOM.len());
    if has_bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(restored.as_bytes());
    bytes
}

fn first_changed_line(before: &str, after: &str) -> usize {
    let before = before.split('\n').collect::<Vec<_>>();
    let after = after.split('\n').collect::<Vec<_>>();
    let shared = before.len().min(after.len());
    before
        .iter()
        .zip(&after)
        .position(|(left, right)| left != right)
        .map_or(shared + 1, |index| index + 1)
}

fn build_diffs(path: &str, before: &str, after: &str) -> (String, String) {
    let diff = TextDiff::from_lines(before, after);
    let mut display = String::new();
    for change in diff.iter_all_changes() {
        let marker = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        display.push(marker);
        display.push_str(change.value());
        if change.missing_newline() {
            display.push('\n');
        }
    }
    let patch = diff.unified_diff().header(path, path).to_string();
    (display, patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_normalization_handles_nfkc_quotes_dashes_and_trailing_space() {
        assert_eq!(normalize_fuzzy_line("Ａ ‘quote’ —  "), "A 'quote' -");
    }

    #[test]
    fn fuzzy_overlay_preserves_unchanged_original_lines() {
        let matched = "let label = “hello”;  \nlet count = 1;";
        let old = "let label = \"hello\";\nlet count = 1;";
        let new = "let label = \"hello\";\nlet count = 2;";

        assert_eq!(
            overlay_fuzzy_replacement(matched, old, new),
            "let label = “hello”;  \nlet count = 2;"
        );
    }
}
