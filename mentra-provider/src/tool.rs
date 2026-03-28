use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Declares whether a tool is loaded eagerly or deferred for provider-native tool search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolLoadingPolicy {
    #[default]
    Immediate,
    Deferred,
}

/// Provider-visible tool kinds supported by Mentra-backed providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolKind {
    #[default]
    Function,
    HostedWebSearch,
    ImageGeneration,
}

/// Provider-facing description of a tool and its schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub kind: ProviderToolKind,
    #[serde(default)]
    pub loading_policy: ToolLoadingPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

impl ToolSpec {
    pub fn builder(name: impl Into<String>) -> ToolSpecBuilder {
        ToolSpecBuilder {
            name: name.into(),
            description: None,
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: None,
            kind: ProviderToolKind::Function,
            loading_policy: ToolLoadingPolicy::Immediate,
            options: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpecBuilder {
    name: String,
    description: Option<String>,
    input_schema: Value,
    output_schema: Option<Value>,
    kind: ProviderToolKind,
    loading_policy: ToolLoadingPolicy,
    options: Option<Value>,
}

impl ToolSpecBuilder {
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn input_schema(mut self, input_schema: Value) -> Self {
        self.input_schema = input_schema;
        self
    }

    pub fn output_schema(mut self, output_schema: Value) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    pub fn kind(mut self, kind: ProviderToolKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn options(mut self, options: Value) -> Self {
        self.options = Some(options);
        self
    }

    pub fn loading_policy(mut self, loading_policy: ToolLoadingPolicy) -> Self {
        self.loading_policy = loading_policy;
        self
    }

    pub fn defer_loading(self, defer_loading: bool) -> Self {
        self.loading_policy(if defer_loading {
            ToolLoadingPolicy::Deferred
        } else {
            ToolLoadingPolicy::Immediate
        })
    }

    pub fn build(self) -> ToolSpec {
        ToolSpec {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema,
            output_schema: self.output_schema,
            kind: self.kind,
            loading_policy: self.loading_policy,
            options: self.options,
        }
    }
}
