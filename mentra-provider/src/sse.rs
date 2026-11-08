use std::{borrow::Cow, str::Utf8Error};

#[derive(Default)]
pub(crate) struct SseBuffer {
    bytes: Vec<u8>,
    start: usize,
    scanned: usize,
}

impl SseBuffer {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        // Compact once per input chunk, keeping the inspected part of an
        // incomplete frame so the next scan only visits new bytes.
        if self.start != 0 {
            self.bytes.drain(..self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.bytes.extend_from_slice(chunk);
    }

    pub(crate) fn next_frame(&mut self) -> Option<&[u8]> {
        while let Some(offset) = self.bytes[self.scanned..].iter().position(|&b| b == b'\n') {
            self.scanned += offset + 1;
            let scanned = &self.bytes[self.start..self.scanned];
            let delimiter_len = if scanned.ends_with(b"\n\n") {
                2
            } else if scanned.ends_with(b"\r\n\r\n") {
                4
            } else {
                continue;
            };
            self.start = self.scanned;
            return Some(&scanned[..scanned.len() - delimiter_len]);
        }

        self.scanned = self.bytes.len();
        None
    }

    pub(crate) fn tail(&self) -> &[u8] {
        &self.bytes[self.start..]
    }
}

pub(crate) fn frame_data(frame: &[u8]) -> Result<Option<Cow<'_, str>>, Utf8Error> {
    let mut lines = std::str::from_utf8(frame)?.lines().filter_map(|line| {
        line.strip_suffix('\r')
            .unwrap_or(line)
            .strip_prefix("data:")
            .map(str::trim_start)
    });
    let Some(first) = lines.next() else {
        return Ok(None);
    };
    let Some(second) = lines.next() else {
        return Ok(Some(Cow::Borrowed(first)));
    };

    let mut data = String::with_capacity(first.len() + 1 + second.len());
    data.push_str(first);
    for line in std::iter::once(second).chain(lines) {
        data.push('\n');
        data.push_str(line);
    }
    Ok(Some(Cow::Owned(data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_frames_in_order_across_all_chunk_sizes() {
        let input = "data: α\r\n\r\n\n\ndata: β\n\ndata: γ\r\n\r\ndata: tail";
        let expected = ["data: α", "", "data: β", "data: γ"].map(str::as_bytes);
        for size in 1..=input.len() {
            let mut buffer = SseBuffer::default();
            let mut frames = Vec::new();
            for chunk in input.as_bytes().chunks(size) {
                buffer.push(chunk);
                while let Some(frame) = buffer.next_frame() {
                    frames.push(frame.to_vec());
                }
            }
            assert_eq!(frames, expected, "chunk size {size}");
            assert_eq!(buffer.tail(), b"data: tail");
        }
    }

    #[test]
    fn borrows_single_line_data_and_joins_multiline_data() {
        assert!(matches!(
            frame_data(b"event: token\r\n: heartbeat\r\ndata:  token\r"),
            Ok(Some(Cow::Borrowed("token")))
        ));
        assert_eq!(
            frame_data(b"data: {\r\nevent: ignored\r\ndata:\r\ndata: }")
                .unwrap()
                .as_deref(),
            Some("{\n\n}")
        );
        assert_eq!(frame_data(b": heartbeat"), Ok(None));
        assert!(frame_data(b": \xff").is_err());
    }
}
