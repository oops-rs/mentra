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

pub use auth::AuthScheme;
pub use auth::CredentialSource;
pub use auth::ProviderCredentials;
pub use auth::StaticCredentialSource;
pub use definition::BuiltinProvider;
pub use definition::ProviderCapabilities;
pub use definition::ProviderDefinition;
pub use definition::ProviderDescriptor;
pub use definition::ProviderId;
pub use definition::RetryPolicy;
pub use definition::WireApi;
pub use error::ProviderError;
pub use model::ContentBlock;
pub use model::HostedToolSearchCall;
pub use model::HostedWebSearchCall;
pub use model::ImageGenerationCall;
pub use model::ImageGenerationResult;
pub use model::ImageSource;
pub use model::Message;
pub use model::ModelInfo;
pub use model::ModelSelector;
pub use model::Role;
pub use model::TokenUsage;
pub use model::ToolChoice;
pub use model::ToolResultContent;
pub use model::WebSearchAction;
pub use registry::ModelCatalog;
pub use registry::Provider;
pub use registry::ProviderRegistry;
pub use registry::ProviderSession;
pub use registry::ProviderSessionFactory;
pub use registry::RegisteredProvider;
pub use request::AnthropicRequestOptions;
pub use request::CompactionInputItem;
pub use request::CompactionRequest;
pub use request::GeminiRequestOptions;
pub use request::MemorySummarizeRequest;
pub use request::ProviderRequestOptions;
pub use request::RawMemory;
pub use request::RawMemoryMetadata;
pub use request::ReasoningEffort;
pub use request::ReasoningOptions;
pub use request::ReasoningSummary;
pub use request::Request;
pub use request::ResponsesRequestCompression;
pub use request::ResponsesRequestOptions;
pub use request::ResponsesTextControls;
pub use request::ResponsesTextFormat;
pub use request::ResponsesTextFormatType;
pub use request::ResponsesVerbosity;
pub use request::SessionRequestOptions;
pub use request::ToolSearchMode;
pub use response::CompactionResponse;
pub use response::MemorySummarizeOutput;
pub use response::MemorySummarizeResponse;
pub use response::Response;
pub use response::collect_response_from_stream;
pub use response::provider_event_stream_from_response;
pub use stream::ContentBlockDelta;
pub use stream::ContentBlockStart;
pub use stream::ProviderEvent;
pub use stream::ProviderEventStream;
pub use stream::ResponseHeaders;
pub use tool::ProviderToolKind;
pub use tool::ToolLoadingPolicy;
pub use tool::ToolSpec;
pub use tool::ToolSpecBuilder;

pub type OpenAIRequestOptions = ResponsesRequestOptions;

pub mod provider {
    pub use crate::Provider;

    pub mod model {
        pub use crate::AnthropicRequestOptions;
        pub use crate::ContentBlock;
        pub use crate::ContentBlockDelta;
        pub use crate::ContentBlockStart;
        pub use crate::HostedToolSearchCall;
        pub use crate::HostedWebSearchCall;
        pub use crate::ImageGenerationCall;
        pub use crate::ImageGenerationResult;
        pub use crate::ImageSource;
        pub use crate::Message;
        pub use crate::ModelInfo;
        pub use crate::OpenAIRequestOptions;
        pub use crate::ProviderError;
        pub use crate::ProviderEvent;
        pub use crate::ProviderEventStream;
        pub use crate::ProviderId;
        pub use crate::ProviderRequestOptions;
        pub use crate::ReasoningEffort;
        pub use crate::ReasoningOptions;
        pub use crate::ReasoningSummary;
        pub use crate::Request;
        pub use crate::Response;
        pub use crate::ResponsesTextControls;
        pub use crate::ResponsesTextFormat;
        pub use crate::ResponsesTextFormatType;
        pub use crate::ResponsesVerbosity;
        pub use crate::Role;
        pub use crate::SessionRequestOptions;
        pub use crate::TokenUsage;
        pub use crate::ToolChoice;
        pub use crate::ToolResultContent;
        pub use crate::ToolSearchMode;
        pub use crate::WebSearchAction;
        pub use crate::collect_response_from_stream;
        pub use crate::provider_event_stream_from_response;
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
