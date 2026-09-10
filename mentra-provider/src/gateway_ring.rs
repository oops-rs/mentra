//! A ring of interchangeable gateways behind one provider.
//!
//! A host that reaches its model through more than one gateway — a remote
//! relay and a local one, say — wants to prefer one and fall back to the next
//! when it goes bad, without every layer above learning about the second
//! address. [`GatewayRing`] is that: a [`Provider`] whose members are
//! providers, which sends every call to one *current* member and rotates to
//! the next when the current one keeps failing.
//!
//! The runtime above it sees one provider with one descriptor. Its own retry
//! loop keeps working exactly as before — each retry lands on the ring, the
//! ring counts it, and once the streak reaches the policy's threshold the
//! ring finishes *that same call* on the next member. Nothing above the ring
//! has to know a rotation happened, though a host that wants to can watch
//! through [`GatewayRing::with_observer`].
//!
//! # Members must be interchangeable
//!
//! A rotation replays the conversation so far to the new member. That is
//! only sound if every member fronts the **same upstream provider serving
//! the same model**: a transcript carrying one vendor's reasoning items
//! replayed to another vendor's endpoint is refused (measured: OpenAI-shaped
//! `encrypted_content` sent to an xAI-backed relay answers `422`). Two
//! relays in front of the same model are a ring; two different vendors are
//! not, and nothing here can tell the difference for you.
//!
//! Each member keeps its own session state — a Responses provider's response
//! chain, its websocket, its endpoint knowledge — so no continuation id ever
//! crosses from one gateway to another. A ring built from two clones of the
//! *same* provider instance would share that state and defeat this; build
//! each member separately.
//!
//! # Members are whole providers
//!
//! A member is a provider, not a URL, because two gateways rarely accept the
//! same key. Build each one the way you would build it alone — its own base
//! URL, its own credential — and hand the finished providers to the ring.
//!
//! # The policy
//!
//! [`GatewayRingPolicy`] says when to leave a member and whether to come
//! back; the module doc on [`health`] spells out the rules. The defaults
//! rotate after five consecutive failures and drift back to the preferred
//! member once it has rested a minute.

mod health;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::definition::ProviderDefinition;
use crate::error::ProviderError;
use crate::model::ModelInfo;
use crate::registry::{Provider, ProviderSession, ProviderSessionScope};
use crate::request::{CompactionRequest, MemorySummarizeRequest, Request};
use crate::response::{CompactionResponse, MemorySummarizeResponse};
use crate::stream::ProviderEventStream;

pub use self::health::{FailureKind, GatewayRingPolicy};
use self::health::{RingHealth, Selection};

/// A ring with no members has nobody to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a gateway ring needs at least one member")]
pub struct EmptyGatewayRing;

/// Which member an event is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayMemberRef {
    /// The member's position in the ring, `0` being the preferred one.
    pub index: usize,
    /// The member's display name, or its base URL, or its id — whichever it
    /// had first. For logs.
    pub label: String,
}

/// Something the ring did that a host may want in its logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayRingEvent {
    /// The current member failed a call.
    MemberFailed {
        member: GatewayMemberRef,
        kind: FailureKind,
        /// The streak after this failure; `0` when the failure was not
        /// counted.
        consecutive_failures: u32,
        error: String,
    },
    /// The ring left one member for the next. The call that triggered it
    /// continues on `to`.
    Rotated {
        from: GatewayMemberRef,
        to: GatewayMemberRef,
        /// The streak that caused it; `0` for an immediate rotation.
        after_failures: u32,
    },
    /// The ring came back to a more preferred member whose cooldown elapsed.
    /// The member is on probation until it answers.
    Returned { member: GatewayMemberRef },
    /// A member on probation answered; the ring is settled on it.
    Recovered { member: GatewayMemberRef },
}

type Observer = Arc<dyn Fn(GatewayRingEvent) + Send + Sync>;

struct Member {
    provider: Arc<dyn Provider>,
    label: String,
}

impl Member {
    fn new(provider: Arc<dyn Provider>) -> Self {
        let definition = provider.definition();
        let label = definition
            .descriptor
            .display_name
            .clone()
            .or_else(|| definition.base_url.clone())
            .unwrap_or_else(|| definition.descriptor.id.to_string());
        Self { provider, label }
    }

    fn reference(&self, index: usize) -> GatewayMemberRef {
        GatewayMemberRef {
            index,
            label: self.label.clone(),
        }
    }
}

/// An ordered ring of gateways, spoken to one at a time. See the module doc.
///
/// Cloning shares everything: the members, their session state, and the
/// ring's memory of who is answering. Use
/// [`Provider::fresh_session_scope`] for members with fresh session state;
/// the ring's memory is shared across scopes regardless, because a gateway
/// that is down is down for every conversation.
#[derive(Clone)]
pub struct GatewayRing {
    members: Arc<[Member]>,
    definition: ProviderDefinition,
    policy: GatewayRingPolicy,
    health: Arc<Mutex<RingHealth>>,
    observer: Option<Observer>,
}

impl fmt::Debug for GatewayRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayRing")
            .field(
                "members",
                &self
                    .members
                    .iter()
                    .map(|member| member.label.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("policy", &self.policy)
            .field("current", &self.health().current())
            .finish_non_exhaustive()
    }
}

impl GatewayRing {
    /// Builds a ring over `members`, in preference order.
    ///
    /// The ring takes its identity — id, wire, capabilities — from the first
    /// member, and only its display name says there is a ring at all.
    pub fn new(
        members: impl IntoIterator<Item = Arc<dyn Provider>>,
        policy: GatewayRingPolicy,
    ) -> Result<Self, EmptyGatewayRing> {
        let members: Arc<[Member]> = members.into_iter().map(Member::new).collect();
        let first = members.first().ok_or(EmptyGatewayRing)?;
        let definition = ring_definition(first.provider.definition(), &members);
        Ok(Self {
            health: Arc::new(Mutex::new(RingHealth::new(members.len()))),
            members,
            definition,
            policy,
            observer: None,
        })
    }

    /// Reports what the ring does — every failure it counts, every rotation,
    /// every return — to `observer`. The ring has no logger of its own; this
    /// is how a rotation reaches a host's logs.
    #[must_use]
    pub fn with_observer(
        self,
        observer: impl Fn(GatewayRingEvent) + Send + Sync + 'static,
    ) -> Self {
        Self {
            observer: Some(Arc::new(observer)),
            ..self
        }
    }

    /// The policy this ring rotates by.
    pub fn policy(&self) -> GatewayRingPolicy {
        self.policy
    }

    /// The members, in preference order.
    pub fn members(&self) -> Vec<GatewayMemberRef> {
        self.members
            .iter()
            .enumerate()
            .map(|(index, member)| member.reference(index))
            .collect()
    }

    /// The member the next call would go to, before any cooldown is applied.
    pub fn active(&self) -> GatewayMemberRef {
        let index = self.health().current();
        self.members[index].reference(index)
    }

    fn health(&self) -> RingHealth {
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Applies a pure transition to the shared health under the lock.
    fn transition<T>(&self, transition: impl FnOnce(&RingHealth) -> (RingHealth, T)) -> T {
        let mut guard = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (next, out) = transition(&guard);
        *guard = next;
        out
    }

    fn emit(&self, event: GatewayRingEvent) {
        if let Some(observer) = &self.observer {
            observer(event);
        }
    }

    /// Runs `op` against the current member, rotating and re-running it on
    /// the next member when the failure calls for it, at most once around
    /// the ring per call.
    ///
    /// `arg` is passed through rather than captured so the closure borrows
    /// nothing of its own: the future it returns then lives as long as the
    /// member and the argument, which is all the compiler needs to see.
    async fn with_rotation<A, T>(
        &self,
        arg: A,
        op: impl for<'a> Fn(&'a dyn Provider, &'a A) -> BoxFuture<'a, Result<T, ProviderError>>,
    ) -> Result<T, ProviderError>
    where
        A: Sync,
    {
        let mut selection = self.transition(|health| health.select(self.policy, Instant::now()));
        let mut attempts = 0usize;
        loop {
            attempts += 1;
            let index = selection.index;
            let member = &self.members[index];
            if selection.returned {
                self.emit(GatewayRingEvent::Returned {
                    member: member.reference(index),
                });
            }
            let probation = self.health().on_probation();
            match op(member.provider.as_ref(), &arg).await {
                Ok(value) => {
                    self.transition(|health| (health.succeeded(index), ()));
                    if probation {
                        self.emit(GatewayRingEvent::Recovered {
                            member: member.reference(index),
                        });
                    }
                    return Ok(value);
                }
                Err(error) => {
                    let kind = classify(&error);
                    if kind == FailureKind::NotTheMembersFault {
                        return Err(error);
                    }
                    let now = Instant::now();
                    let rotation =
                        self.transition(|health| health.failed(index, kind, self.policy, now));
                    self.emit(GatewayRingEvent::MemberFailed {
                        member: member.reference(index),
                        kind,
                        consecutive_failures: rotation
                            .map_or_else(|| self.streak_of(index), |r| r.after_failures),
                        error: error.to_string(),
                    });
                    let Some(rotation) = rotation else {
                        return Err(error);
                    };
                    self.emit(GatewayRingEvent::Rotated {
                        from: member.reference(rotation.from),
                        to: self.members[rotation.to].reference(rotation.to),
                        after_failures: rotation.after_failures,
                    });
                    if attempts >= self.members.len() {
                        return Err(error);
                    }
                    selection = Selection {
                        index: rotation.to,
                        returned: false,
                    };
                }
            }
        }
    }

    fn streak_of(&self, index: usize) -> u32 {
        self.health().streak_of(index)
    }
}

/// Sorts a failed call into what the ring should do about it. See
/// [`FailureKind`].
pub fn classify(error: &ProviderError) -> FailureKind {
    use reqwest::StatusCode;
    match error {
        ProviderError::Transport(_)
        | ProviderError::Decode(_)
        | ProviderError::Deserialize(_)
        | ProviderError::Retryable { .. }
        | ProviderError::InvalidResponse(_)
        | ProviderError::MalformedStream(_) => FailureKind::Counted,
        ProviderError::Http { status, .. } => {
            if *status == StatusCode::TOO_MANY_REQUESTS || *status == StatusCode::REQUEST_TIMEOUT {
                FailureKind::Counted
            } else if status.is_client_error() {
                FailureKind::Immediate
            } else {
                FailureKind::Counted
            }
        }
        ProviderError::ContextLengthExceeded { .. }
        | ProviderError::Serialize(_)
        | ProviderError::InvalidRequest(_)
        | ProviderError::UnsupportedCapability(_) => FailureKind::NotTheMembersFault,
    }
}

fn ring_definition(first: ProviderDefinition, members: &[Member]) -> ProviderDefinition {
    let labels = members
        .iter()
        .map(|member| member.label.as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    let mut definition = first;
    definition.descriptor.display_name = Some(format!("gateway ring: {labels}"));
    definition
}

#[async_trait]
impl Provider for GatewayRing {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.with_rotation((), |member, ()| member.list_models())
            .await
    }

    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new(GatewayRingSession(self.clone())))
    }

    fn definition(&self) -> ProviderDefinition {
        self.definition.clone()
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        let members = self
            .members
            .iter()
            .map(|member| {
                member.provider.fresh_session_scope().map(|scope| Member {
                    provider: Arc::new(scope),
                    label: member.label.clone(),
                })
            })
            .collect::<Result<Arc<[Member]>, _>>()?;
        Ok(ProviderSessionScope::new(Self {
            members,
            definition: self.definition.clone(),
            policy: self.policy,
            health: Arc::clone(&self.health),
            observer: self.observer.clone(),
        }))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.with_rotation(request, |member, request| member.stream(request.clone()))
            .await
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        self.with_rotation(request, |member, request| member.compact(request.clone()))
            .await
    }

    async fn summarize_memories(
        &self,
        request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        self.with_rotation(request, |member, request| {
            member.summarize_memories(request.clone())
        })
        .await
    }
}

/// A session over the ring. Every call goes back through the ring so the
/// rotation logic runs once, on the provider, rather than being copied here.
struct GatewayRingSession(GatewayRing);

#[async_trait]
impl ProviderSession for GatewayRingSession {
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        Provider::stream(&self.0, request).await
    }

    async fn compact(
        &self,
        request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        Provider::compact(&self.0, request).await
    }

    async fn summarize_memories(
        &self,
        request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        Provider::summarize_memories(&self.0, request).await
    }
}
