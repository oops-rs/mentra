use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProviderError;

/// Evidence and questions for one Decisions request.
#[derive(Clone, Debug, Serialize)]
pub struct DecisionRequest {
    pub model: String,
    pub input: DecisionInput,
    pub questions: Vec<DecisionQuestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<DecisionCacheOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
}

impl DecisionRequest {
    pub fn new(
        model: impl Into<String>,
        input: impl Into<DecisionInput>,
        questions: Vec<DecisionQuestion>,
    ) -> Self {
        Self {
            model: model.into(),
            input: input.into(),
            questions,
            prompt_cache_key: None,
            prompt_cache_options: None,
            safety_identifier: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProviderError> {
        if !(1..=400).contains(&self.questions.len()) {
            return Err(ProviderError::InvalidRequest(
                "Decisions requires between 1 and 400 questions".to_string(),
            ));
        }
        for question in &self.questions {
            match question {
                DecisionQuestion::Choice { choices, .. } if !(2..=255).contains(&choices.len()) => {
                    return Err(ProviderError::InvalidRequest(
                        "a Decisions choice requires between 2 and 255 choices".to_string(),
                    ));
                }
                DecisionQuestion::Score { levels, .. } if !(2..=10).contains(&levels.len()) => {
                    return Err(ProviderError::InvalidRequest(
                        "a Decisions score requires between 2 and 10 levels".to_string(),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionInput {
    Text(String),
    Items(Vec<DecisionInputItem>),
}

impl From<String> for DecisionInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for DecisionInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

/// A conversation item in the order in which it occurred.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionInputItem {
    Message(DecisionMessage),
    Tool(DecisionToolItem),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum DecisionMessage {
    User {
        content: DecisionUserContent,
    },
    Developer {
        content: DecisionTextContent,
    },
    System {
        content: DecisionTextContent,
    },
    Assistant {
        content: DecisionAssistantContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<DecisionAssistantPhase>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionUserContent {
    Text(String),
    Parts(Vec<DecisionInputPart>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionTextContent {
    Text(String),
    Parts(Vec<DecisionTextPart>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionTextPart {
    InputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_cache_breakpoint: Option<DecisionCacheBreakpoint>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionInputPart {
    InputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_cache_breakpoint: Option<DecisionCacheBreakpoint>,
    },
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<DecisionImageDetail>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_cache_breakpoint: Option<DecisionCacheBreakpoint>,
    },
    InputJson {
        data: Value,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionAssistantContent {
    Text(String),
    Parts(Vec<DecisionAssistantPart>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionAssistantPart {
    OutputText { text: String },
    Refusal { refusal: String },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAssistantPhase {
    Commentary,
    FinalAnswer,
}

/// Function and custom tool calls are evidence; the client does not execute them.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionToolItem {
    FunctionCall {
        arguments: String,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<DecisionItemStatus>,
    },
    FunctionCallOutput {
        call_id: String,
        output: DecisionToolOutput,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<DecisionItemStatus>,
    },
    CustomToolCall {
        input: String,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<DecisionItemStatus>,
    },
    CustomToolCallOutput {
        call_id: String,
        output: DecisionToolOutput,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<DecisionItemStatus>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionItemStatus {
    InProgress,
    Completed,
    Incomplete,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionToolOutput {
    Text(String),
    Parts(Vec<DecisionMediaPart>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionMediaPart {
    InputText {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_cache_breakpoint: Option<DecisionCacheBreakpoint>,
    },
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<DecisionImageDetail>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt_cache_breakpoint: Option<DecisionCacheBreakpoint>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionImageDetail {
    Low,
    High,
    Auto,
    Original,
}

/// Marks an explicit cache boundary in the shared input.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DecisionCacheBreakpoint {
    pub mode: DecisionExplicitCacheMode,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionExplicitCacheMode {
    Explicit,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DecisionCacheOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<DecisionCacheMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionCacheMode {
    Implicit,
    Explicit,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionQuestion {
    Predicate {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        instructions: DecisionInstructions,
    },
    Choice {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        instructions: DecisionInstructions,
        choices: Vec<DecisionChoice>,
    },
    Score {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        instructions: DecisionInstructions,
        levels: Vec<DecisionScoreLevel>,
    },
}

impl DecisionQuestion {
    pub fn predicate(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self::Predicate {
            name: Some(name.into()),
            instructions: DecisionInstructions::Text(instructions.into()),
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => {
                name.as_deref()
            }
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionInstructions {
    Text(String),
    Parts(Vec<DecisionQuestionPart>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionQuestionPart {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<DecisionImageDetail>,
    },
    InputJson {
        data: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionChoiceValue {
    String(String),
    Boolean(bool),
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionChoice {
    pub value: DecisionChoiceValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionScoreLevel {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: Vec<DecisionAnswer>,
    pub usage: DecisionUsage,
}

impl DecisionResponse {
    pub(crate) fn validate_against(&self, request: &DecisionRequest) -> Result<(), ProviderError> {
        if self.answers.len() != request.questions.len() {
            return Err(ProviderError::InvalidResponse(format!(
                "Decisions returned {} answers for {} questions",
                self.answers.len(),
                request.questions.len()
            )));
        }
        for (question, answer) in request.questions.iter().zip(&self.answers) {
            if question.name().is_some() && question.name() != answer.name() {
                return Err(ProviderError::InvalidResponse(
                    "Decisions answer name does not match its question".to_string(),
                ));
            }
            if !matches!(
                (question, answer),
                (_, DecisionAnswer::Refusal { .. })
                    | (
                        DecisionQuestion::Predicate { .. },
                        DecisionAnswer::Predicate { .. }
                    )
                    | (
                        DecisionQuestion::Choice { .. },
                        DecisionAnswer::Choice { .. }
                    )
                    | (DecisionQuestion::Score { .. }, DecisionAnswer::Score { .. })
            ) {
                return Err(ProviderError::InvalidResponse(
                    "Decisions answer type does not match its question".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionAnswer {
    Predicate {
        name: Option<String>,
        probability: f64,
    },
    Choice {
        name: Option<String>,
        choice: DecisionChoiceValue,
        confidence: f64,
        probabilities: Vec<DecisionChoiceProbability>,
    },
    Score {
        name: Option<String>,
        score: f64,
        confidence: f64,
        probabilities: Vec<DecisionLevelProbability>,
    },
    Refusal {
        name: Option<String>,
    },
}

impl DecisionAnswer {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. }
            | Self::Choice { name, .. }
            | Self::Score { name, .. }
            | Self::Refusal { name } => name.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionChoiceProbability {
    pub value: DecisionChoiceValue,
    pub probability: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionLevelProbability {
    pub value: i64,
    pub label: String,
    pub probability: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub input_tokens_details: DecisionInputTokenDetails,
    pub output_tokens: u64,
    pub output_tokens_details: DecisionOutputTokenDetails,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionInputTokenDetails {
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DecisionOutputTokenDetails {
    pub reasoning_tokens: u64,
}
