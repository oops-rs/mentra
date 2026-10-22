//! A provider that serves a namespaced catalog under bare model ids.
//!
//! Two gateways in front of one model rarely agree on what to call it. A
//! unified gateway serves a namespaced catalog — `openai/gpt-5.6-sol`,
//! `xai/grok-4.6` — while a relay in front of one vendor serves the same
//! model as `gpt-5.6-sol` and answers the namespaced id with a `400`. A host
//! keeps one catalog id, and the member whose ids differ is wrapped in
//! [`StripModelPrefix`], which removes the namespace on the way out.
//!
//! Only the id is touched. Credentials, transport, session scoping and every
//! response shape are the inner provider's, so a
//! [`GatewayRing`](crate::GatewayRing) sees a member that behaves like any
//! other.
//!
//! An id that does not carry the prefix is sent as it is. A relay that hosts
//! one vendor has never heard of another vendor's model, and inventing a bare
//! id for it would turn a clean rejection — which a ring rotates past at once
//! — into a confusing one.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;

use crate::definition::{ProviderDefinition, ProviderDescriptor};
use crate::error::ProviderError;
use crate::model::ModelInfo;
use crate::registry::{Provider, ProviderSession, ProviderSessionScope};
use crate::request::{CompactionRequest, MemorySummarizeRequest, Request};
use crate::response::{CompactionResponse, MemorySummarizeResponse, Response};
use crate::stream::ProviderEventStream;

/// A [`Provider`] that removes `prefix` from every model id it is asked for
/// before delegating to the provider it wraps. See the module doc.
#[derive(Clone)]
pub struct StripModelPrefix {
    inner: Arc<dyn Provider>,
    prefix: Arc<str>,
}

impl StripModelPrefix {
    /// Wraps `inner` so that `openai/gpt-5.6-sol` reaches it as `gpt-5.6-sol`
    /// when `prefix` is `"openai/"`.
    pub fn new(inner: Arc<dyn Provider>, prefix: impl Into<Arc<str>>) -> Self {
        Self {
            inner,
            prefix: prefix.into(),
        }
    }

    /// The prefix this provider removes.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn rewrap(&self, inner: ProviderSessionScope) -> ProviderSessionScope {
        ProviderSessionScope::new(Self {
            inner: Arc::new(inner),
            prefix: Arc::clone(&self.prefix),
        })
    }
}

/// `model` without `prefix`, or `model` unchanged when it does not carry the
/// prefix or the prefix is all it is — a namespace with nothing after it names
/// no model, and the inner provider is the one to say so.
fn strip<'a>(model: Cow<'a, str>, prefix: &str) -> Cow<'a, str> {
    match model.strip_prefix(prefix) {
        Some(bare) if !bare.is_empty() => Cow::Owned(bare.to_string()),
        _ => model,
    }
}

#[async_trait]
impl Provider for StripModelPrefix {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models().await
    }

    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new(StripModelPrefixSession {
            inner: self.inner.create_session().await?,
            prefix: Arc::clone(&self.prefix),
        }))
    }

    fn definition(&self) -> ProviderDefinition {
        self.inner.definition()
    }

    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    /// Re-wrapped, so a fresh scope — which a ring mints for every member —
    /// still reaches the inner provider under its own ids.
    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Ok(self.rewrap(self.inner.fresh_session_scope()?))
    }

    fn fresh_conversation_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Ok(self.rewrap(self.inner.fresh_conversation_scope()?))
    }

    async fn stream(&self, mut request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.stream(request).await
    }

    async fn send(&self, mut request: Request<'_>) -> Result<Response, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.send(request).await
    }

    async fn compact(
        &self,
        mut request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.compact(request).await
    }

    async fn summarize_memories(
        &self,
        mut request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.summarize_memories(request).await
    }
}

/// The session half: the same rewrite, on every request shape that names a
/// model.
struct StripModelPrefixSession {
    inner: Box<dyn ProviderSession>,
    prefix: Arc<str>,
}

#[async_trait]
impl ProviderSession for StripModelPrefixSession {
    async fn stream(&self, mut request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.stream(request).await
    }

    async fn send(&self, mut request: Request<'_>) -> Result<Response, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.send(request).await
    }

    async fn compact(
        &self,
        mut request: CompactionRequest<'_>,
    ) -> Result<CompactionResponse, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.compact(request).await
    }

    async fn summarize_memories(
        &self,
        mut request: MemorySummarizeRequest<'_>,
    ) -> Result<MemorySummarizeResponse, ProviderError> {
        request.model = strip(request.model, &self.prefix);
        self.inner.summarize_memories(request).await
    }
}
