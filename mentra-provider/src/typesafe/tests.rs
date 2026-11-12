use std::{
    collections::BTreeMap,
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

fn client(base_url: String) -> SystemOneClient<StaticCredentialSource> {
    let mut definition = ProviderDefinition::new("typesafe-test");
    definition.auth_scheme = AuthScheme::BearerToken;
    definition.base_url = Some(base_url);
    SystemOneClient::with_definition(definition, StaticCredentialSource::new("test-key"))
}

#[tokio::test]
async fn evaluates_native_questions_with_structured_state_and_image_opt_in() {
    let body = json!({
        "model":"jev-1.13.0",
        "answers": {
            "urgent": {"type":"noul","noul":0.9},
            "team": {"type":"choice","choice":"billing","confidence":0.8,
                     "probabilities":{"billing":0.9,"other":0.1}},
            "severity": {"type":"score","score":0.25,"confidence":0.6,
                         "legend":{"0":"low","1":{"description":"high"}},
                         "probabilities":{"0":0.75,"1":0.25}},
            "locate": {"type":"box","box":{"top":0.1,"bottom":0.8,"left":0.2,"right":0.7},"confidence":null}
        },
        "usage":{"input_tokens":428,"output_tokens":73}
    });
    let (url, server) = server("200 OK", "", &body.to_string());
    let questions = BTreeMap::from([
        ("urgent".into(), SystemOneQuestion::noul("Is it urgent?")),
        (
            "team".into(),
            SystemOneQuestion::Choice {
                instructions: Some(json!({"question":"Select a team"})),
                criteria: BTreeMap::from([
                    ("billing".into(), json!({"description":"Payments"})),
                    ("other".into(), Value::Null),
                ]),
            },
        ),
        (
            "severity".into(),
            SystemOneQuestion::Score {
                instructions: Some(json!("Rate severity")),
                criteria: vec![json!("low"), json!({"description":"high"})],
            },
        ),
        (
            "locate".into(),
            SystemOneQuestion::Box {
                instructions: Some(json!("Locate the object")),
            },
        ),
    ]);
    let mut request = SystemOneRequest::new(
        DEFAULT_JEV_MODEL,
        json!({"image":"photo", "ticket":"broken"}),
        questions,
    );
    request.assets.insert(
        "photo".into(),
        SystemOneAsset::image(SystemOneImageFormat::Png, b"image-bytes"),
    );
    let response = client(url)
        .with_images_enabled()
        .with_long_context_enabled()
        .evaluate(&request)
        .await
        .expect("evaluation");
    assert_eq!(response.model, "jev-1.13.0");
    assert_eq!(response.usage.input_tokens, 428);
    assert!(matches!(response.answers["urgent"], SystemOneAnswer::Noul { noul } if noul == 0.9));
    assert!(matches!(
        response.answers["locate"],
        SystemOneAnswer::Box {
            confidence: None,
            ..
        }
    ));
    assert!(
        matches!(&response.answers["severity"], SystemOneAnswer::Score { legend, .. } if legend["1"]["description"] == "high")
    );

    let raw = server.join().expect("server thread");
    assert!(raw.starts_with("POST /v1/systemone "));
    assert!(raw.contains("authorization: Bearer test-key"));
    assert!(raw.contains("typesafe-danger-zone-images: true"));
    assert!(raw.contains("typesafe-danger-zone-long-context: true"));
    let payload: Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        payload["assets"]["photo"],
        json!({"type":"image","format":"png","data_base64":"aW1hZ2UtYnl0ZXM="})
    );
    assert_eq!(
        payload["questions"]["team"]["criteria"]["billing"]["description"],
        "Payments"
    );
    assert_eq!(payload["questions"]["locate"]["type"], "box");
}

#[tokio::test]
async fn rejects_image_and_box_requests_without_opt_in_before_network_io() {
    let mut request = SystemOneRequest::new(
        DEFAULT_JEV_MODEL,
        "photo",
        BTreeMap::from([(
            "box".into(),
            SystemOneQuestion::Box {
                instructions: Some(json!("Locate")),
            },
        )]),
    );
    let client = SystemOneClient::new("test-key");
    assert!(matches!(
        client.evaluate(&request).await,
        Err(ProviderError::InvalidRequest(_))
    ));
    request.questions = BTreeMap::from([("p".into(), SystemOneQuestion::noul("Question?"))]);
    request.assets.insert(
        "photo".into(),
        SystemOneAsset::image(SystemOneImageFormat::Jpeg, b"image"),
    );
    assert!(matches!(
        client.evaluate(&request).await,
        Err(ProviderError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn rejects_a_success_response_for_a_different_question() {
    let body = json!({"model":"jev-latest", "answers":{"other":{"type":"noul","noul":0.9}}, "usage":{"input_tokens":5,"output_tokens":1}});
    let (url, server) = server("200 OK", "", &body.to_string());
    let request = SystemOneRequest::new(
        DEFAULT_JEV_MODEL,
        "text",
        BTreeMap::from([("expected".into(), SystemOneQuestion::noul("Question?"))]),
    );
    let error = client(url)
        .evaluate(&request)
        .await
        .expect_err("a mismatched answer cannot be used");
    assert!(matches!(error, ProviderError::InvalidResponse(_)));
    let raw = server.join().unwrap();
    assert!(!raw.contains("typesafe-danger-zone-images"));
    assert!(!raw.contains("typesafe-danger-zone-long-context"));
}

#[tokio::test]
async fn lists_the_models_available_to_the_account() {
    let body = json!({"models":[{"name":"jev-latest","description":"model alias","release_date":"2026-09-15"}]});
    let (url, server) = server("200 OK", "", &body.to_string());
    let models = client(url).list_models().await.expect("models");
    assert_eq!(models[0].name, "jev-latest");
    let raw = server.join().unwrap();
    assert!(raw.starts_with("GET /v1/models "));
    assert!(raw.contains("authorization: Bearer test-key"));
}

#[tokio::test]
async fn http_errors_preserve_retry_after() {
    let (url, server) = server(
        "429 Too Many Requests",
        "retry-after: 9\r\n",
        "rate limited",
    );
    let request = SystemOneRequest::new(
        DEFAULT_JEV_MODEL,
        "text",
        BTreeMap::from([("p".into(), SystemOneQuestion::noul("Question?"))]),
    );
    let error = client(url)
        .evaluate(&request)
        .await
        .expect_err("rate limited");
    assert!(matches!(
        error,
        ProviderError::Http {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            ..
        }
    ));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(9)));
    server.join().unwrap();
}
