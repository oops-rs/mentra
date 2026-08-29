use std::{borrow::Cow, collections::VecDeque, io};

use tokio::io::{AsyncRead, AsyncReadExt};

/// Bytes held back from `max_bytes` to pay for the elision marker when a
/// capture keeps both ends of a stream. The marker is
/// `\n[... <n> bytes elided ...]\n`, at most 45 bytes for any `u64`.
const ELISION_MARKER_RESERVE: usize = 64;

/// Caps below this keep the head alone. Splitting a budget this small between
/// two windows and a marker leaves neither window long enough to say anything.
const MIN_CAP_FOR_TWO_WINDOWS: usize = 256;

/// How far into the kept tail to look for a line boundary to start on, so the
/// tail does not open mid-line. A stream with no newline that close — one long
/// JSON line, say — is kept as-is rather than searched to its end.
const TAIL_LINE_BOUNDARY_WINDOW: usize = 512;

/// One of a child process's output streams, already bounded.
///
/// The bytes are what a caller gets to keep: never more than the cap the run
/// was given, marker included, however much the program printed. There is no
/// accessor for the discarded bytes because they were never held — the cap is
/// applied while reading, so a program printing a gigabyte costs the reader a
/// buffer, not a gigabyte.
#[derive(Clone, PartialEq, Eq)]
pub struct CapturedStream {
    pub(super) bytes: Vec<u8>,
    pub(super) truncated: bool,
}

impl CapturedStream {
    /// The kept bytes, head and tail with an elision marker between them when
    /// [`truncated`](Self::truncated) is set.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Takes the kept bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The kept bytes as text, with invalid UTF-8 replaced.
    ///
    /// Lossy rather than fallible on purpose: a program that prints one bad
    /// byte has still said something, and a cap can land mid-codepoint by
    /// construction.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    /// Whether the program printed more than the cap allowed to be kept.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// How many bytes were kept.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing at all was kept.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl std::fmt::Debug for CapturedStream {
    /// Prints the kept text rather than a list of byte values, because this
    /// type shows up in assertion failures where the text is the evidence.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedStream")
            .field("text", &self.to_string_lossy())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Reads `reader` to EOF, keeping at most `max_bytes` of it.
///
/// The whole stream is always drained, so a child process is never blocked on a
/// full pipe by a cap this side of it. What is *kept* is the head and the tail:
/// a command's most load-bearing output is at both ends — the command echo and
/// early context at the start, the assertion that failed or the stack that
/// unwound at the end — and keeping only the head is keeping the half that says
/// a run started. What fell out between them is replaced by a marker naming its
/// size, so the result never reads as contiguous output.
///
/// The kept bytes never exceed `max_bytes`, marker included.
pub(super) async fn read_capped<R>(mut reader: R, max_bytes: usize) -> io::Result<CapturedStream>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (head_budget, tail_budget) = if max_bytes < MIN_CAP_FOR_TWO_WINDOWS {
        (max_bytes, 0)
    } else {
        let split = max_bytes - ELISION_MARKER_RESERVE;
        let head = split / 2;
        (head, split - head)
    };

    let mut head = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut elided = 0_u64;
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let mut chunk = &buffer[..read];

        let head_room = head_budget.saturating_sub(head.len());
        if head_room > 0 {
            let take = head_room.min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if chunk.is_empty() {
            continue;
        }

        if tail_budget == 0 {
            elided += chunk.len() as u64;
            continue;
        }

        // Keep the last `tail_budget` bytes seen, counting what falls off the
        // front rather than growing without bound.
        if chunk.len() >= tail_budget {
            elided += tail.len() as u64 + (chunk.len() - tail_budget) as u64;
            tail.clear();
            tail.extend(&chunk[chunk.len() - tail_budget..]);
        } else {
            let overflow = (tail.len() + chunk.len()).saturating_sub(tail_budget);
            elided += overflow as u64;
            tail.drain(..overflow);
            tail.extend(chunk);
        }
    }

    // Nothing fell out: head and tail are still one contiguous run of bytes.
    if elided == 0 {
        head.extend(tail);
        return Ok(CapturedStream {
            bytes: head,
            truncated: false,
        });
    }

    if tail.is_empty() {
        return Ok(CapturedStream {
            bytes: head,
            truncated: true,
        });
    }

    let tail = Vec::from(tail);
    let boundary = tail[..TAIL_LINE_BOUNDARY_WINDOW.min(tail.len())]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .filter(|start| *start < tail.len())
        .unwrap_or(0);
    let elided = elided + boundary as u64;

    let mut bytes = head;
    bytes.extend_from_slice(format!("\n[... {elided} bytes elided ...]\n").as_bytes());
    bytes.extend_from_slice(&tail[boundary..]);

    Ok(CapturedStream {
        bytes,
        truncated: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `input` through the capture used for a child process's stdout.
    async fn capture(input: &[u8], max_bytes: usize) -> CapturedStream {
        read_capped(std::io::Cursor::new(input.to_vec()), max_bytes)
            .await
            .expect("cursor never fails to read")
    }

    #[tokio::test]
    async fn a_stream_under_the_cap_is_byte_identical() {
        let input = b"line one\nline two\nline three\n";

        let captured = capture(input, 4096).await;

        assert_eq!(captured.as_bytes(), input);
        assert!(!captured.truncated());
    }

    #[tokio::test]
    async fn a_capped_stream_keeps_the_end_a_failure_is_reported_at() {
        // A test runner names what failed on its last lines. Keeping only the
        // head of a capped stream is keeping the half that says a run started.
        let mut input = String::from("FIRST LINE\n");
        for index in 0..4000 {
            input.push_str(&format!("filler line {index}\n"));
        }
        input.push_str("LAST LINE: assertion failed\n");

        let captured = capture(input.as_bytes(), 4096).await;

        assert!(captured.truncated());
        assert!(captured.len() <= 4096, "cap is still a hard bound");
        let text = captured.to_string_lossy();
        assert!(text.starts_with("FIRST LINE\n"), "{text}");
        assert!(text.ends_with("LAST LINE: assertion failed\n"), "{text}");
        assert!(text.contains("bytes elided"), "{text}");
    }

    #[tokio::test]
    async fn a_cap_too_small_to_split_keeps_the_head() {
        // Below the split threshold there is no room for two windows and a
        // marker, so the capture stays exactly what it has always been.
        let captured = capture(b"aaaaaaaaaaaaaaaaaaaaaaaa", 8).await;

        assert_eq!(captured.as_bytes(), b"aaaaaaaa");
        assert!(captured.truncated());
    }
}
