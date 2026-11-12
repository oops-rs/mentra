use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use serde_json::{Value, json};

use super::*;

fn server(status: &str, headers: &str, body: &str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("local address");
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).expect("read request");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..n]);
            if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = index + 4;
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .unwrap_or_default();
                if request.len() >= end + length {
                    break;
                }
            }
        }
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        String::from_utf8(request).expect("UTF-8 request")
    });
    (format!("http://{address}"), handle)
}

fn client(base_url: String) -> DecisionsClient<StaticCredentialSource> {
    let mut definition = crate::responses::openai_definition();
    definition.base_url = Some(base_url);
    DecisionsClient::with_definition(definition, StaticCredentialSource::new("test-key"))
}

fn usage() -> Value {
    json!({
        "input_tokens": 42,
        "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 4},
        "output_tokens": 0,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 42
    })
}

#[tokio::test]
async fn sends_conversation_questions_and_preserves_mixed_answers() {
    let body = json!({
        "model": "gpt-6-luna",
        "answers": [
            {"type":"predicate", "name":"urgent", "probability":0.9},
            {"type":"choice", "name":"team", "choice":true, "confidence":0.7,
             "probabilities":[{"value":true,"probability":0.8},{"value":"true","probability":0.2}]},
            {"type":"score", "name":"severity", "score":0.3, "confidence":0.6,
             "probabilities":[{"value":0,"label":"low","probability":0.7},{"value":1,"label":"high","probability":0.3}]},
            {"type":"refusal", "name":"refused"}
        ],
        "usage": usage()
    });
    let (url, server) = server("200 OK", "", &body.to_string());
    let input = DecisionInput::Items(vec![
        DecisionInputItem::Message(DecisionMessage::User {
            content: DecisionUserContent::Parts(vec![
                DecisionInputPart::InputText {
                    text: "Inspect this order".into(),
                    prompt_cache_breakpoint: Some(DecisionCacheBreakpoint {
                        mode: DecisionExplicitCacheMode::Explicit,
                    }),
                },
                DecisionInputPart::InputJson {
                    data: json!({"order": 123}),
                },
                DecisionInputPart::InputImage {
                    image_url: "https://example.com/item.png".into(),
                    detail: Some(DecisionImageDetail::High),
                    prompt_cache_breakpoint: None,
                },
            ]),
        }),
        DecisionInputItem::Tool(DecisionToolItem::FunctionCall {
            arguments: "{}".into(),
            call_id: "call-1".into(),
            name: "lookup".into(),
            id: None,
            namespace: None,
            status: None,
        }),
        DecisionInputItem::Tool(DecisionToolItem::FunctionCallOutput {
            call_id: "call-1".into(),
            output: DecisionToolOutput::Text("found".into()),
            id: None,
            status: None,
        }),
    ]);
    let request = DecisionRequest::new(
        DEFAULT_DECISIONS_MODEL,
        input,
        vec![
            DecisionQuestion::predicate("urgent", "Is the order urgent?"),
            DecisionQuestion::Choice {
                name: Some("team".into()),
                instructions: DecisionInstructions::Text("Choose".into()),
                choices: vec![
                    DecisionChoice {
                        value: DecisionChoiceValue::Boolean(true),
                        description: None,
                    },
                    DecisionChoice {
                        value: DecisionChoiceValue::String("true".into()),
                        description: None,
                    },
                ],
            },
            DecisionQuestion::Score {
                name: Some("severity".into()),
                instructions: DecisionInstructions::Text("Rate".into()),
                levels: vec![
                    DecisionScoreLevel {
                        label: "low".into(),
                        description: None,
                    },
                    DecisionScoreLevel {
                        label: "high".into(),
                        description: None,
                    },
                ],
            },
            DecisionQuestion::predicate("refused", "Another question"),
        ],
    );
    let response = client(url)
        .create(&request)
        .await
        .expect("decision response");
    assert!(matches!(
        response.answers[1],
        DecisionAnswer::Choice {
            choice: DecisionChoiceValue::Boolean(true),
            ..
        }
    ));
    assert!(matches!(response.answers[2], DecisionAnswer::Score { score, .. } if score == 0.3));
    assert!(matches!(
        response.answers[3],
        DecisionAnswer::Refusal { .. }
    ));
    assert_eq!(response.usage.input_tokens_details.cached_tokens, 20);
    assert_eq!(response.usage.input_tokens_details.cache_write_tokens, 4);

    let raw = server.join().expect("server thread");
    assert!(raw.starts_with("POST /v1/decisions "));
    assert!(raw.contains("authorization: Bearer test-key"));
    let payload: Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["input"][0]["content"][1]["data"]["order"], 123);
    assert_eq!(
        payload["input"][1]["call_id"],
        payload["input"][2]["call_id"]
    );
    assert_eq!(payload["questions"][1]["choices"][0]["value"], true);
    assert_eq!(payload["questions"][1]["choices"][1]["value"], "true");
}

#[tokio::test]
async fn http_errors_preserve_retry_after() {
    let (url, server) = server(
        "429 Too Many Requests",
        "retry-after: 7\r\n",
        "rate limited",
    );
    let request = DecisionRequest::new(
        DEFAULT_DECISIONS_MODEL,
        "text",
        vec![DecisionQuestion::predicate("p", "Question?")],
    );
    let error = client(url)
        .create(&request)
        .await
        .expect_err("rate limited");
    assert!(matches!(
        error,
        ProviderError::Http {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            ..
        }
    ));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    server.join().unwrap();
}

#[tokio::test]
async fn rejects_a_success_response_for_a_different_question() {
    let body = json!({
        "model": "gpt-6-luna",
        "answers": [{"type":"predicate", "name":"other", "probability":0.9}],
        "usage": usage()
    });
    let (url, server) = server("200 OK", "", &body.to_string());
    let request = DecisionRequest::new(
        DEFAULT_DECISIONS_MODEL,
        "text",
        vec![DecisionQuestion::predicate("expected", "Question?")],
    );
    let error = client(url)
        .create(&request)
        .await
        .expect_err("a mismatched answer cannot be used");
    assert!(matches!(error, ProviderError::InvalidResponse(_)));
    server.join().unwrap();
}

#[test]
fn rejects_wrong_answer_count_name_and_type() {
    let request = DecisionRequest::new(
        DEFAULT_DECISIONS_MODEL,
        "text",
        vec![DecisionQuestion::predicate("expected", "Question?")],
    );
    for answers in [
        json!([]),
        json!([{"type":"predicate","name":"other","probability":0.5}]),
        json!([{"type":"choice","name":"expected","choice":"x","confidence":0.5,"probabilities":[]}]),
    ] {
        let response: DecisionResponse =
            serde_json::from_value(json!({"model":"x","answers":answers,"usage":usage()})).unwrap();
        assert!(matches!(
            response.validate_against(&request),
            Err(ProviderError::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn rejects_invalid_question_counts_before_network_io() {
    let request = DecisionRequest::new(DEFAULT_DECISIONS_MODEL, "text", Vec::new());
    let error = DecisionsClient::new("test-key")
        .create(&request)
        .await
        .expect_err("empty questions");
    assert!(matches!(error, ProviderError::InvalidRequest(_)));
}
