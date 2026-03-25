pub mod anthropic;
mod auth;
mod definition;
mod error;
pub mod gemini;
mod model;
mod registry;
mod request;
mod response;
pub mod responses;
mod stream;
mod tool;

pub use auth::{AuthScheme, CredentialSource, ProviderCredentials, StaticCredentialSource};
pub use definition::{
    BuiltinProvider, ProviderCapabilities, ProviderDefinition, ProviderDescriptor, ProviderId,
    RetryPolicy, WireApi,
};
pub use error::ProviderError;
pub use model::{
    ContentBlock, ImageSource, Message, ModelInfo, ModelSelector, Role, TokenUsage, ToolChoice,
};
pub use registry::{
    ModelCatalog, Provider, ProviderRegistry, ProviderSession, ProviderSessionFactory,
    RegisteredProvider,
};
pub use request::{
    AnthropicRequestOptions, CompactionInputItem, CompactionRequest, GeminiRequestOptions,
    ProviderRequestOptions, ReasoningEffort, ReasoningOptions, Request, ResponsesRequestOptions,
    ToolSearchMode,
};
pub use response::{
    CompactionResponse, Response, collect_response_from_stream, provider_event_stream_from_response,
};
pub use stream::{ContentBlockDelta, ContentBlockStart, ProviderEvent, ProviderEventStream};
pub use tool::{
    ToolCapability, ToolDurability, ToolExecutionMode, ToolLoadingPolicy, ToolSideEffectLevel,
    ToolSpec, ToolSpecBuilder,
};

pub type OpenAIRequestOptions = ResponsesRequestOptions;

pub mod provider {
    pub use crate::Provider;

    pub mod model {
        pub use crate::{
            AnthropicRequestOptions, ContentBlock, ContentBlockDelta, ContentBlockStart,
            ImageSource, Message, ModelInfo, OpenAIRequestOptions, ProviderError, ProviderEvent,
            ProviderEventStream, ProviderId, ProviderRequestOptions, ReasoningEffort,
            ReasoningOptions, Request, Response, Role, TokenUsage, ToolChoice, ToolSearchMode,
            collect_response_from_stream, provider_event_stream_from_response,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_definition_defaults_to_responses_wire_api_and_websockets_disabled() {
        let definition = ProviderDefinition::new(BuiltinProvider::OpenAI);

        assert_eq!(
            definition.descriptor.id,
            ProviderId::from(BuiltinProvider::OpenAI)
        );
        assert_eq!(definition.wire_api, WireApi::Responses);
        assert!(!definition.capabilities.supports_websockets);
    }
}
