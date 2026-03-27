use std::collections::HashSet;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{
    ContentBlockDelta, ContentBlockStart, HostedToolSearchCall, HostedWebSearchCall,
    ImageGenerationCall, ImageGenerationResult, ProviderError, ProviderEvent, ProviderEventStream,
    ResponseHeaders, Role, TokenUsage, WebSearchAction,
};

/// Spawns an event stream that decodes Responses SSE frames.
pub fn spawn_event_stream(response: reqwest::Response) -> ProviderEventStream {
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(error) = forward_events(response, tx.clone()).await {
            let _ = tx.send(Err(error));
        }
    });

    rx
}

async fn forward_events(
    response: reqwest::Response,
    tx: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
) -> Result<(), ProviderError> {
    if let Some(headers) = response_headers_event(response.headers())
        && tx.send(Ok(headers)).is_err()
    {
        return Ok(());
    }

    let mut bytes_stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut state = StreamState::default();

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

    Ok(())
}

pub(crate) fn response_headers_event(headers: &http::HeaderMap) -> Option<ProviderEvent> {
    let values = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect::<Vec<_>>();

    (!values.is_empty()).then_some(ProviderEvent::ResponseHeaders(ResponseHeaders { values }))
}

#[derive(Default)]
pub(crate) struct StreamState {
    ignored_output_indices: HashSet<usize>,
    text_delta_seen: HashSet<usize>,
    function_delta_seen: HashSet<usize>,
    tool_search_delta_seen: HashSet<usize>,
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
    parse_json_event(&data, state)
}

pub(crate) fn parse_json_event(
    data: &str,
    state: &mut StreamState,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if data == "[DONE]" {
        return Ok(Vec::new());
    }

    let event: ResponsesStreamEvent =
        serde_json::from_str(data).map_err(ProviderError::Deserialize)?;

    match event {
        ResponsesStreamEvent::ResponseCreated { response } => Ok(vec![
            ProviderEvent::ResponseCreated,
            ProviderEvent::MessageStarted {
                id: response.id,
                model: response.model,
                role: Role::Assistant,
            },
        ]),
        ResponsesStreamEvent::ResponseOutputItemAdded { output_index, item } => {
            if let Some(kind) = item.into_provider_start() {
                Ok(vec![ProviderEvent::ContentBlockStarted {
                    index: output_index,
                    kind,
                }])
            } else {
                state.ignored_output_indices.insert(output_index);
                Ok(Vec::new())
            }
        }
        ResponsesStreamEvent::ResponseOutputTextDelta {
            output_index,
            delta,
            ..
        } => {
            if state.ignored_output_indices.contains(&output_index) {
                return Ok(Vec::new());
            }

            state.text_delta_seen.insert(output_index);
            Ok(vec![ProviderEvent::ContentBlockDelta {
                index: output_index,
                delta: ContentBlockDelta::Text(delta),
            }])
        }
        ResponsesStreamEvent::ResponseReasoningSummaryTextDelta {
            delta,
            summary_index,
        } => Ok(vec![ProviderEvent::ReasoningSummaryDelta {
            delta,
            summary_index,
        }]),
        ResponsesStreamEvent::ResponseReasoningTextDelta {
            delta,
            content_index,
        } => Ok(vec![ProviderEvent::ReasoningContentDelta {
            delta,
            content_index,
        }]),
        ResponsesStreamEvent::ResponseReasoningSummaryPartAdded { summary_index } => {
            Ok(vec![ProviderEvent::ReasoningSummaryPartAdded {
                summary_index,
            }])
        }
        ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta {
            output_index,
            delta,
            ..
        } => {
            if state.ignored_output_indices.contains(&output_index) {
                return Ok(Vec::new());
            }

            state.function_delta_seen.insert(output_index);
            Ok(vec![ProviderEvent::ContentBlockDelta {
                index: output_index,
                delta: ContentBlockDelta::ToolUseInputJson(delta),
            }])
        }
        ResponsesStreamEvent::ResponseToolSearchCallDelta {
            output_index,
            delta,
        } => {
            if state.ignored_output_indices.contains(&output_index) {
                return Ok(Vec::new());
            }

            state.tool_search_delta_seen.insert(output_index);
            Ok(vec![ProviderEvent::ContentBlockDelta {
                index: output_index,
                delta: ContentBlockDelta::HostedToolSearchQuery(delta),
            }])
        }
        ResponsesStreamEvent::ResponseOutputItemDone { output_index, item } => {
            if state.ignored_output_indices.remove(&output_index) {
                return Ok(Vec::new());
            }

            let mut events = Vec::new();

            if !state.text_delta_seen.remove(&output_index)
                && let Some(text) = item.completed_text()
                && !text.is_empty()
            {
                events.push(ProviderEvent::ContentBlockDelta {
                    index: output_index,
                    delta: ContentBlockDelta::Text(text),
                });
            }

            if !state.function_delta_seen.remove(&output_index)
                && let Some(arguments) = item.completed_arguments()
                && !arguments.is_empty()
            {
                events.push(ProviderEvent::ContentBlockDelta {
                    index: output_index,
                    delta: ContentBlockDelta::ToolUseInputJson(arguments),
                });
            }

            if !state.tool_search_delta_seen.remove(&output_index)
                && let Some(query) = item.completed_tool_search_query()
                && !query.is_empty()
            {
                events.push(ProviderEvent::ContentBlockDelta {
                    index: output_index,
                    delta: ContentBlockDelta::HostedToolSearchQuery(query),
                });
            }

            events.extend(item.completion_deltas(output_index));

            if item.is_supported() {
                events.push(ProviderEvent::ContentBlockStopped {
                    index: output_index,
                });
            }

            Ok(events)
        }
        ResponsesStreamEvent::ResponseCompleted { response }
        | ResponsesStreamEvent::ResponseIncomplete { response } => Ok(vec![
            ProviderEvent::MessageDelta {
                stop_reason: response.stop_reason(),
                usage: response.usage(),
            },
            ProviderEvent::MessageStopped,
        ]),
        ResponsesStreamEvent::ResponseFailed { response } => {
            Err(ProviderError::MalformedStream(format!(
                "responses response failed{}",
                response
                    .error_message()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            )))
        }
        ResponsesStreamEvent::Error { message, error } => Err(ProviderError::MalformedStream(
            error
                .and_then(|error| error.message)
                .or(message)
                .unwrap_or_else(|| "responses stream error".to_string()),
        )),
        ResponsesStreamEvent::Unknown => Ok(Vec::new()),
    }
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
#[serde(tag = "type")]
enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated { response: ResponsesResponseEnvelope },
    #[serde(rename = "response.output_item.added")]
    ResponseOutputItemAdded {
        output_index: usize,
        item: ResponsesOutputItem,
    },
    #[serde(rename = "response.output_text.delta")]
    ResponseOutputTextDelta {
        output_index: usize,
        delta: String,
        #[allow(dead_code)]
        content_index: Option<usize>,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ResponseReasoningSummaryTextDelta { delta: String, summary_index: i64 },
    #[serde(rename = "response.reasoning_text.delta")]
    ResponseReasoningTextDelta { delta: String, content_index: i64 },
    #[serde(rename = "response.reasoning_summary_part.added")]
    ResponseReasoningSummaryPartAdded { summary_index: i64 },
    #[serde(rename = "response.function_call_arguments.delta")]
    ResponseFunctionCallArgumentsDelta {
        output_index: usize,
        delta: String,
        #[allow(dead_code)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.tool_search_call.delta")]
    ResponseToolSearchCallDelta { output_index: usize, delta: String },
    #[serde(rename = "response.output_item.done")]
    ResponseOutputItemDone {
        output_index: usize,
        item: ResponsesOutputItem,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: ResponsesResponseEnvelope },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete { response: ResponsesResponseEnvelope },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: ResponsesResponseEnvelope },
    Error {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error: Option<ResponsesErrorBody>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct ResponsesResponseEnvelope {
    id: String,
    model: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default)]
    error: Option<ResponsesErrorBody>,
}

impl ResponsesResponseEnvelope {
    fn stop_reason(&self) -> Option<String> {
        if let Some(details) = &self.incomplete_details
            && let Some(reason) = &details.reason
        {
            return Some(reason.clone());
        }

        match self.status.as_deref() {
            Some("completed") | Some("in_progress") => None,
            Some(status) => Some(status.to_string()),
            None => None,
        }
    }

    fn error_message(&self) -> Option<String> {
        self.error.as_ref().and_then(|error| error.message.clone())
    }

    fn usage(&self) -> Option<TokenUsage> {
        self.usage.as_ref().and_then(ResponsesUsage::to_token_usage)
    }
}

#[derive(Deserialize)]
struct ResponsesIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesErrorBody {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponsesOutputTokenDetails>,
}

impl ResponsesUsage {
    fn to_token_usage(&self) -> Option<TokenUsage> {
        let usage = TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            cache_read_input_tokens: self
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            cache_creation_input_tokens: None,
            reasoning_tokens: self
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens),
            thoughts_tokens: None,
            tool_input_tokens: None,
        };

        (!usage.is_empty()).then_some(usage)
    }
}

#[derive(Deserialize)]
struct ResponsesInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ResponsesOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesOutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<ResponsesMessageContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(rename = "tool_search_call")]
    ToolSearchCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        _execution: Option<String>,
        #[serde(default)]
        arguments: Option<serde_json::Value>,
    },
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        action: Option<WebSearchAction>,
    },
    #[serde(rename = "image_generation_call")]
    ImageGenerationCall {
        id: String,
        status: String,
        #[serde(default)]
        revised_prompt: Option<String>,
        #[serde(default)]
        result: Option<String>,
    },
    #[serde(other)]
    Unsupported,
}

impl ResponsesOutputItem {
    fn into_provider_start(self) -> Option<ContentBlockStart> {
        match self {
            ResponsesOutputItem::Message { .. } => Some(ContentBlockStart::Text),
            ResponsesOutputItem::FunctionCall { call_id, name, .. } => {
                Some(ContentBlockStart::ToolUse { id: call_id, name })
            }
            ResponsesOutputItem::ToolSearchCall {
                id,
                call_id,
                status,
                arguments,
                ..
            } => Some(ContentBlockStart::HostedToolSearch {
                call: HostedToolSearchCall {
                    id: call_id
                        .or(id)
                        .unwrap_or_else(|| "tool_search_call".to_string()),
                    status,
                    query: arguments
                        .as_ref()
                        .and_then(extract_tool_search_query_from_value),
                },
            }),
            ResponsesOutputItem::WebSearchCall { id, status, action } => {
                Some(ContentBlockStart::HostedWebSearch {
                    call: HostedWebSearchCall {
                        id: id.unwrap_or_else(|| "web_search_call".to_string()),
                        status,
                        action,
                    },
                })
            }
            ResponsesOutputItem::ImageGenerationCall {
                id,
                status,
                revised_prompt,
                result,
            } => Some(ContentBlockStart::ImageGeneration {
                call: ImageGenerationCall {
                    id,
                    status,
                    revised_prompt,
                    result: result.map(|result| ImageGenerationResult::ArtifactRef {
                        artifact_id: result,
                    }),
                },
            }),
            ResponsesOutputItem::Unsupported => None,
        }
    }

    fn completed_text(&self) -> Option<String> {
        match self {
            ResponsesOutputItem::Message { content } => {
                let text = content
                    .iter()
                    .filter_map(ResponsesMessageContent::text)
                    .collect::<Vec<_>>()
                    .join("");
                Some(text)
            }
            _ => None,
        }
    }

    fn completed_arguments(&self) -> Option<String> {
        match self {
            ResponsesOutputItem::FunctionCall { arguments, .. } => Some(arguments.clone()),
            _ => None,
        }
    }

    fn completed_tool_search_query(&self) -> Option<String> {
        match self {
            ResponsesOutputItem::ToolSearchCall { arguments, .. } => arguments
                .as_ref()
                .and_then(extract_tool_search_query_from_value),
            _ => None,
        }
    }

    fn completion_deltas(&self, output_index: usize) -> Vec<ProviderEvent> {
        match self {
            ResponsesOutputItem::ToolSearchCall { status, .. } => status
                .clone()
                .filter(|status| !status.is_empty())
                .map(|status| ProviderEvent::ContentBlockDelta {
                    index: output_index,
                    delta: ContentBlockDelta::HostedToolSearchStatus(status),
                })
                .into_iter()
                .collect(),
            ResponsesOutputItem::WebSearchCall { status, action, .. } => {
                let mut events = Vec::new();
                if let Some(action) = action.clone() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index: output_index,
                        delta: ContentBlockDelta::HostedWebSearchAction(action),
                    });
                }
                if let Some(status) = status.clone()
                    && !status.is_empty()
                {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index: output_index,
                        delta: ContentBlockDelta::HostedWebSearchStatus(status),
                    });
                }
                events
            }
            ResponsesOutputItem::ImageGenerationCall {
                status,
                revised_prompt,
                result,
                ..
            } => {
                let mut events = Vec::new();
                if let Some(revised_prompt) = revised_prompt.clone()
                    && !revised_prompt.is_empty()
                {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index: output_index,
                        delta: ContentBlockDelta::ImageGenerationRevisedPrompt(revised_prompt),
                    });
                }
                if let Some(result) = result.clone() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index: output_index,
                        delta: ContentBlockDelta::ImageGenerationResult(
                            ImageGenerationResult::ArtifactRef {
                                artifact_id: result,
                            },
                        ),
                    });
                }
                if !status.is_empty() {
                    events.push(ProviderEvent::ContentBlockDelta {
                        index: output_index,
                        delta: ContentBlockDelta::ImageGenerationStatus(status.clone()),
                    });
                }
                events
            }
            _ => Vec::new(),
        }
    }

    fn is_supported(&self) -> bool {
        !matches!(self, ResponsesOutputItem::Unsupported)
    }
}

fn extract_tool_search_query_from_value(value: &serde_json::Value) -> Option<String> {
    value
        .get("query")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesMessageContent {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(other)]
    Unsupported,
}

impl ResponsesMessageContent {
    fn text(&self) -> Option<String> {
        match self {
            ResponsesMessageContent::OutputText { text }
            | ResponsesMessageContent::InputText { text } => Some(text.clone()),
            ResponsesMessageContent::Unsupported => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use http::HeaderValue;

    use crate::{ContentBlockDelta, ContentBlockStart, ProviderEvent, Role, TokenUsage};

    use super::{StreamState, parse_frame, response_headers_event};

    #[test]
    fn emits_response_headers_event_for_metadata_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("openai-model", HeaderValue::from_static("gpt-5"));
        headers.insert("x-models-etag", HeaderValue::from_static("etag-123"));
        headers.insert(
            "x-ratelimit-limit-requests",
            HeaderValue::from_static("1000"),
        );

        let event = response_headers_event(&headers).expect("headers event should be emitted");
        assert_eq!(
            event,
            ProviderEvent::ResponseHeaders(crate::ResponseHeaders {
                values: vec![
                    ("openai-model".to_string(), "gpt-5".to_string()),
                    ("x-models-etag".to_string(), "etag-123".to_string()),
                    ("x-ratelimit-limit-requests".to_string(), "1000".to_string()),
                ],
            })
        );
    }

    #[test]
    fn parses_reasoning_delta_stream_events() {
        let mut state = StreamState::default();

        let summary_delta = parse_frame(
            br#"data: {"type":"response.reasoning_summary_text.delta","summary_index":2,"delta":"short summary"}"#,
            &mut state,
        )
        .expect("reasoning summary delta should parse");
        assert_eq!(
            summary_delta,
            vec![ProviderEvent::ReasoningSummaryDelta {
                delta: "short summary".to_string(),
                summary_index: 2,
            }]
        );

        let part_added = parse_frame(
            br#"data: {"type":"response.reasoning_summary_part.added","summary_index":2}"#,
            &mut state,
        )
        .expect("reasoning summary part should parse");
        assert_eq!(
            part_added,
            vec![ProviderEvent::ReasoningSummaryPartAdded { summary_index: 2 }]
        );

        let reasoning_delta = parse_frame(
            br#"data: {"type":"response.reasoning_text.delta","content_index":7,"delta":"internal chain"}"#,
            &mut state,
        )
        .expect("reasoning content delta should parse");
        assert_eq!(
            reasoning_delta,
            vec![ProviderEvent::ReasoningContentDelta {
                delta: "internal chain".to_string(),
                content_index: 7,
            }]
        );
    }

    #[test]
    fn parses_tool_call_stream_events() {
        let mut state = StreamState::default();

        let created = parse_frame(
            br#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5","status":"in_progress"}}"#,
            &mut state,
        )
        .expect("created event should parse");
        assert_eq!(
            created,
            vec![
                ProviderEvent::ResponseCreated,
                ProviderEvent::MessageStarted {
                    id: "resp_1".to_string(),
                    model: "gpt-5".to_string(),
                    role: Role::Assistant,
                },
            ]
        );

        let added = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":""}}"#,
            &mut state,
        )
        .expect("tool call start should parse");
        assert_eq!(
            added,
            vec![ProviderEvent::ContentBlockStarted {
                index: 0,
                kind: ContentBlockStart::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                },
            }]
        );

        let delta = parse_frame(
            br#"data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"README.md\"}"}"#,
            &mut state,
        )
        .expect("tool arguments delta should parse");
        assert_eq!(
            delta,
            vec![ProviderEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolUseInputJson("{\"path\":\"README.md\"}".to_string()),
            }]
        );

        let done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}}"#,
            &mut state,
        )
        .expect("tool call completion should parse");
        assert_eq!(done, vec![ProviderEvent::ContentBlockStopped { index: 0 }]);
    }

    #[test]
    fn falls_back_to_completed_message_text_when_no_text_delta_arrives() {
        let mut state = StreamState::default();

        let _ = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"message","content":[]}}"#,
            &mut state,
        )
        .expect("message start should parse");

        let done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":"Hello"}]}}"#,
            &mut state,
        )
        .expect("message completion should parse");
        assert_eq!(
            done,
            vec![
                ProviderEvent::ContentBlockDelta {
                    index: 1,
                    delta: ContentBlockDelta::Text("Hello".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 1 },
            ]
        );

        let completed = parse_frame(
            br#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed"}}"#,
            &mut state,
        )
        .expect("completion should parse");
        assert_eq!(
            completed,
            vec![
                ProviderEvent::MessageDelta {
                    stop_reason: None,
                    usage: None,
                },
                ProviderEvent::MessageStopped,
            ]
        );
    }

    #[test]
    fn parses_final_usage_from_completed_response() {
        let mut state = StreamState::default();

        let completed = parse_frame(
            br#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed","usage":{"input_tokens":328,"input_tokens_details":{"cached_tokens":12},"output_tokens":52,"output_tokens_details":{"reasoning_tokens":7},"total_tokens":380}}}"#,
            &mut state,
        )
        .expect("completion should parse");

        assert_eq!(
            completed,
            vec![
                ProviderEvent::MessageDelta {
                    stop_reason: None,
                    usage: Some(TokenUsage {
                        input_tokens: Some(328),
                        output_tokens: Some(52),
                        total_tokens: Some(380),
                        cache_read_input_tokens: Some(12),
                        cache_creation_input_tokens: None,
                        reasoning_tokens: Some(7),
                        thoughts_tokens: None,
                        tool_input_tokens: None,
                    }),
                },
                ProviderEvent::MessageStopped,
            ]
        );
    }

    #[test]
    fn parses_hosted_tool_search_output_items() {
        let mut state = StreamState::default();

        let added = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"tool_search_call","id":"search_1","status":"in_progress"}}"#,
            &mut state,
        )
        .expect("hosted search start should parse");
        assert_eq!(
            added,
            vec![ProviderEvent::ContentBlockStarted {
                index: 3,
                kind: ContentBlockStart::HostedToolSearch {
                    call: crate::HostedToolSearchCall {
                        id: "search_1".to_string(),
                        status: Some("in_progress".to_string()),
                        query: None,
                    },
                },
            }]
        );

        let delta = parse_frame(
            br#"data: {"type":"response.tool_search_call.delta","output_index":3,"delta":"weather"}"#,
            &mut state,
        )
        .expect("hosted search delta should parse");
        assert_eq!(
            delta,
            vec![ProviderEvent::ContentBlockDelta {
                index: 3,
                delta: ContentBlockDelta::HostedToolSearchQuery("weather".to_string()),
            }]
        );

        let done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":3,"item":{"type":"tool_search_call","id":"search_1","status":"completed","arguments":{"query":"weather"}}}"#,
            &mut state,
        )
        .expect("hosted search completion should parse");
        assert_eq!(
            done,
            vec![
                ProviderEvent::ContentBlockDelta {
                    index: 3,
                    delta: ContentBlockDelta::HostedToolSearchStatus("completed".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 3 },
            ]
        );
    }

    #[test]
    fn parses_web_search_and_image_generation_output_items() {
        let mut state = StreamState::default();

        let added = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":4,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}"#,
            &mut state,
        )
        .expect("web search start should parse");
        assert_eq!(
            added,
            vec![ProviderEvent::ContentBlockStarted {
                index: 4,
                kind: ContentBlockStart::HostedWebSearch {
                    call: crate::HostedWebSearchCall {
                        id: "ws_1".to_string(),
                        status: Some("in_progress".to_string()),
                        action: None,
                    },
                },
            }]
        );

        let done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":4,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"weather seattle"}}}"#,
            &mut state,
        )
        .expect("web search done should parse");
        assert_eq!(
            done,
            vec![
                ProviderEvent::ContentBlockDelta {
                    index: 4,
                    delta: ContentBlockDelta::HostedWebSearchAction(
                        crate::WebSearchAction::Search {
                            query: Some("weather seattle".to_string()),
                            queries: None,
                        }
                    ),
                },
                ProviderEvent::ContentBlockDelta {
                    index: 4,
                    delta: ContentBlockDelta::HostedWebSearchStatus("completed".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 4 },
            ]
        );

        let image_added = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":5,"item":{"type":"image_generation_call","id":"ig_1","status":"in_progress"}}"#,
            &mut state,
        )
        .expect("image generation start should parse");
        assert_eq!(
            image_added,
            vec![ProviderEvent::ContentBlockStarted {
                index: 5,
                kind: ContentBlockStart::ImageGeneration {
                    call: crate::ImageGenerationCall {
                        id: "ig_1".to_string(),
                        status: "in_progress".to_string(),
                        revised_prompt: None,
                        result: None,
                    },
                },
            }]
        );

        let image_done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":5,"item":{"type":"image_generation_call","id":"ig_1","status":"completed","revised_prompt":"A blue square","result":"artifact_1"}}"#,
            &mut state,
        )
        .expect("image generation done should parse");
        assert_eq!(
            image_done,
            vec![
                ProviderEvent::ContentBlockDelta {
                    index: 5,
                    delta: ContentBlockDelta::ImageGenerationRevisedPrompt(
                        "A blue square".to_string()
                    ),
                },
                ProviderEvent::ContentBlockDelta {
                    index: 5,
                    delta: ContentBlockDelta::ImageGenerationResult(
                        crate::ImageGenerationResult::ArtifactRef {
                            artifact_id: "artifact_1".to_string(),
                        }
                    ),
                },
                ProviderEvent::ContentBlockDelta {
                    index: 5,
                    delta: ContentBlockDelta::ImageGenerationStatus("completed".to_string()),
                },
                ProviderEvent::ContentBlockStopped { index: 5 },
            ]
        );
    }
}
