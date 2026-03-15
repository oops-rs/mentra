use std::collections::{BTreeSet, HashMap};

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::provider::model::{
    ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, ProviderEventStream, Role,
    TokenUsage,
};

pub(crate) fn spawn_event_stream(
    response: reqwest::Response,
    request_model: String,
) -> ProviderEventStream {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(error) = forward_events(response, request_model, tx.clone()).await {
            let _ = tx.send(Err(error));
        }
    });

    rx
}

async fn forward_events(
    response: reqwest::Response,
    request_model: String,
    tx: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    let mut bytes_stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut state = StreamState::new(request_model);

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
            let _ = tx.send(Ok(event));
        }
    }

    if state.started && !state.stopped {
        return Err(ProviderError::MalformedStream(
            "Gemini stream ended before MessageStopped".to_string(),
        ));
    }

    Ok(())
}

struct StreamState {
    request_model: String,
    response_id: Option<String>,
    model_version: Option<String>,
    started: bool,
    stopped: bool,
    latest_usage: Option<TokenUsage>,
    open_blocks: BTreeSet<usize>,
    text_snapshots: HashMap<usize, String>,
    tool_snapshots: HashMap<usize, String>,
    tool_call_ids: HashMap<usize, String>,
}

impl StreamState {
    fn new(request_model: String) -> Self {
        Self {
            request_model,
            response_id: None,
            model_version: None,
            started: false,
            stopped: false,
            latest_usage: None,
            open_blocks: BTreeSet::new(),
            text_snapshots: HashMap::new(),
            tool_snapshots: HashMap::new(),
            tool_call_ids: HashMap::new(),
        }
    }

    fn ensure_message_started(&mut self, chunk: &GeminiStreamChunk) -> Option<ProviderEvent> {
        if self.started {
            return None;
        }

        self.started = true;
        self.response_id = chunk
            .response_id
            .clone()
            .or_else(|| Some(format!("gemini-{}", self.request_model)));
        self.model_version = chunk.model_version.clone();

        Some(ProviderEvent::MessageStarted {
            id: self
                .response_id
                .clone()
                .unwrap_or_else(|| "gemini".to_string()),
            model: self
                .model_version
                .clone()
                .unwrap_or_else(|| self.request_model.clone()),
            role: Role::Assistant,
        })
    }

    fn ensure_text_block_started(&mut self, index: usize) -> Option<ProviderEvent> {
        if self.open_blocks.insert(index) {
            Some(ProviderEvent::ContentBlockStarted {
                index,
                kind: ContentBlockStart::Text,
            })
        } else {
            None
        }
    }

    fn ensure_tool_block_started(
        &mut self,
        index: usize,
        function_call: &GeminiFunctionCall,
    ) -> Option<ProviderEvent> {
        if self.open_blocks.insert(index) {
            let response_id = self
                .response_id
                .clone()
                .unwrap_or_else(|| format!("gemini-{}", self.request_model));
            let id = format!("{response_id}-{index}-{}", function_call.name);
            self.tool_call_ids.insert(index, id.clone());
            Some(ProviderEvent::ContentBlockStarted {
                index,
                kind: ContentBlockStart::ToolUse {
                    id,
                    name: function_call.name.clone(),
                },
            })
        } else {
            None
        }
    }

    fn close_all_blocks(&mut self) -> Vec<ProviderEvent> {
        let indices = self.open_blocks.iter().copied().collect::<Vec<_>>();
        self.open_blocks.clear();
        self.text_snapshots.clear();
        self.tool_snapshots.clear();
        self.tool_call_ids.clear();

        indices
            .into_iter()
            .map(|index| ProviderEvent::ContentBlockStopped { index })
            .collect()
    }

    fn update_usage(&mut self, usage: Option<TokenUsage>) -> Option<TokenUsage> {
        match usage {
            Some(usage) if self.latest_usage.as_ref() != Some(&usage) => {
                self.latest_usage = Some(usage.clone());
                Some(usage)
            }
            Some(usage) => {
                self.latest_usage = Some(usage);
                None
            }
            None => None,
        }
    }
}

fn parse_frame(frame: &[u8], state: &mut StreamState) -> Result<Vec<ProviderEvent>, ProviderError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| ProviderError::MalformedStream(error.to_string()))?;
    let mut data_lines = Vec::new();

    for raw_line in frame.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
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
    let chunk: GeminiStreamChunk =
        serde_json::from_str(&data).map_err(ProviderError::Deserialize)?;

    if let Some(error) = chunk.error {
        return Err(ProviderError::MalformedStream(
            error
                .message
                .unwrap_or_else(|| "gemini stream error".to_string()),
        ));
    }

    let mut events = Vec::new();
    let latest_usage = chunk
        .usage_metadata
        .as_ref()
        .and_then(GeminiUsageMetadata::to_token_usage);

    if let Some(candidate) = chunk.candidates.first() {
        if let Some(event) = state.ensure_message_started(&chunk) {
            events.push(event);
        }

        if let Some(content) = candidate.content.as_ref() {
            for (index, part) in content.parts.iter().enumerate() {
                if let Some(text) = part.text.as_deref() {
                    if let Some(start) = state.ensure_text_block_started(index) {
                        events.push(start);
                    }

                    let previous = state.text_snapshots.entry(index).or_default();
                    if let Some(delta) = merge_chunk(previous, text) {
                        events.push(ProviderEvent::ContentBlockDelta {
                            index,
                            delta: ContentBlockDelta::Text(delta),
                        });
                    }
                } else if let Some(function_call) = part.function_call.as_ref() {
                    if let Some(start) = state.ensure_tool_block_started(index, function_call) {
                        events.push(start);
                    }

                    let args = serde_json::to_string(&function_call.args)
                        .map_err(ProviderError::Serialize)?;
                    let previous = state.tool_snapshots.entry(index).or_default();
                    if let Some(delta) = merge_chunk(previous, &args) {
                        events.push(ProviderEvent::ContentBlockDelta {
                            index,
                            delta: ContentBlockDelta::ToolUseInputJson(delta),
                        });
                    }
                }
            }
        }

        let usage_changed = state.update_usage(latest_usage.clone());

        if let Some(stop_reason) = candidate.finish_reason.clone() {
            events.extend(state.close_all_blocks());
            events.push(ProviderEvent::MessageDelta {
                stop_reason: Some(stop_reason),
                usage: latest_usage.or_else(|| state.latest_usage.clone()),
            });
            events.push(ProviderEvent::MessageStopped);
            state.stopped = true;
        } else if let Some(usage) = usage_changed {
            events.push(ProviderEvent::MessageDelta {
                stop_reason: None,
                usage: Some(usage),
            });
        }
    } else if let Some(prompt_feedback) = chunk.prompt_feedback.as_ref() {
        if let Some(event) = state.ensure_message_started(&chunk) {
            events.push(event);
        }
        let _ = state.update_usage(latest_usage.clone());
        events.extend(state.close_all_blocks());
        events.push(ProviderEvent::MessageDelta {
            stop_reason: Some(prompt_feedback.stop_reason()),
            usage: latest_usage.or_else(|| state.latest_usage.clone()),
        });
        events.push(ProviderEvent::MessageStopped);
        state.stopped = true;
    }

    Ok(events)
}

fn merge_chunk(previous: &mut String, current: &str) -> Option<String> {
    if current.is_empty() {
        return None;
    }

    if previous.is_empty() {
        *previous = current.to_string();
        return Some(current.to_string());
    }

    if current == previous {
        return None;
    }

    if current.starts_with(previous.as_str()) {
        let delta = current[previous.len()..].to_string();
        *previous = current.to_string();
        return (!delta.is_empty()).then_some(delta);
    }

    previous.push_str(current);
    Some(current.to_string())
}

fn find_frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for (index, window) in buffer.windows(2).enumerate() {
        if window == b"\n\n" {
            return Some((index, 2));
        }
    }

    for (index, window) in buffer.windows(4).enumerate() {
        if window == b"\r\n\r\n" {
            return Some((index, 4));
        }
    }

    None
}

#[derive(Deserialize)]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "promptFeedback", alias = "prompt_feedback")]
    prompt_feedback: Option<GeminiPromptFeedback>,
    #[serde(default, rename = "usageMetadata", alias = "usage_metadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default, rename = "responseId", alias = "response_id")]
    response_id: Option<String>,
    #[serde(default, rename = "modelVersion", alias = "model_version")]
    model_version: Option<String>,
    #[serde(default)]
    error: Option<GeminiErrorBody>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(default, rename = "finishReason", alias = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiContent {
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Deserialize)]
struct GeminiPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall", alias = "function_call")]
    function_call: Option<GeminiFunctionCall>,
}

#[derive(Deserialize)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
struct GeminiErrorBody {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct GeminiPromptFeedback {
    #[serde(default, rename = "blockReason", alias = "block_reason")]
    block_reason: Option<String>,
}

impl GeminiPromptFeedback {
    fn stop_reason(&self) -> String {
        self.block_reason
            .clone()
            .unwrap_or_else(|| "BLOCKED".to_string())
    }
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount", alias = "prompt_token_count")]
    prompt_token_count: Option<u64>,
    #[serde(
        default,
        rename = "candidatesTokenCount",
        alias = "candidates_token_count"
    )]
    candidates_token_count: Option<u64>,
    #[serde(default, rename = "totalTokenCount", alias = "total_token_count")]
    total_token_count: Option<u64>,
    #[serde(
        default,
        rename = "cachedContentTokenCount",
        alias = "cached_content_token_count"
    )]
    cached_content_token_count: Option<u64>,
    #[serde(default, rename = "thoughtsTokenCount", alias = "thoughts_token_count")]
    thoughts_token_count: Option<u64>,
    #[serde(
        default,
        rename = "toolUsePromptTokenCount",
        alias = "tool_use_prompt_token_count"
    )]
    tool_use_prompt_token_count: Option<u64>,
}

impl GeminiUsageMetadata {
    fn to_token_usage(&self) -> Option<TokenUsage> {
        let usage = TokenUsage {
            input_tokens: self.prompt_token_count,
            output_tokens: self.candidates_token_count,
            total_tokens: self.total_token_count,
            cache_read_input_tokens: self.cached_content_token_count,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            thoughts_tokens: self.thoughts_token_count,
            tool_input_tokens: self.tool_use_prompt_token_count,
        };

        (!usage.is_empty()).then_some(usage)
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::model::{ContentBlockDelta, ContentBlockStart, ProviderEvent, TokenUsage};

    use super::{StreamState, parse_frame};

    #[test]
    fn streams_text_and_completion_events() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());

        let events = parse_frame(
            br#"data: {"responseId":"resp-1","modelVersion":"gemini-2.0-flash-001","candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]}}]}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::MessageStarted {
                    id: "resp-1".to_string(),
                    model: "gemini-2.0-flash-001".to_string(),
                    role: crate::provider::Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("Hel".to_string()),
                },
            ]
        );

        let events = parse_frame(
            br#"data: {"candidates":[{"content":{"parts":[{"text":"lo"}]}}]}"#,
            &mut state,
        )
        .expect("frame should parse");
        assert_eq!(
            events,
            vec![ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("lo".to_string()),
            }]
        );

        let events = parse_frame(
            br#"data: {"candidates":[{"finishReason":"STOP"}]}"#,
            &mut state,
        )
        .expect("frame should parse");
        assert_eq!(
            events,
            vec![
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageDelta {
                    stop_reason: Some("STOP".to_string()),
                    usage: None,
                },
                ProviderEvent::MessageStopped,
            ]
        );
    }

    #[test]
    fn streams_function_calls() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());

        let events = parse_frame(
            br#"data: {"responseId":"resp-1","candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"README.md"}}}]}}]}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::MessageStarted {
                    id: "resp-1".to_string(),
                    model: "gemini-2.0-flash".to_string(),
                    role: crate::provider::Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::ToolUse {
                        id: "resp-1-0-read_file".to_string(),
                        name: "read_file".to_string(),
                    },
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolUseInputJson(
                        "{\"path\":\"README.md\"}".to_string()
                    ),
                },
            ]
        );
    }

    #[test]
    fn ignores_duplicate_full_function_call_payloads() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());
        parse_frame(
            br#"data: {"responseId":"resp-1","candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"README.md"}}}]}}]}"#,
            &mut state,
        )
        .expect("first frame should parse");

        let events = parse_frame(
            br#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"README.md"}}}]}}]}"#,
            &mut state,
        )
        .expect("second frame should parse");

        assert!(events.is_empty());
    }

    #[test]
    fn ignores_unsupported_parts_without_breaking_indexes() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());

        let events = parse_frame(
            br#"data: {"responseId":"resp-1","candidates":[{"content":{"parts":[{"fileData":{"mimeType":"image/png","fileUri":"files/1"}},{"text":"Done"}]}}]}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::MessageStarted {
                    id: "resp-1".to_string(),
                    model: "gemini-2.0-flash".to_string(),
                    role: crate::provider::Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 1,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 1,
                    delta: ContentBlockDelta::Text("Done".to_string()),
                },
            ]
        );
    }

    #[test]
    fn surfaces_stream_errors() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());
        let error = parse_frame(br#"data: {"error":{"message":"boom"}}"#, &mut state)
            .expect_err("frame should fail");

        match error {
            crate::provider::ProviderError::MalformedStream(message) => {
                assert_eq!(message, "boom");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn treats_prompt_feedback_only_chunks_as_terminal() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());

        let events = parse_frame(
            br#"data: {"responseId":"resp-2","promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":11,"totalTokenCount":11,"cachedContentTokenCount":4}}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::MessageStarted {
                    id: "resp-2".to_string(),
                    model: "gemini-2.0-flash".to_string(),
                    role: crate::provider::Role::Assistant,
                },
                ProviderEvent::MessageDelta {
                    stop_reason: Some("SAFETY".to_string()),
                    usage: Some(TokenUsage {
                        input_tokens: Some(11),
                        output_tokens: None,
                        total_tokens: Some(11),
                        cache_read_input_tokens: Some(4),
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                        thoughts_tokens: None,
                        tool_input_tokens: None,
                    }),
                },
                ProviderEvent::MessageStopped,
            ]
        );
    }

    #[test]
    fn emits_usage_metadata_updates_and_final_usage() {
        let mut state = StreamState::new("gemini-2.0-flash".to_string());

        let events = parse_frame(
            br#"data: {"responseId":"resp-3","candidates":[{"content":{"parts":[{"text":"Hi"}]}}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":1,"totalTokenCount":9,"thoughtsTokenCount":2,"toolUsePromptTokenCount":1}}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::MessageStarted {
                    id: "resp-3".to_string(),
                    model: "gemini-2.0-flash".to_string(),
                    role: crate::provider::Role::Assistant,
                },
                ProviderEvent::ContentBlockStarted {
                    index: 0,
                    kind: ContentBlockStart::Text,
                },
                ProviderEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text("Hi".to_string()),
                },
                ProviderEvent::MessageDelta {
                    stop_reason: None,
                    usage: Some(TokenUsage {
                        input_tokens: Some(8),
                        output_tokens: Some(1),
                        total_tokens: Some(9),
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                        thoughts_tokens: Some(2),
                        tool_input_tokens: Some(1),
                    }),
                },
            ]
        );

        let events = parse_frame(
            br#"data: {"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":2,"totalTokenCount":10,"thoughtsTokenCount":2,"toolUsePromptTokenCount":1}}"#,
            &mut state,
        )
        .expect("frame should parse");

        assert_eq!(
            events,
            vec![
                ProviderEvent::ContentBlockStopped { index: 0 },
                ProviderEvent::MessageDelta {
                    stop_reason: Some("STOP".to_string()),
                    usage: Some(TokenUsage {
                        input_tokens: Some(8),
                        output_tokens: Some(2),
                        total_tokens: Some(10),
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                        thoughts_tokens: Some(2),
                        tool_input_tokens: Some(1),
                    }),
                },
                ProviderEvent::MessageStopped,
            ]
        );
    }
}
