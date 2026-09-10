//! The ring as a provider: which member each call reaches, and what the
//! runtime above it sees. The policy's own arithmetic is pinned in
//! `health::tests`; these prove the wiring around it.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use tokio::sync::mpsc;

use super::*;
use crate::ProviderDescriptor;
use crate::ProviderRequestOptions;

/// A member that answers from a script: `Ok` streams an empty response,
/// `Err` fails with the scripted error. Once the script runs out it keeps
/// answering with its last entry, so "healthy" and "dead" are one line each.
#[derive(Clone)]
struct ScriptedMember {
    definition: ProviderDefinition,
    script: Arc<Mutex<VecDeque<Result<(), ProviderError>>>>,
    calls: Arc<AtomicUsize>,
    scopes: Arc<AtomicUsize>,
}

impl ScriptedMember {
    fn new(label: &str, script: Vec<Result<(), ProviderError>>) -> Self {
        let mut definition = ProviderDefinition::new("gateway");
        definition.descriptor.display_name = Some(label.to_string());
        definition.base_url = Some(format!("http://{label}/"));
        Self {
            definition,
            script: Arc::new(Mutex::new(script.into())),
            calls: Arc::new(AtomicUsize::new(0)),
            scopes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn healthy(label: &str) -> Self {
        Self::new(label, vec![Ok(())])
    }

    fn dead(label: &str, error: fn() -> ProviderError) -> Self {
        Self::new(label, vec![Err(error())])
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn scopes(&self) -> usize {
        self.scopes.load(Ordering::SeqCst)
    }

    fn answer(&self) -> Result<(), ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().expect("script");
        let next = if script.len() > 1 {
            script.pop_front()
        } else {
            script.front().map(clone_result)
        };
        next.expect("a scripted member has at least one answer")
    }

    fn shared(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn clone_result(result: &Result<(), ProviderError>) -> Result<(), ProviderError> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(clone_error(error)),
    }
}

/// `ProviderError` is not `Clone` (it wraps reqwest errors); the variants
/// these tests script are.
fn clone_error(error: &ProviderError) -> ProviderError {
    match error {
        ProviderError::Retryable { message, delay } => ProviderError::Retryable {
            message: message.clone(),
            delay: *delay,
        },
        ProviderError::Http {
            status,
            body,
            retry_after,
        } => ProviderError::Http {
            status: *status,
            body: body.clone(),
            retry_after: *retry_after,
        },
        ProviderError::ContextLengthExceeded { status, body } => {
            ProviderError::ContextLengthExceeded {
                status: *status,
                body: body.clone(),
            }
        }
        ProviderError::InvalidRequest(message) => ProviderError::InvalidRequest(message.clone()),
        ProviderError::MalformedStream(message) => ProviderError::MalformedStream(message.clone()),
        ProviderError::UnsupportedCapability(name) => {
            ProviderError::UnsupportedCapability(name.clone())
        }
        other => panic!("tests do not script {other:?}"),
    }
}

#[async_trait]
impl Provider for ScriptedMember {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.answer()?;
        Ok(vec![ModelInfo::new("test-model", "gateway")])
    }

    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new(self.clone()))
    }

    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        self.scopes.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderSessionScope::new(self.clone()))
    }

    async fn stream(&self, _request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.answer()?;
        let (_tx, rx) = mpsc::unbounded_channel();
        Ok(rx)
    }
}

#[async_trait]
impl ProviderSession for ScriptedMember {
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        Provider::stream(self, request).await
    }
}

fn retryable() -> ProviderError {
    ProviderError::Retryable {
        message: "upstream overloaded".to_string(),
        delay: None,
    }
}

fn http(status: StatusCode) -> ProviderError {
    ProviderError::Http {
        status,
        body: String::new(),
        retry_after: None,
    }
}

fn unauthorized() -> ProviderError {
    http(StatusCode::UNAUTHORIZED)
}

fn too_long() -> ProviderError {
    ProviderError::ContextLengthExceeded {
        status: StatusCode::BAD_REQUEST,
        body: String::new(),
    }
}

fn request() -> Request<'static> {
    Request {
        model: Cow::Borrowed("test-model"),
        system: None,
        messages: Cow::Owned(Vec::new()),
        tools: Cow::Owned(Vec::new()),
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        metadata: Cow::Owned(Default::default()),
        provider_request_options: ProviderRequestOptions::default(),
    }
}

fn policy(threshold: u32, cooldown: Option<Duration>) -> GatewayRingPolicy {
    GatewayRingPolicy {
        failure_threshold: threshold,
        cooldown,
    }
}

fn ring(members: &[&ScriptedMember], policy: GatewayRingPolicy) -> GatewayRing {
    GatewayRing::new(members.iter().map(|member| member.shared()), policy).expect("members")
}

/// Records every event the ring reports.
fn recording(ring: GatewayRing) -> (GatewayRing, Arc<Mutex<Vec<GatewayRingEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let ring = ring.with_observer(move |event| sink.lock().expect("events").push(event));
    (ring, events)
}

fn labels(events: &[GatewayRingEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            GatewayRingEvent::MemberFailed {
                member,
                consecutive_failures,
                ..
            } => format!("failed {} x{consecutive_failures}", member.label),
            GatewayRingEvent::Rotated {
                from,
                to,
                after_failures,
            } => format!("rotated {}→{} after {after_failures}", from.label, to.label),
            GatewayRingEvent::Returned { member } => format!("returned {}", member.label),
            GatewayRingEvent::Recovered { member } => format!("recovered {}", member.label),
        })
        .collect()
}

#[test]
fn an_empty_ring_is_refused() {
    assert_eq!(
        GatewayRing::new(Vec::new(), GatewayRingPolicy::default()).err(),
        Some(EmptyGatewayRing)
    );
}

#[test]
fn the_ring_carries_the_preferred_members_identity() {
    let a = ScriptedMember::healthy("a");
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], GatewayRingPolicy::default());

    let definition = ring.definition();
    assert_eq!(definition.descriptor.id, a.definition.descriptor.id);
    assert_eq!(definition.base_url, a.definition.base_url);
    assert_eq!(definition.wire_api, a.definition.wire_api);
    assert_eq!(
        definition.descriptor.display_name.as_deref(),
        Some("gateway ring: a → b")
    );
    assert_eq!(
        ring.members()
            .into_iter()
            .map(|member| member.label)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn a_member_is_labelled_by_what_it_has() {
    let mut definition = ProviderDefinition::new("only-id");
    let by_id = ScriptedMember {
        definition: definition.clone(),
        ..ScriptedMember::healthy("x")
    };
    definition.base_url = Some("http://by-url/".to_string());
    let by_url = ScriptedMember {
        definition,
        ..ScriptedMember::healthy("x")
    };
    let ring = ring(&[&by_id, &by_url], GatewayRingPolicy::default());

    let descriptor: ProviderDescriptor = ring.descriptor();
    assert_eq!(
        descriptor.display_name.as_deref(),
        Some("gateway ring: only-id → http://by-url/")
    );
}

#[tokio::test]
async fn the_preferred_member_answers_while_it_works() {
    let a = ScriptedMember::healthy("a");
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], GatewayRingPolicy::default());

    for _ in 0..3 {
        ring.stream(request()).await.expect("a answers");
    }

    assert_eq!((a.calls(), b.calls()), (3, 0));
    assert_eq!(ring.active().index, 0);
}

#[tokio::test]
async fn failures_short_of_the_threshold_surface_and_stay_on_the_member() {
    let a = ScriptedMember::dead("a", retryable);
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], policy(3, None));

    for _ in 0..2 {
        let error = ring.stream(request()).await.expect_err("a fails");
        assert!(matches!(error, ProviderError::Retryable { .. }));
    }

    assert_eq!((a.calls(), b.calls()), (2, 0));
    assert_eq!(ring.active().index, 0);
}

#[tokio::test]
async fn the_threshold_rotates_and_finishes_the_same_call_on_the_next_member() {
    let a = ScriptedMember::dead("a", retryable);
    let b = ScriptedMember::healthy("b");
    let (ring, events) = recording(ring(&[&a, &b], policy(3, None)));

    assert!(ring.stream(request()).await.is_err());
    assert!(ring.stream(request()).await.is_err());
    ring.stream(request())
        .await
        .expect("the third failure rotates and b answers this call");

    assert_eq!((a.calls(), b.calls()), (3, 1));
    assert_eq!(ring.active().index, 1);
    assert_eq!(
        labels(&events.lock().expect("events")),
        [
            "failed a x1",
            "failed a x2",
            "failed a x3",
            "rotated a→b after 3",
        ]
    );
}

#[tokio::test]
async fn a_rejection_rotates_at_once() {
    let a = ScriptedMember::dead("a", unauthorized);
    let b = ScriptedMember::healthy("b");
    let (ring, events) = recording(ring(&[&a, &b], policy(5, None)));

    ring.stream(request())
        .await
        .expect("b answers the call a rejected");

    assert_eq!((a.calls(), b.calls()), (1, 1));
    assert_eq!(
        labels(&events.lock().expect("events")),
        ["failed a x0", "rotated a→b after 0"]
    );
}

#[tokio::test]
async fn the_requests_own_fault_is_returned_without_moving_the_ring() {
    let a = ScriptedMember::dead("a", too_long);
    let b = ScriptedMember::healthy("b");
    let (ring, events) = recording(ring(&[&a, &b], policy(1, None)));

    let error = ring.stream(request()).await.expect_err("refused");

    assert!(matches!(error, ProviderError::ContextLengthExceeded { .. }));
    assert_eq!((a.calls(), b.calls()), (1, 0));
    assert_eq!(ring.active().index, 0);
    assert!(events.lock().expect("events").is_empty());
}

#[tokio::test]
async fn a_dead_ring_is_tried_once_around_per_call() {
    let a = ScriptedMember::dead("a", retryable);
    let b = ScriptedMember::dead("b", retryable);
    let c = ScriptedMember::dead("c", retryable);
    let ring = ring(&[&a, &b, &c], policy(1, None));

    let error = ring.stream(request()).await.expect_err("everyone fails");

    assert!(matches!(error, ProviderError::Retryable { .. }));
    assert_eq!((a.calls(), b.calls(), c.calls()), (1, 1, 1));
    // The ring wrapped back to a, on probation, for the next call.
    assert_eq!(ring.active().index, 0);
}

#[tokio::test]
async fn a_sticky_ring_stays_where_it_landed() {
    let a = ScriptedMember::new("a", vec![Err(retryable()), Ok(())]);
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], policy(1, None));

    ring.stream(request()).await.expect("b answers");
    for _ in 0..3 {
        ring.stream(request()).await.expect("b keeps answering");
    }

    assert_eq!((a.calls(), b.calls()), (1, 4));
}

#[tokio::test]
async fn a_cooldown_returns_the_ring_to_the_preferred_member() {
    let a = ScriptedMember::new("a", vec![Err(retryable()), Ok(())]);
    let b = ScriptedMember::healthy("b");
    let (ring, events) = recording(ring(&[&a, &b], policy(1, Some(Duration::ZERO))));

    ring.stream(request()).await.expect("b answers");
    assert_eq!(ring.active().index, 1);
    ring.stream(request())
        .await
        .expect("a is probed and answers");

    assert_eq!((a.calls(), b.calls()), (2, 1));
    assert_eq!(ring.active().index, 0);
    assert_eq!(
        labels(&events.lock().expect("events")),
        [
            "failed a x1",
            "rotated a→b after 1",
            "returned a",
            "recovered a",
        ]
    );
}

#[tokio::test]
async fn a_failed_probe_goes_straight_back() {
    let a = ScriptedMember::dead("a", retryable);
    let b = ScriptedMember::healthy("b");
    let (ring, events) = recording(ring(&[&a, &b], policy(2, Some(Duration::ZERO))));

    assert!(ring.stream(request()).await.is_err());
    ring.stream(request()).await.expect("rotates to b");
    ring.stream(request())
        .await
        .expect("a fails its probe once and b answers the same call");

    assert_eq!((a.calls(), b.calls()), (3, 2));
    assert_eq!(
        labels(&events.lock().expect("events")),
        [
            "failed a x1",
            "failed a x2",
            "rotated a→b after 2",
            "returned a",
            "failed a x1",
            "rotated a→b after 1",
        ]
    );
}

#[tokio::test]
async fn fresh_scopes_get_fresh_members_but_share_the_rings_memory() {
    let a = ScriptedMember::dead("a", unauthorized);
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], GatewayRingPolicy::sticky());

    ring.stream(request()).await.expect("rotates to b");
    let scope = ring.fresh_session_scope().expect("members support scopes");
    scope.stream(request()).await.expect("answers");

    assert_eq!((a.scopes(), b.scopes()), (1, 1));
    assert_eq!(
        (a.calls(), b.calls()),
        (1, 2),
        "the new scope started on b: it did not rediscover that a rejects the key"
    );
    assert_eq!(scope.descriptor().id, ring.descriptor().id);
}

#[tokio::test]
async fn a_session_reaches_the_members_not_the_ring_itself() {
    let a = ScriptedMember::dead("a", unauthorized);
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], GatewayRingPolicy::sticky());

    let session = ring.create_session().await.expect("session");
    session.stream(request()).await.expect("b answers");
    assert_eq!((a.calls(), b.calls()), (1, 1));

    // Neither member implements compaction: the ring says so rather than
    // recursing through its own session, and does not count it against
    // anyone.
    let error = session
        .compact(CompactionRequest {
            model: Cow::Borrowed("test-model"),
            instructions: Cow::Borrowed(""),
            input: Cow::Owned(Vec::new()),
            metadata: Cow::Owned(Default::default()),
            provider_request_options: ProviderRequestOptions::default(),
        })
        .await
        .expect_err("unsupported");
    assert!(matches!(error, ProviderError::UnsupportedCapability(_)));
    assert_eq!(ring.active().index, 1);
}

#[tokio::test]
async fn listing_models_rotates_like_any_other_call() {
    let a = ScriptedMember::dead("a", unauthorized);
    let b = ScriptedMember::healthy("b");
    let ring = ring(&[&a, &b], GatewayRingPolicy::sticky());

    let models = ring.list_models().await.expect("b lists");

    assert_eq!(models.len(), 1);
    assert_eq!((a.calls(), b.calls()), (1, 1));
}

#[test]
fn classification_sorts_errors_by_whose_fault_they_are() {
    assert_eq!(classify(&retryable()), FailureKind::Counted);
    assert_eq!(
        classify(&http(StatusCode::BAD_GATEWAY)),
        FailureKind::Counted
    );
    assert_eq!(
        classify(&http(StatusCode::TOO_MANY_REQUESTS)),
        FailureKind::Counted
    );
    assert_eq!(
        classify(&http(StatusCode::REQUEST_TIMEOUT)),
        FailureKind::Counted
    );
    assert_eq!(
        classify(&ProviderError::MalformedStream("x".into())),
        FailureKind::Counted
    );
    assert_eq!(
        classify(&http(StatusCode::BAD_REQUEST)),
        FailureKind::Immediate
    );
    assert_eq!(classify(&unauthorized()), FailureKind::Immediate);
    assert_eq!(
        classify(&http(StatusCode::NOT_FOUND)),
        FailureKind::Immediate
    );
    assert_eq!(classify(&too_long()), FailureKind::NotTheMembersFault);
    assert_eq!(
        classify(&ProviderError::InvalidRequest("x".into())),
        FailureKind::NotTheMembersFault
    );
    assert_eq!(
        classify(&ProviderError::UnsupportedCapability("x".into())),
        FailureKind::NotTheMembersFault
    );
}
