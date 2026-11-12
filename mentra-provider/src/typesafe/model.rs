use std::collections::BTreeMap;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProviderError;

/// A System One state and the named questions evaluated against it.
#[derive(Clone, Debug, Serialize)]
pub struct SystemOneRequest {
    pub model: String,
    /// A string, object, or array. Strings inside it may reference image assets.
    pub state: Value,
    pub questions: BTreeMap<String, SystemOneQuestion>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, SystemOneAsset>,
}

impl SystemOneRequest {
    pub fn new(
        model: impl Into<String>,
        state: impl Into<Value>,
        questions: BTreeMap<String, SystemOneQuestion>,
    ) -> Self {
        Self {
            model: model.into(),
            state: state.into(),
            questions,
            assets: BTreeMap::new(),
        }
    }

    pub(crate) fn validate(&self, images_enabled: bool) -> Result<(), ProviderError> {
        if !matches!(
            self.state,
            Value::String(_) | Value::Array(_) | Value::Object(_)
        ) {
            return Err(ProviderError::InvalidRequest(
                "System One state must be a string, object, or array".to_string(),
            ));
        }
        if self.questions.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "System One requires at least one question".to_string(),
            ));
        }
        for question in self.questions.values() {
            match question {
                SystemOneQuestion::Choice { criteria, .. }
                    if !(1..=255).contains(&criteria.len()) =>
                {
                    return Err(ProviderError::InvalidRequest(
                        "a System One choice requires between 1 and 255 choices".to_string(),
                    ));
                }
                SystemOneQuestion::Score { criteria, .. }
                    if !(1..=10).contains(&criteria.len()) =>
                {
                    return Err(ProviderError::InvalidRequest(
                        "a System One score requires between 1 and 10 levels".to_string(),
                    ));
                }
                _ => {}
            }
        }
        if !images_enabled
            && (!self.assets.is_empty()
                || self
                    .questions
                    .values()
                    .any(|question| matches!(question, SystemOneQuestion::Box { .. })))
        {
            return Err(ProviderError::InvalidRequest(
                "System One images and box questions require explicit image opt-in".to_string(),
            ));
        }
        Ok(())
    }
}

/// Questions retain TypeSafe's structured instructions and criteria.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemOneQuestion {
    Noul {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<SystemOneNoulCriteria>,
    },
    Choice {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        criteria: BTreeMap<String, Value>,
    },
    Score {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
        criteria: Vec<Value>,
    },
    Box {
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<Value>,
    },
}

impl SystemOneQuestion {
    pub fn noul(instructions: impl Into<String>) -> Self {
        Self::Noul {
            instructions: Some(Value::String(instructions.into())),
            criteria: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemOneNoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub yes: Option<Value>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub no: Option<Value>,
}

/// An inline image asset. Multipart uploads are not supported by this client.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemOneAsset {
    Image {
        format: SystemOneImageFormat,
        data_base64: String,
    },
}

impl SystemOneAsset {
    pub fn image(format: SystemOneImageFormat, bytes: impl AsRef<[u8]>) -> Self {
        Self::Image {
            format,
            data_base64: STANDARD.encode(bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemOneImageFormat {
    Png,
    Jpg,
    Jpeg,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SystemOneResponse {
    pub model: String,
    pub answers: BTreeMap<String, SystemOneAnswer>,
    pub usage: SystemOneUsage,
}

impl SystemOneResponse {
    pub(crate) fn validate_against(&self, request: &SystemOneRequest) -> Result<(), ProviderError> {
        if self.answers.len() != request.questions.len() {
            return Err(ProviderError::InvalidResponse(format!(
                "System One returned {} answers for {} questions",
                self.answers.len(),
                request.questions.len()
            )));
        }
        for (name, question) in &request.questions {
            let Some(answer) = self.answers.get(name) else {
                return Err(ProviderError::InvalidResponse(format!(
                    "System One did not return an answer for question {name:?}"
                )));
            };
            if !matches!(
                (question, answer),
                (SystemOneQuestion::Noul { .. }, SystemOneAnswer::Noul { .. })
                    | (
                        SystemOneQuestion::Choice { .. },
                        SystemOneAnswer::Choice { .. }
                    )
                    | (
                        SystemOneQuestion::Score { .. },
                        SystemOneAnswer::Score { .. }
                    )
                    | (SystemOneQuestion::Box { .. }, SystemOneAnswer::Box { .. })
            ) {
                return Err(ProviderError::InvalidResponse(format!(
                    "System One answer type does not match question {name:?}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemOneAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        legend: BTreeMap<String, Value>,
        probabilities: BTreeMap<String, f64>,
    },
    Box {
        #[serde(rename = "box")]
        bounds: SystemOneBox,
        confidence: Option<f64>,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub struct SystemOneBox {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SystemOneModel {
    pub name: String,
    pub description: String,
    pub release_date: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SystemOneModels {
    pub models: Vec<SystemOneModel>,
}
