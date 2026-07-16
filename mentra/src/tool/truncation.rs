use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use mentra_provider::ToolResultContent;

static NEXT_SPILL_ID: AtomicU64 = AtomicU64::new(1);

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

    pub(super) fn apply(&self, content: ToolResultContent) -> ToolResultContent {
        match content {
            ToolResultContent::Text(text) => self.apply_text(text),
            ToolResultContent::Structured(value) => self.apply_structured(value),
        }
    }

    fn apply_text(&self, text: String) -> ToolResultContent {
        let total_lines = line_count(&text);
        if text.len() <= self.max_bytes && total_lines <= self.max_lines {
            return ToolResultContent::Text(text);
        }

        let spill = self.spill(&text, "txt");
        let mut shown_bytes = 0_usize;
        let mut shown_lines = 0_usize;
        for line in text.split_inclusive('\n') {
            if shown_lines == self.max_lines
                || shown_bytes.saturating_add(line.len()) > self.max_bytes
            {
                break;
            }
            shown_bytes += line.len();
            shown_lines += 1;
        }

        let mut truncated = text[..shown_bytes].to_string();
        if !truncated.is_empty() && !truncated.ends_with('\n') {
            truncated.push('\n');
        }
        truncated.push_str(&format!(
            "[truncated: showing {shown_lines} of {total_lines} lines; {spill}]"
        ));
        ToolResultContent::Text(truncated)
    }

    fn apply_structured(&self, value: serde_json::Value) -> ToolResultContent {
        let serialized = serde_json::to_string(&value)
            .expect("serde_json::Value always serializes to valid JSON");
        let total_lines = line_count(&serialized);
        if serialized.len() <= self.max_bytes && total_lines <= self.max_lines {
            return ToolResultContent::Structured(value);
        }

        let spill = self.spill(&serialized, "json");
        ToolResultContent::Text(format!(
            "[truncated: structured tool output is {} bytes across {total_lines} lines; {spill}]",
            serialized.len()
        ))
    }

    fn spill(&self, content: &str, extension: &str) -> String {
        match &self.spill {
            SpillBehavior::Enabled(directory) => match spill_file(directory, extension, content) {
                Ok(path) => format!("full output at {}", path.display()),
                Err(error) => format!(
                    "full output could not be saved ({error}); increase the tool-result limits"
                ),
            },
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

    #[test]
    fn under_limit_text_is_byte_identical() {
        let original = "alpha\r\nbéta\n".to_string();
        assert_eq!(
            no_spill(original.len(), 2).apply(ToolResultContent::Text(original.clone())),
            ToolResultContent::Text(original)
        );
    }

    #[test]
    fn truncation_preserves_complete_crlf_and_utf8_lines() {
        let result = text(no_spill(10, 10).apply(ToolResultContent::Text(
            "alpha\r\nbéta\r\ngamma\r\n".to_string(),
        )));
        assert!(result.starts_with("alpha\r\n"));
        assert!(!result.contains("béta"));
        assert!(result.contains("showing 1 of 3 lines"));
    }

    #[test]
    fn oversized_first_line_is_never_partially_emitted() {
        let result = text(no_spill(4, 10).apply(ToolResultContent::Text("ééé\nnext".to_string())));
        assert!(result.starts_with("[truncated:"));
        assert!(result.contains("showing 0 of 2 lines"));
        assert!(!result.contains('é'));
    }

    #[test]
    fn line_limit_preserves_the_requested_head() {
        let result = text(
            no_spill(usize::MAX, 2).apply(ToolResultContent::Text("one\ntwo\nthree\n".to_string())),
        );
        assert!(result.starts_with("one\ntwo\n[truncated:"));
        assert!(result.contains("showing 2 of 3 lines"));
    }

    #[test]
    fn structured_content_spills_whole_json_and_becomes_pointer_text() {
        let directory = std::env::temp_dir().join(format!(
            "mentra-tool-output-limiter-{}-{}",
            std::process::id(),
            NEXT_SPILL_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let limiter = ToolOutputLimiter::new(4, 10, SpillBehavior::Enabled(directory.clone()));
        let value = serde_json::json!({"answer": [1, 2, 3]});
        let pointer = text(limiter.apply(ToolResultContent::Structured(value.clone())));
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
}
