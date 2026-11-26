//! Protocol-specific sampling parameter mixins.
//!
//! Sampling parameters (`temperature`, `top_p`, etc.) are capabilities rather
//! than universal base fields: OpenAI Codex rejects them, DeepSeek reasoner
//! omits them, and Anthropic forbids them when extended thinking is active.
//! Each protocol family keeps its own typed mixin so vendor differences stay
//! explicit and never leak across protocols.

use serde::Serialize;
use serde_json::{Map, Value};

use meridian_provider_sdk::chat::ChatParams;

/// OpenAI Chat Completions & Responses API sampling parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OpenAISamplingParamsMixin {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

impl OpenAISamplingParamsMixin {
    pub fn from_chat_params(params: &ChatParams) -> Self {
        Self {
            temperature: params.temperature,
            top_p: params.top_p,
        }
    }

    pub fn apply_to_json(&self, body: &mut Map<String, Value>) {
        if let Some(temperature) = self.temperature {
            body.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = self.top_p {
            body.insert("top_p".into(), serde_json::json!(top_p));
        }
    }
}

/// Anthropic Messages API sampling parameters.
///
/// Anthropic forbids sending `temperature`/`top_p` alongside active extended
/// thinking; callers construct this mixin with `thinking_active` so the rule
/// stays in one place.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AnthropicSamplingParamsMixin {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

impl AnthropicSamplingParamsMixin {
    pub fn from_chat_params(params: &ChatParams, thinking_active: bool) -> Self {
        Self {
            temperature: if thinking_active { None } else { params.temperature },
            top_p: params.top_p,
        }
    }

    pub fn apply_to_json(&self, body: &mut Map<String, Value>) {
        if let Some(temperature) = self.temperature {
            body.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = self.top_p {
            body.insert("top_p".into(), serde_json::json!(top_p));
        }
    }
}

/// Google Gemini (`generateContent` camelCase & `interactions` snake_case)
/// sampling parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GoogleSamplingParamsMixin {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

impl GoogleSamplingParamsMixin {
    pub fn from_chat_params(params: &ChatParams) -> Self {
        Self {
            max_output_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
        }
    }

    /// Apply to `generationConfig` (camelCase, `gemini_generate_content`).
    pub fn apply_camel_case(&self, generation: &mut Map<String, Value>) {
        if let Some(max) = self.max_output_tokens {
            generation.insert("maxOutputTokens".into(), serde_json::json!(max));
        }
        if let Some(temperature) = self.temperature {
            generation.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = self.top_p {
            generation.insert("topP".into(), serde_json::json!(top_p));
        }
    }

    /// Apply to `generation_config` (snake_case, `gemini_interactions`).
    pub fn apply_snake_case(&self, generation: &mut Map<String, Value>) {
        if let Some(max) = self.max_output_tokens {
            generation.insert("max_output_tokens".into(), serde_json::json!(max));
        }
        if let Some(temperature) = self.temperature {
            generation.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(top_p) = self.top_p {
            generation.insert("top_p".into(), serde_json::json!(top_p));
        }
    }
}
