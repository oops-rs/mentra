//! Which id reaches the wrapped provider, by every door that carries one.

use std::sync::Mutex;

use tokio::sync::mpsc;

use super::*;
use crate::ProviderRequestOptions;

/// Records the model id of every request that reaches it.
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }

    fn record(&self, model: &str) -> Result<ProviderEventStream, ProviderError> {
        self.seen.lock().expect("seen").push(model.to_string());
        let (_tx, rx) = mpsc::unbounded_channel();
        Ok(rx)
    }
}

#[async_trait]
impl Provider for Recorder {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    async fn create_session(&self) -> Result<Box<dyn ProviderSession>, ProviderError> {
        Ok(Box::new(self.clone()))
    }

    fn definition(&self) -> ProviderDefinition {
        ProviderDefinition::new("relay")
    }

    fn fresh_session_scope(&self) -> Result<ProviderSessionScope, ProviderError> {
        Ok(ProviderSessionScope::new(self.clone()))
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.record(&request.model)
    }
}

#[async_trait]
impl ProviderSession for Recorder {
    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.record(&request.model)
    }
}

fn request(model: &'static str) -> Request<'static> {
    Request {
        model: Cow::Borrowed(model),
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

fn wrapped(recorder: &Recorder) -> StripModelPrefix {
    StripModelPrefix::new(Arc::new(recorder.clone()), "openai/")
}

#[tokio::test]
async fn a_namespaced_id_reaches_the_relay_bare() {
    let recorder = Recorder::default();

    wrapped(&recorder)
        .stream(request("openai/gpt-5.6-sol"))
        .await
        .expect("streams");

    assert_eq!(recorder.seen(), ["gpt-5.6-sol"]);
}

#[tokio::test]
async fn another_namespace_and_a_bare_namespace_pass_through_for_the_relay_to_judge() {
    let recorder = Recorder::default();
    let provider = wrapped(&recorder);

    for model in ["xai/grok-4.6", "gpt-5.6-sol", "openai/"] {
        provider.stream(request(model)).await.expect("streams");
    }

    assert_eq!(recorder.seen(), ["xai/grok-4.6", "gpt-5.6-sol", "openai/"]);
}

#[tokio::test]
async fn a_session_and_a_fresh_scope_rewrite_too() {
    // A ring mints a fresh scope per member and streams through sessions, so
    // a rewrite only on the provider's own `stream` would miss every turn.
    let recorder = Recorder::default();
    let provider = wrapped(&recorder);

    provider
        .create_session()
        .await
        .expect("session")
        .stream(request("openai/a"))
        .await
        .expect("streams");
    provider
        .fresh_session_scope()
        .expect("scope")
        .stream(request("openai/b"))
        .await
        .expect("streams");

    assert_eq!(recorder.seen(), ["a", "b"]);
}

#[test]
fn the_definition_is_the_inner_providers() {
    let provider = wrapped(&Recorder::default());

    assert_eq!(provider.definition().descriptor.id.as_str(), "relay");
    assert_eq!(provider.prefix(), "openai/");
}
