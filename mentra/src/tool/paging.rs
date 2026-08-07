//! Windowing for oversized tool results.
//!
//! An agent loop cannot control how much a tool returns, so a single
//! oversized result can overflow the model's context before the run can
//! react. This module computes the *model's view* of such a result: the first
//! window plus a trailer naming the follow-up call that returns the next one.
//! The full text is retained separately (see [`PagedToolResults`]) so nothing
//! is lost — paging never discards, it defers.
//!
//! Line numbers in every trailer are **absolute over the full result**, so a
//! line the model quotes from window three means the same line it would have
//! meant in an unpaged result.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::agent::ToolResultPagingConfig;

/// Name of the built-in tool that returns further windows. Referenced by the
/// paging trailer, so the two must always agree.
pub(crate) const READ_TOOL_RESULT_TOOL: &str = "read_tool_result";

/// Computes windows over a tool result according to an agent's paging
/// configuration. Holds no state: every window is derived from the full text
/// it is handed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolResultPager {
    threshold_bytes: usize,
    page_bytes: usize,
}

/// One window of a full result, before the trailer is appended.
struct Window<'a> {
    text: &'a str,
    /// 1-based absolute line this window starts at; equals `last_line + 1`
    /// when the window is empty because `start_line` was past the end.
    first_line: usize,
    /// 1-based absolute line this window ends at, or `first_line - 1` when
    /// the window is empty.
    last_line: usize,
    total_lines: usize,
    /// Set when a single line exceeded `page_bytes` and had to be cut mid-line:
    /// `(line number, bytes shown, bytes in the whole line)`.
    hard_cut: Option<(usize, usize, usize)>,
}

impl ToolResultPager {
    pub(crate) fn new(config: ToolResultPagingConfig) -> Self {
        Self {
            threshold_bytes: config.threshold_bytes,
            // A zero-byte page would emit nothing but markers forever; one
            // byte still guarantees forward progress line by line.
            page_bytes: config.page_bytes.max(1),
        }
    }

    /// Returns the first window when `text` is oversized, or `None` when it
    /// is at or below the threshold and must be inserted unchanged.
    pub(crate) fn first_page(&self, tool_use_id: &str, text: &str) -> Option<String> {
        if text.len() <= self.threshold_bytes {
            return None;
        }
        Some(self.window(tool_use_id, text, 1))
    }

    /// Returns the window starting at the 1-based absolute `start_line`,
    /// terminated by either a paging trailer or the end-of-result marker. A
    /// `start_line` past the end yields an empty window and the end marker.
    pub(crate) fn window(&self, tool_use_id: &str, text: &str, start_line: usize) -> String {
        let window = self.cut(text, start_line.max(1));
        let mut rendered = String::with_capacity(window.text.len() + TRAILER_HEADROOM_BYTES);
        rendered.push_str(window.text);
        if !rendered.is_empty() && !rendered.ends_with('\n') {
            rendered.push('\n');
        }

        if let Some((line, shown, total)) = window.hard_cut {
            rendered.push_str(&format!(
                "…[line {} hard-cut at {} of {} bytes; the remainder of this line is skipped]\n",
                thousands(line),
                thousands(shown),
                thousands(total),
            ));
        }

        if window.last_line >= window.total_lines {
            rendered.push_str(END_OF_RESULT_MARKER);
            return rendered;
        }

        rendered.push_str(&format!(
            "…[paged: lines {}–{} of {} ({} KB of {} KB). \
             Call {READ_TOOL_RESULT_TOOL}(tool_use_id=\"{tool_use_id}\", start_line={}) \
             for the next window.]",
            thousands(window.first_line),
            thousands(window.last_line),
            thousands(window.total_lines),
            kilobytes(window.text.len()),
            kilobytes(text.len()),
            thousands(window.last_line + 1),
        ));
        rendered
    }

    /// Selects the slice of `text` that starts at `start_line` and fits in
    /// `page_bytes`, always ending on a line boundary unless a single line is
    /// itself too long to fit.
    fn cut<'a>(&self, text: &'a str, start_line: usize) -> Window<'a> {
        let total_lines = text.split_inclusive('\n').count();
        if start_line > total_lines {
            return Window {
                text: "",
                first_line: start_line,
                last_line: total_lines,
                total_lines,
                hard_cut: None,
            };
        }

        let start = text
            .split_inclusive('\n')
            .take(start_line - 1)
            .map(str::len)
            .sum::<usize>();
        let mut shown = 0_usize;
        let mut lines = 0_usize;
        for line in text[start..].split_inclusive('\n') {
            if shown + line.len() > self.page_bytes {
                break;
            }
            shown += line.len();
            lines += 1;
        }

        if lines == 0 {
            // The line at `start_line` alone exceeds a whole page: the only
            // case where a window may end mid-line. Cut on a character
            // boundary so the window is always valid UTF-8, and resume at the
            // next line — a partial line has no addressable start.
            let line = text[start..]
                .split_inclusive('\n')
                .next()
                .expect("start_line is within the result, so a line follows");
            let mut end = self.page_bytes.min(line.len());
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            return Window {
                text: &line[..end],
                first_line: start_line,
                last_line: start_line,
                total_lines,
                hard_cut: Some((start_line, end, line.len())),
            };
        }

        Window {
            text: &text[start..start + shown],
            first_line: start_line,
            last_line: start_line + lines - 1,
            total_lines,
            hard_cut: None,
        }
    }
}

/// Full texts of this agent's paged tool results, keyed by `tool_use_id`.
///
/// Entries are immutable once recorded and are only ever read back whole;
/// the map lives for the life of the agent and is dropped with it. Nothing
/// here is persisted: the pager serves the live run, and the transcript
/// already records what the model actually saw.
#[derive(Clone, Default)]
pub(crate) struct PagedToolResults {
    entries: Arc<Mutex<HashMap<String, Arc<str>>>>,
}

impl PagedToolResults {
    pub(crate) fn record(&self, tool_use_id: &str, full: &str) {
        self.entries
            .lock()
            .expect("paged tool results poisoned")
            .insert(tool_use_id.to_string(), Arc::from(full));
    }

    pub(crate) fn get(&self, tool_use_id: &str) -> Option<Arc<str>> {
        self.entries
            .lock()
            .expect("paged tool results poisoned")
            .get(tool_use_id)
            .cloned()
    }
}

const END_OF_RESULT_MARKER: &str = "…[end of result]";

/// Slack reserved for the trailer when sizing the rendered window buffer.
const TRAILER_HEADROOM_BYTES: usize = 256;

fn kilobytes(bytes: usize) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

/// Formats a count with thousands separators, matching the trailer format
/// (`lines 1–812 of 5,723`).
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager(threshold_bytes: usize, page_bytes: usize) -> ToolResultPager {
        ToolResultPager::new(ToolResultPagingConfig {
            threshold_bytes,
            page_bytes,
        })
    }

    /// 26 lines of exactly 10 bytes each ("line-01xx\n" … ) = 260 bytes.
    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line-{line:02}xx\n"))
            .collect()
    }

    #[test]
    fn results_at_or_below_the_threshold_are_never_paged() {
        let text = numbered_lines(6);
        assert_eq!(text.len(), 60);

        assert_eq!(pager(60, 20).first_page("call-1", &text), None);
        assert!(pager(59, 20).first_page("call-1", &text).is_some());
    }

    #[test]
    fn the_first_page_carries_absolute_lines_and_byte_totals() {
        let text = numbered_lines(26);
        let page = pager(100, 30).first_page("call-8", &text).expect("paged");

        assert!(page.starts_with("line-01xx\nline-02xx\nline-03xx\n"));
        assert!(
            page.contains(
                "…[paged: lines 1–3 of 26 (0.0 KB of 0.3 KB). \
             Call read_tool_result(tool_use_id=\"call-8\", start_line=4) for the next window.]"
            ),
            "unexpected trailer: {page}"
        );
    }

    #[test]
    fn windows_tile_the_result_without_gaps_or_overlap() {
        let text = numbered_lines(26);
        let pager = pager(100, 30);

        let second = pager.window("call-8", &text, 4);
        assert!(second.starts_with("line-04xx\nline-05xx\nline-06xx\n"));
        assert!(second.contains("lines 4–6 of 26"));
        assert!(second.contains("start_line=7"));
    }

    #[test]
    fn the_final_window_carries_the_end_marker_instead_of_a_trailer() {
        let text = numbered_lines(26);
        let last = pager(100, 30).window("call-8", &text, 25);

        assert!(last.starts_with("line-25xx\nline-26xx\n"));
        assert!(last.ends_with("…[end of result]"));
        assert!(!last.contains("[paged:"));
    }

    #[test]
    fn a_start_line_past_the_end_returns_an_empty_window() {
        let text = numbered_lines(26);

        assert_eq!(
            pager(100, 30).window("call-8", &text, 27),
            "…[end of result]"
        );
        assert_eq!(
            pager(100, 30).window("call-8", &text, 9_999),
            "…[end of result]"
        );
    }

    #[test]
    fn a_line_longer_than_a_page_hard_cuts_on_a_character_boundary() {
        // Four-byte characters, so every page_bytes that is not a multiple of
        // four must round down rather than split the character.
        let text = format!("{}\nnext line\n", "𝄞".repeat(10));
        let window = pager(10, 10).window("call-8", &text, 1);

        assert!(window.starts_with("𝄞𝄞"));
        assert!(!window.starts_with("𝄞𝄞𝄞"));
        assert!(window.contains("…[line 1 hard-cut at 8 of 41 bytes"));
        assert!(window.contains("start_line=2"));
    }

    #[test]
    fn the_window_after_a_hard_cut_resumes_at_the_next_whole_line() {
        let text = format!("{}\nnext line\n", "𝄞".repeat(10));
        let window = pager(10, 10).window("call-8", &text, 2);

        assert!(window.starts_with("next line\n"));
        assert!(window.ends_with("…[end of result]"));
    }

    #[test]
    fn windows_never_split_a_line_that_fits_and_preserve_crlf() {
        let text = "alpha\r\nbéta\r\ngamma\r\n";
        let window = pager(4, 8).window("call-8", text, 1);

        assert!(window.starts_with("alpha\r\n"));
        assert!(!window.contains("béta"));
        assert!(window.contains("lines 1–1 of 3"));
    }

    #[test]
    fn a_result_without_a_trailing_newline_still_ends_before_the_trailer() {
        let window = pager(4, 8).window("call-8", "alpha\nomega", 2);

        assert!(window.starts_with("omega\n"));
        assert!(window.ends_with("…[end of result]"));
    }

    #[test]
    fn thousands_separators_match_the_documented_trailer_format() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(812), "812");
        assert_eq!(thousands(5_723), "5,723");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn recorded_results_are_readable_by_tool_use_id_and_isolated_per_id() {
        let store = PagedToolResults::default();
        store.record("call-1", "first");
        store.record("call-2", "second");

        assert_eq!(store.get("call-1").as_deref(), Some("first"));
        assert_eq!(store.get("call-2").as_deref(), Some("second"));
        assert_eq!(store.get("call-3"), None);
    }
}
