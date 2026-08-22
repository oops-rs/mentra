//! Turns a streamed `chat/completions` response into provider events.
//!
//! The wire streams flat deltas — a little text, a slice of a tool call's
//! arguments — with no notion of a content block opening or closing. The
//! provider-neutral stream is block-structured and the collector rejects a
//! delta for a block that never started or a block that never stopped, so this
//! is where blocks are opened on first sight, indexed, and closed together at
//! the end of the response.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::{
    ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, ProviderEventStream,
    ProviderId, ReasoningFormat, ReasoningProvenance, Role, TokenUsage,
};

use super::model::{ChatCompletionChunk, ChatUsage};

pub(crate) fn spawn_event_stream(
    response: reqwest::Response,
    provider: ProviderId,
    requested_model: String,
) -> ProviderEventStream {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(error) = forward_events(response, tx.clone(), provider, requested_model).await {
            let _ = tx.send(Err(error));
        }
    });

    rx
}

async fn forward_events(
    response: reqwest::Response,
    tx: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    provider: ProviderId,
    requested_model: String,
) -> Result<(), ProviderError> {
    let mut bytes_stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut state = StreamState::new(provider, requested_model);

    while let Some(chunk) = bytes_stream.next().await {
        let chunk = chunk.map_err(ProviderError::Transport)?;
        buffer.extend_from_slice(&chunk);

        while let Some((frame_end, delimiter_len)) = find_frame_boundary(&buffer) {
            let frame = buffer.drain(..frame_end).collect::<Vec<_>>();
            buffer.drain(..delimiter_len);

            for event in parse_frame(&frame, &mut state)? {
                if tx.send(Ok(event)).is_err() {
                    return Ok(());
                }
            }
        }
    }

    if !buffer.is_empty() {
        for event in parse_frame(&buffer, &mut state)? {
            if tx.send(Ok(event)).is_err() {
                return Ok(());
            }
        }
    }

    // The wire has no "response finished" event beyond `[DONE]`, and plenty of
    // endpoints just close the connection. Closing the blocks here means a
    // truncated stream still produces a well-formed response rather than a
    // "block did not complete" error.
    for event in state.finish() {
        if tx.send(Ok(event)).is_err() {
            return Ok(());
        }
    }

    Ok(())
}

/// Splits an SSE byte buffer on the first blank line, tolerating both `\n\n`
/// and `\r\n\r\n`.
fn find_frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let lf = buffer.windows(2).position(|window| window == b"\n\n");

    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf <= lf => Some((crlf, 4)),
        (_, Some(lf)) => Some((lf, 2)),
        (Some(crlf), None) => Some((crlf, 4)),
        (None, None) => None,
    }
}

struct StreamState {
    started: bool,
    finished: bool,
    requested_model: String,
    provenance: ReasoningProvenance,
    next_index: usize,
    text_index: Option<usize>,
    reasoning_index: Option<usize>,
    /// The wire numbers tool calls within a turn; the neutral stream numbers
    /// every content block. This maps one to the other.
    tool_indices: HashMap<usize, usize>,
    open_blocks: Vec<usize>,
    stop_reason: Option<String>,
    usage: Option<TokenUsage>,
}

impl StreamState {
    fn new(provider: ProviderId, requested_model: String) -> Self {
        Self {
            started: false,
            finished: false,
            provenance: ReasoningProvenance {
                provider,
                model: requested_model.clone(),
                format: ReasoningFormat::OpenAiEncrypted,
            },
            requested_model,
            next_index: 0,
            text_index: None,
            reasoning_index: None,
            tool_indices: HashMap::new(),
            open_blocks: Vec::new(),
            stop_reason: None,
            usage: None,
        }
    }

    fn allocate(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        self.open_blocks.push(index);
        index
    }

    /// Emits the events that close out a response, once.
    fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut events = Vec::new();
        if !self.started {
            // Nothing ever arrived; still produce a well-formed empty response.
            events.push(ProviderEvent::MessageStarted {
                id: String::new(),
                model: self.requested_model.clone(),
                role: Role::Assistant,
            });
            self.started = true;
        }
        for index in std::mem::take(&mut self.open_blocks) {
            events.push(ProviderEvent::ContentBlockStopped { index });
        }
        events.push(ProviderEvent::MessageDelta {
            stop_reason: self.stop_reason.take(),
            usage: self.usage.take(),
        });
        events.push(ProviderEvent::MessageStopped);
        events
    }
}

fn parse_frame(frame: &[u8], state: &mut StreamState) -> Result<Vec<ProviderEvent>, ProviderError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| ProviderError::MalformedStream(error.to_string()))?;
    let mut data_lines = Vec::new();

    for raw_line in frame.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }

    if data_lines.is_empty() {
        return Ok(Vec::new());
    }

    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(state.finish());
    }

    let chunk: ChatCompletionChunk =
        serde_json::from_str(&data).map_err(ProviderError::Deserialize)?;
    Ok(apply_chunk(chunk, state))
}

fn apply_chunk(chunk: ChatCompletionChunk, state: &mut StreamState) -> Vec<ProviderEvent> {
    if state.finished {
        return Vec::new();
    }

    let mut events = Vec::new();

    if !state.started {
        state.started = true;
        events.push(ProviderEvent::MessageStarted {
            id: chunk.id.clone().unwrap_or_default(),
            // Gateways routinely answer with a model id other than the one
            // asked for; the one they name is the one that ran.
            model: chunk
                .model
                .clone()
                .unwrap_or_else(|| state.requested_model.clone()),
            role: Role::Assistant,
        });
    }

    if let Some(usage) = chunk.usage {
        state.usage = Some(token_usage(usage));
    }

    for choice in chunk.choices {
        if let Some(reasoning) = choice.delta.reasoning_text() {
            let index = match state.reasoning_index {
                Some(index) => index,
                None => {
                    let index = state.allocate();
                    state.reasoning_index = Some(index);
                    events.push(ProviderEvent::ContentBlockStarted {
                        index,
                        kind: ContentBlockStart::Thinking {
                            encrypted_content: None,
                            id: None,
                            provenance: Some(state.provenance.clone()),
                            redacted: false,
                        },
                    });
                    index
                }
            };
            events.push(ProviderEvent::ContentBlockDelta {
                index,
                delta: ContentBlockDelta::ThinkingText(reasoning.to_string()),
            });
        }

        if let Some(text) = choice.delta.content.as_deref().filter(|t| !t.is_empty()) {
            let index = match state.text_index {
                Some(index) => index,
                None => {
                    let index = state.allocate();
                    state.text_index = Some(index);
                    events.push(ProviderEvent::ContentBlockStarted {
                        index,
                        kind: ContentBlockStart::Text,
                    });
                    index
                }
            };
            events.push(ProviderEvent::ContentBlockDelta {
                index,
                delta: ContentBlockDelta::Text(text.to_string()),
            });
        }

        for call in choice.delta.tool_calls {
            let index = match state.tool_indices.get(&call.index) {
                Some(index) => *index,
                None => {
                    let index = state.allocate();
                    state.tool_indices.insert(call.index, index);
                    events.push(ProviderEvent::ContentBlockStarted {
                        index,
                        kind: ContentBlockStart::ToolUse {
                            // An endpoint that omits the id still has to be
                            // answerable, and the tool result has to name
                            // something: fall back to the block's own position.
                            id: call.id.clone().unwrap_or_else(|| format!("call_{index}")),
                            name: call
                                .function
                                .as_ref()
                                .and_then(|function| function.name.clone())
                                .unwrap_or_default(),
                        },
                    });
                    index
                }
            };

            if let Some(arguments) = call
                .function
                .and_then(|function| function.arguments)
                .filter(|arguments| !arguments.is_empty())
            {
                events.push(ProviderEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ToolUseInputJson(arguments),
                });
            }
        }

        if let Some(finish_reason) = choice.finish_reason {
            state.stop_reason = Some(finish_reason);
        }
    }

    events
}

fn token_usage(usage: ChatUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        cache_read_input_tokens: usage
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens),
        cache_creation_input_tokens: None,
        reasoning_tokens: usage
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens),
        thoughts_tokens: None,
        tool_input_tokens: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::collect_response_from_stream;
    use crate::{ContentBlock, Response};

    /// Feeds raw SSE bytes through the parser exactly as the transport would,
    /// then closes the stream, and returns every event produced.
    fn events(raw: &str) -> Vec<ProviderEvent> {
        let mut state = StreamState::new(ProviderId::new("test"), "asked-for".to_string());
        let mut buffer = raw.as_bytes().to_vec();
        let mut events = Vec::new();

        while let Some((frame_end, delimiter_len)) = find_frame_boundary(&buffer) {
            let frame = buffer.drain(..frame_end).collect::<Vec<_>>();
            buffer.drain(..delimiter_len);
            events.extend(parse_frame(&frame, &mut state).expect("frame parses"));
        }
        if !buffer.is_empty() {
            events.extend(parse_frame(&buffer, &mut state).expect("trailing frame parses"));
        }
        events.extend(state.finish());
        events
    }

    /// Collects the events into the response a caller would actually receive,
    /// which is the real contract: the collector rejects any stream whose
    /// blocks do not open and close in order.
    async fn response(raw: &str) -> Response {
        let (tx, rx) = mpsc::unbounded_channel();
        for event in events(raw) {
            tx.send(Ok(event)).expect("receiver alive");
        }
        drop(tx);
        collect_response_from_stream(rx)
            .await
            .expect("a well-formed stream collects")
    }

    #[tokio::test]
    async fn text_and_usage_become_one_text_block() {
        let response = response(concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"served-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2,\"total_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;

        assert_eq!(response.id, "chatcmpl-1");
        // The model that answered, not the one asked for: a gateway routinely
        // serves something other than the requested id.
        assert_eq!(response.model, "served-model");
        assert_eq!(response.content, vec![ContentBlock::text("Hello")]);
        assert_eq!(response.stop_reason.as_deref(), Some("stop"));

        let usage = response.usage.expect("stream_options asked for usage");
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(2));
        assert_eq!(usage.cache_read_input_tokens, Some(8));
    }

    #[tokio::test]
    async fn a_tool_call_is_assembled_from_its_argument_slices() {
        let response = response(concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;

        assert_eq!(
            response.content,
            vec![ContentBlock::ToolUse {
                id: "call_a".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "a.rs"}),
            }]
        );
        assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn parallel_tool_calls_keep_their_own_blocks() {
        // The wire numbers tool calls within the turn and interleaves their
        // argument slices; each has to land in its own content block.
        let response = response(concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"one\",\"arguments\":\"{\\\"x\\\":\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"two\",\"arguments\":\"{\\\"y\\\":\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"2}\"}},{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;

        assert_eq!(
            response.content,
            vec![
                ContentBlock::ToolUse {
                    id: "a".to_string(),
                    name: "one".to_string(),
                    input: serde_json::json!({"x": 1}),
                },
                ContentBlock::ToolUse {
                    id: "b".to_string(),
                    name: "two".to_string(),
                    input: serde_json::json!({"y": 2}),
                },
            ]
        );
    }

    #[tokio::test]
    async fn separated_reasoning_becomes_a_thinking_block() {
        // DeepSeek and vLLM put a reasoning model's scratchpad in
        // `reasoning_content`; several gateways spell it `reasoning`.
        let response = response(concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"weigh\"}}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"ing\"}}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;

        assert_eq!(response.content.len(), 2, "{:?}", response.content);
        assert!(
            matches!(
                &response.content[0],
                ContentBlock::Thinking { thinking, .. } if thinking == "weighing"
            ),
            "{:?}",
            response.content
        );
        assert_eq!(response.content[1], ContentBlock::text("answer"));
    }

    #[tokio::test]
    async fn a_stream_cut_off_without_done_still_closes_its_blocks() {
        // Plenty of endpoints just close the connection instead of sending
        // `[DONE]`. The collector rejects a block that never stopped, so the
        // end of the byte stream has to close them.
        let response = response(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        )
        .await;

        assert_eq!(response.content, vec![ContentBlock::text("partial")]);
        assert_eq!(response.stop_reason, None);
    }

    #[tokio::test]
    async fn done_after_the_final_chunk_does_not_close_twice() {
        let events = events(concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ));

        let stops = events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::MessageStopped))
            .count();
        assert_eq!(stops, 1, "{events:?}");
    }

    #[test]
    fn crlf_framing_is_read_like_lf_framing() {
        let events = events(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
        );

        assert!(
            events.iter().any(|event| matches!(
                event,
                ProviderEvent::ContentBlockDelta {
                    delta: ContentBlockDelta::Text(text),
                    ..
                } if text == "hi"
            )),
            "{events:?}"
        );
    }
}
