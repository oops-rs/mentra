use std::collections::HashSet;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::provider::model::{
    ContentBlockDelta, ContentBlockStart, ProviderError, ProviderEvent, ProviderEventStream, Role,
    TokenUsage,
};

pub(crate) fn spawn_event_stream(response: reqwest::Response) -> ProviderEventStream {
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

#[derive(Default)]
struct StreamState {
    ignored_output_indices: HashSet<usize>,
    text_delta_seen: HashSet<usize>,
    function_delta_seen: HashSet<usize>,
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
    if data == "[DONE]" {
        return Ok(Vec::new());
    }

    let event: OpenAIStreamEvent =
        serde_json::from_str(&data).map_err(ProviderError::Deserialize)?;

    match event {
        OpenAIStreamEvent::ResponseCreated { response } => {
            Ok(vec![ProviderEvent::MessageStarted {
                id: response.id,
                model: response.model,
                role: Role::Assistant,
            }])
        }
        OpenAIStreamEvent::ResponseOutputItemAdded { output_index, item } => {
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
        OpenAIStreamEvent::ResponseOutputTextDelta {
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
        OpenAIStreamEvent::ResponseFunctionCallArgumentsDelta {
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
        OpenAIStreamEvent::ResponseOutputItemDone { output_index, item } => {
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

            if item.is_supported() {
                events.push(ProviderEvent::ContentBlockStopped {
                    index: output_index,
                });
            }

            Ok(events)
        }
        OpenAIStreamEvent::ResponseCompleted { response }
        | OpenAIStreamEvent::ResponseIncomplete { response } => Ok(vec![
            ProviderEvent::MessageDelta {
                stop_reason: response.stop_reason(),
                usage: response.usage(),
            },
            ProviderEvent::MessageStopped,
        ]),
        OpenAIStreamEvent::ResponseFailed { response } => {
            Err(ProviderError::MalformedStream(format!(
                "openai response failed{}",
                response
                    .error_message()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            )))
        }
        OpenAIStreamEvent::Error { message, error } => Err(ProviderError::MalformedStream(
            error
                .and_then(|error| error.message)
                .or(message)
                .unwrap_or_else(|| "openai stream error".to_string()),
        )),
        OpenAIStreamEvent::Unknown => Ok(Vec::new()),
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
enum OpenAIStreamEvent {
    #[serde(rename = "response.created")]
    ResponseCreated { response: OpenAIResponseEnvelope },
    #[serde(rename = "response.output_item.added")]
    ResponseOutputItemAdded {
        output_index: usize,
        item: OpenAIOutputItem,
    },
    #[serde(rename = "response.output_text.delta")]
    ResponseOutputTextDelta {
        output_index: usize,
        delta: String,
        #[allow(dead_code)]
        content_index: Option<usize>,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    ResponseFunctionCallArgumentsDelta {
        output_index: usize,
        delta: String,
        #[allow(dead_code)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_item.done")]
    ResponseOutputItemDone {
        output_index: usize,
        item: OpenAIOutputItem,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: OpenAIResponseEnvelope },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete { response: OpenAIResponseEnvelope },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: OpenAIResponseEnvelope },
    Error {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        error: Option<OpenAIErrorBody>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct OpenAIResponseEnvelope {
    id: String,
    model: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
    #[serde(default)]
    incomplete_details: Option<OpenAIIncompleteDetails>,
    #[serde(default)]
    error: Option<OpenAIErrorBody>,
}

impl OpenAIResponseEnvelope {
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
        self.usage.as_ref().and_then(OpenAIUsage::to_token_usage)
    }
}

#[derive(Deserialize)]
struct OpenAIIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAIErrorBody {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<OpenAIInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<OpenAIOutputTokenDetails>,
}

impl OpenAIUsage {
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
struct OpenAIInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OpenAIOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum OpenAIOutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        content: Vec<OpenAIMessageContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Unsupported,
}

impl OpenAIOutputItem {
    fn into_provider_start(self) -> Option<ContentBlockStart> {
        match self {
            OpenAIOutputItem::Message { .. } => Some(ContentBlockStart::Text),
            OpenAIOutputItem::FunctionCall { call_id, name, .. } => {
                Some(ContentBlockStart::ToolUse { id: call_id, name })
            }
            OpenAIOutputItem::Unsupported => None,
        }
    }

    fn completed_text(&self) -> Option<String> {
        match self {
            OpenAIOutputItem::Message { content } => {
                let text = content
                    .iter()
                    .filter_map(OpenAIMessageContent::text)
                    .collect::<Vec<_>>()
                    .join("");
                Some(text)
            }
            _ => None,
        }
    }

    fn completed_arguments(&self) -> Option<String> {
        match self {
            OpenAIOutputItem::FunctionCall { arguments, .. } => Some(arguments.clone()),
            _ => None,
        }
    }

    fn is_supported(&self) -> bool {
        !matches!(self, OpenAIOutputItem::Unsupported)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum OpenAIMessageContent {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(other)]
    Unsupported,
}

impl OpenAIMessageContent {
    fn text(&self) -> Option<String> {
        match self {
            OpenAIMessageContent::OutputText { text }
            | OpenAIMessageContent::InputText { text } => Some(text.clone()),
            OpenAIMessageContent::Unsupported => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::provider::model::{
        ContentBlockDelta, ContentBlockStart, ProviderEvent, Role, TokenUsage,
    };

    use super::{StreamState, parse_frame};

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
            vec![ProviderEvent::MessageStarted {
                id: "resp_1".to_string(),
                model: "gpt-5".to_string(),
                role: Role::Assistant,
            }]
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
    fn ignores_hosted_tool_search_output_items() {
        let mut state = StreamState::default();

        let added = parse_frame(
            br#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"tool_search_call","id":"search_1","status":"in_progress"}}"#,
            &mut state,
        )
        .expect("hosted search start should parse");
        assert!(added.is_empty());

        let delta = parse_frame(
            br#"data: {"type":"response.tool_search_call.delta","output_index":3,"delta":"ignored"}"#,
            &mut state,
        )
        .expect("hosted search delta should parse");
        assert!(delta.is_empty());

        let done = parse_frame(
            br#"data: {"type":"response.output_item.done","output_index":3,"item":{"type":"tool_search_call","id":"search_1","status":"completed"}}"#,
            &mut state,
        )
        .expect("hosted search completion should parse");
        assert!(done.is_empty());
    }
}
