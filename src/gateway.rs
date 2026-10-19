//! AI Gateway: provider-agnostic OpenAI-compatible chat completions client.
//!
//! Works with any OpenAI-compatible endpoint: OpenAI, Anthropic-compatible
//! gateways, Azure OpenAI, vLLM, Ollama (`/v1` mode), LM Studio, llama.cpp
//! server, etc. The base URL and API key are configuration; no provider
//! is hardcoded.
//!
//! Configuration (checked in order):
//!   SOMEDA_API_KEY  / OPENAI_API_KEY  — the API key
//!   SOMEDA_BASE_URL / OPENAI_BASE_URL — the API base URL
//!   SOMEDA_MODEL    / OPENAI_MODEL    — the model name
//!
//! Defaults: base_url = https://api.openai.com/v1, model = gpt-4o-mini

use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Gateway {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
}

#[derive(Debug)]
pub enum GatewayError {
    NotConfigured(String),
    Http(String),
    Parse(String),
    Empty,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::NotConfigured(msg) => write!(f, "gateway not configured: {}", msg),
            GatewayError::Http(msg) => write!(f, "HTTP error: {}", msg),
            GatewayError::Parse(msg) => write!(f, "response parse error: {}", msg),
            GatewayError::Empty => write!(f, "empty response from model"),
        }
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatRespMessage,
}

#[derive(Deserialize)]
struct ChatRespMessage {
    content: Option<String>,
}

impl Gateway {
    /// Create from environment variables. Returns Err with what's missing.
    pub fn from_env() -> Result<Gateway, GatewayError> {
        let api_key = env::var("SOMEDA_API_KEY")
            .or_else(|_| env::var("OPENAI_API_KEY"))
            .map_err(|_| GatewayError::NotConfigured(
                "set SOMEDA_API_KEY or OPENAI_API_KEY".into()
            ))?;

        let base_url = env::var("SOMEDA_BASE_URL")
            .or_else(|_| env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let model = env::var("SOMEDA_MODEL")
            .or_else(|_| env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());

        Ok(Gateway {
            base_url,
            api_key,
            model,
            timeout: Duration::from_secs(120),
        })
    }

    /// Send a chat completion request. Returns the assistant's message content.
    pub fn chat(&self, system: &str, user: &str) -> Result<String, GatewayError> {
        self.chat_with_temperature(system, user, None)
    }

    /// Chat with a specific temperature (lower = more deterministic for JSON output).
    pub fn chat_with_temperature(
        &self,
        system: &str,
        user: &str,
        temperature: Option<f64>,
    ) -> Result<String, GatewayError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage { role: "system", content: system },
                ChatMessage { role: "user", content: user },
            ],
            temperature,
        };

        let response = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .timeout(self.timeout)
            .send_json(serde_json::to_value(&body).map_err(|e| GatewayError::Parse(e.to_string()))?)
            .map_err(|e| GatewayError::Http(e.to_string()))?;

        let parsed: ChatResponse = response
            .into_json()
            .map_err(|e| GatewayError::Parse(e.to_string()))?;

        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .filter(|c| !c.is_empty())
            .ok_or(GatewayError::Empty)
    }

    /// Chat and extract a JSON object from the response (handles markdown fences).
    pub fn chat_json(&self, system: &str, user: &str) -> Result<serde_json::Value, GatewayError> {
        // Low temperature for structured output
        let raw = self.chat_with_temperature(system, user, Some(0.1))?;
        Self::extract_json(&raw)
    }

    /// Extract JSON from a response that may be wrapped in markdown fences.
    pub fn extract_json(raw: &str) -> Result<serde_json::Value, GatewayError> {
        let trimmed = raw.trim();
        // Try direct parse first
        if let Ok(v) = serde_json::from_str(trimmed) {
            return Ok(v);
        }
        // Try stripping markdown fences: ```json ... ``` or ``` ... ```
        if let Some(start) = trimmed.find("```") {
            let after_fence = &trimmed[start + 3..];
            let after_fence = after_fence
                .strip_prefix("json")
                .or_else(|| after_fence.strip_prefix("JSON"))
                .unwrap_or(after_fence);
            if let Some(end) = after_fence.rfind("```") {
                let inner = &after_fence[..end];
                if let Ok(v) = serde_json::from_str(inner.trim()) {
                    return Ok(v);
                }
            }
        }
        // Try to find the first { or [ as a last resort
        if let Some(start) = trimmed.find('{') {
            if let Some(end) = trimmed.rfind('}') {
                if let Ok(v) = serde_json::from_str(&trimmed[start..=end]) {
                    return Ok(v);
                }
            }
        }
        if let Some(start) = trimmed.find('[') {
            if let Some(end) = trimmed.rfind(']') {
                if let Ok(v) = serde_json::from_str(&trimmed[start..=end]) {
                    return Ok(v);
                }
            }
        }
        Err(GatewayError::Parse(format!(
            "no JSON found in response: {}...",
            &raw[..raw.len().min(100)]
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_direct() {
        let v = Gateway::extract_json(r#"{"a": 1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn extract_json_markdown_fenced() {
        let v = Gateway::extract_json("Here's the plan:\n```json\n{\"components\": []}\n```\ndone").unwrap();
        assert!(v["components"].is_array());
    }

    #[test]
    fn extract_json_bare_fence() {
        let v = Gateway::extract_json("```\n{\"x\": \"y\"}\n```").unwrap();
        assert_eq!(v["x"], "y");
    }

    #[test]
    fn extract_json_embedded() {
        let v = Gateway::extract_json(r#"The result is {"nested": {"deep": true}} as shown."#).unwrap();
        assert_eq!(v["nested"]["deep"], true);
    }

    #[test]
    fn extract_json_array() {
        let v = Gateway::extract_json(r#"```json
[1, 2, 3]
```"#).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
    }

    #[test]
    fn extract_json_fails_on_garbage() {
        assert!(Gateway::extract_json("no json here at all").is_err());
    }

    #[test]
    fn gateway_from_env_unconfigured() {
        // In CI there's no API key, so this should fail with a helpful message
        env::remove_var("SOMEDA_API_KEY");
        env::remove_var("OPENAI_API_KEY");
        let result = Gateway::from_env();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("SOMEDA_API_KEY") || msg.contains("OPENAI_API_KEY"));
    }
}