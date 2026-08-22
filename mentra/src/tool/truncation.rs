use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use mentra_provider::ToolResultContent;

static NEXT_SPILL_ID: AtomicU64 = AtomicU64::new(1);

/// Byte budgets below this keep the head alone rather than splitting into two
/// windows too short to carry a line of context each.
const MIN_BYTES_FOR_TWO_WINDOWS: usize = 256;

/// Line budgets below this keep the head alone, for the same reason.
const MIN_LINES_FOR_TWO_WINDOWS: usize = 4;

pub(super) enum SpillBehavior {
    Enabled(PathBuf),
    Disabled(&'static str),
}

pub(super) struct ToolOutputLimiter {
    max_bytes: usize,
    max_lines: usize,
    spill: SpillBehavior,
}

impl ToolOutputLimiter {
    pub(super) fn new(max_bytes: usize, max_lines: usize, spill: SpillBehavior) -> Self {
        Self {
            max_bytes,
            max_lines,
            spill,
        }
    }

    pub(super) async fn apply(&self, content: ToolResultContent) -> ToolResultContent {
        match content {
            ToolResultContent::Text(text) => self.apply_text(text).await,
            ToolResultContent::Structured(value) => self.apply_structured(value).await,
        }
    }

    async fn apply_text(&self, text: String) -> ToolResultContent {
        let total_lines = line_count(&text);
        if text.len() <= self.max_bytes && total_lines <= self.max_lines {
            return ToolResultContent::Text(text);
        }

        // A budget with room for two windows is spent on both ends of the
        // output. What a command has to say is rarely all at the top: the
        // compile line that failed, the assertion that tripped and the stack it
        // unwound are the last thing written, and a head-only window is the one
        // that reliably misses them.
        let (head_bytes, head_lines, tail_bytes, tail_lines) = self.windows();

        let mut shown_bytes = 0_usize;
        let mut shown_lines = 0_usize;
        for line in text.split_inclusive('\n') {
            if shown_lines == head_lines || shown_bytes.saturating_add(line.len()) > head_bytes {
                break;
            }
            shown_bytes += line.len();
            shown_lines += 1;
        }

        // Walk back from the end while the tail stays inside its own budget and
        // never reaches back into what the head already showed.
        let mut tail_start = text.len();
        let mut kept_tail_lines = 0_usize;
        if tail_lines > 0 {
            for line in text.split_inclusive('\n').rev() {
                let start = tail_start - line.len();
                if start < shown_bytes
                    || kept_tail_lines == tail_lines
                    || (text.len() - start) > tail_bytes
                {
                    break;
                }
                tail_start = start;
                kept_tail_lines += 1;
            }
        }

        let mut truncated = text[..shown_bytes].to_string();
        if !truncated.is_empty() && !truncated.ends_with('\n') {
            truncated.push('\n');
        }
        let tail = text[tail_start..].to_string();
        let shown = shown_lines + kept_tail_lines;
        let spill = self.spill(text, "txt").await;
        truncated.push_str(&format!(
            "[truncated: showing {shown} of {total_lines} lines; {spill}]"
        ));
        if !tail.is_empty() {
            truncated.push('\n');
            truncated.push_str(&tail);
        }
        ToolResultContent::Text(truncated)
    }

    /// Splits the byte and line budgets into a head window and a tail window.
    ///
    /// A budget too small for two useful windows is spent entirely on the head:
    /// half of four lines, or of a couple hundred bytes, says less than the
    /// whole of it does.
    fn windows(&self) -> (usize, usize, usize, usize) {
        if self.max_bytes < MIN_BYTES_FOR_TWO_WINDOWS || self.max_lines < MIN_LINES_FOR_TWO_WINDOWS
        {
            return (self.max_bytes, self.max_lines, 0, 0);
        }

        let head_bytes = self.max_bytes / 2;
        let head_lines = self.max_lines / 2;
        (
            head_bytes,
            head_lines,
            self.max_bytes - head_bytes,
            self.max_lines - head_lines,
        )
    }

    async fn apply_structured(&self, value: serde_json::Value) -> ToolResultContent {
        let serialized = serde_json::to_string(&value)
            .expect("serde_json::Value always serializes to valid JSON");
        let total_lines = line_count(&serialized);
        if serialized.len() <= self.max_bytes && total_lines <= self.max_lines {
            return ToolResultContent::Structured(value);
        }

        let serialized_len = serialized.len();
        let spill = self.spill(serialized, "json").await;
        ToolResultContent::Text(format!(
            "[truncated: structured tool output is {serialized_len} bytes across {total_lines} lines; {spill}]"
        ))
    }

    async fn spill(&self, content: String, extension: &'static str) -> String {
        match &self.spill {
            SpillBehavior::Enabled(directory) => {
                let directory = directory.clone();
                match tokio::task::spawn_blocking(move || {
                    spill_file(&directory, extension, &content)
                })
                .await
                {
                    Ok(Ok(path)) => format!("full output at {}", path.display()),
                    Ok(Err(error)) => format!(
                        "full output could not be saved ({error}); increase the tool-result limits"
                    ),
                    Err(error) => format!(
                        "full output could not be saved (spill task failed: {error}); increase the tool-result limits"
                    ),
                }
            }
            SpillBehavior::Disabled(reason) => {
                format!("full output was not saved because {reason}")
            }
        }
    }
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
    }
}

fn spill_file(directory: &Path, extension: &str, content: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| {
        format!(
            "failed to create spill directory '{}': {error}",
            directory.display()
        )
    })?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    for _ in 0..16 {
        let id = NEXT_SPILL_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "tool-output-{}-{timestamp}-{id}.{extension}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(content.as_bytes()) {
                    let _ = fs::remove_file(&path);
                    return Err(format!("failed to write '{}': {error}", path.display()));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to create '{}': {error}", path.display())),
        }
    }

    Err("could not allocate a unique spill filename".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_spill(max_bytes: usize, max_lines: usize) -> ToolOutputLimiter {
        ToolOutputLimiter::new(
            max_bytes,
            max_lines,
            SpillBehavior::Disabled("spill is disabled for this test"),
        )
    }

    fn text(content: ToolResultContent) -> String {
        match content {
            ToolResultContent::Text(text) => text,
            ToolResultContent::Structured(_) => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn under_limit_text_is_byte_identical() {
        let original = "alpha\r\nbéta\n".to_string();
        assert_eq!(
            no_spill(original.len(), 2)
                .apply(ToolResultContent::Text(original.clone()))
                .await,
            ToolResultContent::Text(original)
        );
    }

    #[tokio::test]
    async fn truncation_preserves_complete_crlf_and_utf8_lines() {
        let result = text(
            no_spill(10, 10)
                .apply(ToolResultContent::Text(
                    "alpha\r\nbéta\r\ngamma\r\n".to_string(),
                ))
                .await,
        );
        assert!(result.starts_with("alpha\r\n"));
        assert!(!result.contains("béta"));
        assert!(result.contains("showing 1 of 3 lines"));
    }

    #[tokio::test]
    async fn oversized_first_line_is_never_partially_emitted() {
        let result = text(
            no_spill(4, 10)
                .apply(ToolResultContent::Text("ééé\nnext".to_string()))
                .await,
        );
        assert!(result.starts_with("[truncated:"));
        assert!(result.contains("showing 0 of 2 lines"));
        assert!(!result.contains('é'));
    }

    #[tokio::test]
    async fn line_limit_preserves_the_requested_head() {
        let result = text(
            no_spill(usize::MAX, 2)
                .apply(ToolResultContent::Text("one\ntwo\nthree\n".to_string()))
                .await,
        );
        assert!(result.starts_with("one\ntwo\n[truncated:"));
        assert!(result.contains("showing 2 of 3 lines"));
    }

    #[tokio::test]
    async fn a_truncated_result_keeps_the_last_lines_too() {
        // The reason a build failed is on the last line of its output, not the
        // first. A budget with room for two windows spends it on both ends.
        let mut content = String::from("FIRST: compiling\n");
        for index in 0..500 {
            content.push_str(&format!("filler {index}\n"));
        }
        content.push_str("LAST: assertion failed\n");

        let result = text(
            no_spill(2_048, 40)
                .apply(ToolResultContent::Text(content))
                .await,
        );

        assert!(result.starts_with("FIRST: compiling\n"), "{result}");
        assert!(result.ends_with("LAST: assertion failed\n"), "{result}");
        assert!(result.contains("[truncated:"), "{result}");
        assert!(result.contains(" of 502 lines"), "{result}");
    }

    #[tokio::test]
    async fn structured_content_spills_whole_json_and_becomes_pointer_text() {
        let directory = std::env::temp_dir().join(format!(
            "mentra-tool-output-limiter-{}-{}",
            std::process::id(),
            NEXT_SPILL_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let limiter = ToolOutputLimiter::new(4, 10, SpillBehavior::Enabled(directory.clone()));
        let value = serde_json::json!({"answer": [1, 2, 3]});
        let pointer = text(
            limiter
                .apply(ToolResultContent::Structured(value.clone()))
                .await,
        );
        assert!(pointer.contains("structured tool output"));
        assert!(pointer.contains("full output at"));

        let files = fs::read_dir(&directory)
            .expect("read spill directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read spill entries");
        assert_eq!(files.len(), 1);
        let stored = fs::read_to_string(files[0].path()).expect("read spill file");
        assert_eq!(stored, serde_json::to_string(&value).unwrap());
        fs::remove_dir_all(directory).expect("remove spill directory");
    }

    #[tokio::test]
    async fn spill_failures_keep_text_and_structured_results_actionable() {
        let blocking_file = std::env::temp_dir().join(format!(
            "mentra-tool-output-blocker-{}-{}",
            std::process::id(),
            NEXT_SPILL_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&blocking_file, "not a directory").expect("create blocking file");
        let limiter = ToolOutputLimiter::new(
            4,
            10,
            SpillBehavior::Enabled(blocking_file.join("tool-output")),
        );

        let text_result = text(
            limiter
                .apply(ToolResultContent::Text("oversized text".to_string()))
                .await,
        );
        assert!(text_result.contains("full output could not be saved"));
        assert!(text_result.contains("increase the tool-result limits"));

        let structured_result = text(
            limiter
                .apply(ToolResultContent::Structured(
                    serde_json::json!({"oversized": true}),
                ))
                .await,
        );
        assert!(structured_result.contains("full output could not be saved"));
        assert!(structured_result.contains("increase the tool-result limits"));
        assert!(blocking_file.is_file());
        fs::remove_file(blocking_file).expect("remove blocking file");
    }
}
