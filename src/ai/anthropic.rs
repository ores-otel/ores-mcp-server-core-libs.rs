use async_trait::async_trait;
use reqwest::{
    Url,
    header::{HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};

use super::{
    AiProvider, AiRequest, AiResponse, ProviderError, ProviderKind, ProviderSettings,
    collect_output,
    http::{SecureHttp, fixed_endpoint, secret_header},
};
use crate::bounds::Limits;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const HOST: &str = "api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");
const VERSION_HEADER: HeaderName = HeaderName::from_static("anthropic-version");

/// Fixed-origin connector for the Anthropic Messages API.
pub struct AnthropicProvider {
    settings: ProviderSettings,
    http: SecureHttp,
    endpoint: Url,
    api_key: HeaderValue,
    limits: Limits,
}

impl AnthropicProvider {
    /// Creates a connector using rustls roots and the fixed Anthropic API origin.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Configuration`] if the fixed endpoint, secret
    /// header, or hardened HTTP client cannot be constructed.
    pub fn new(settings: ProviderSettings, limits: Limits) -> Result<Self, ProviderError> {
        let endpoint = fixed_endpoint(ENDPOINT, HOST)?;
        let api_key = secret_header(settings.api_key(), None)?;
        Ok(Self {
            settings,
            http: SecureHttp::new(limits)?,
            endpoint,
            api_key,
            limits,
        })
    }
}

#[async_trait]
impl AiProvider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    #[tracing::instrument(
        name = "ai.provider.request",
        skip_all,
        fields(ai.provider = "anthropic")
    )]
    async fn generate(&self, request: &AiRequest) -> Result<AiResponse, ProviderError> {
        let body = AnthropicRequest {
            model: self.settings.model().as_str(),
            system: request.instructions(),
            messages: [AnthropicMessage {
                role: "user",
                content: request.input(),
            }],
            max_tokens: request.max_output_tokens(),
        };
        let encoded = self.http.encode(&body)?;
        let request = self
            .http
            .post(self.endpoint.clone())
            .header(API_KEY_HEADER, self.api_key.clone())
            .header(VERSION_HEADER, ANTHROPIC_VERSION);
        let bytes = self.http.send(request, encoded).await?;
        let response: AnthropicResponse =
            serde_json::from_slice(&bytes).map_err(|_| ProviderError::MalformedResponse)?;
        let text = extract_text(&response, self.limits)?;
        Ok(AiResponse::new(
            self.kind(),
            self.settings.model().clone(),
            text,
        ))
    }
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    system: &'a str,
    messages: [AnthropicMessage<'a>; 1],
    max_tokens: u32,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    stop_reason: String,
    #[serde(default)]
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
}

fn extract_text(response: &AnthropicResponse, limits: Limits) -> Result<String, ProviderError> {
    if response.kind != "message" || response.role != "assistant" {
        return Err(ProviderError::MalformedResponse);
    }
    match response.stop_reason.as_str() {
        "end_turn" => {}
        "refusal" => return Err(ProviderError::RejectedResponse),
        "max_tokens" | "pause_turn" | "model_context_window_exceeded" => {
            return Err(ProviderError::IncompleteResponse);
        }
        _ => return Err(ProviderError::UnsupportedResponse),
    }

    let mut output_text = Vec::new();
    for content in &response.content {
        match content.kind.as_str() {
            "text" => {
                if content.thinking.is_some() || content.signature.is_some() {
                    return Err(ProviderError::UnsupportedResponse);
                }
                output_text.push(
                    content
                        .text
                        .as_deref()
                        .ok_or(ProviderError::MalformedResponse)?,
                );
            }
            // Claude Fable 5 returns an encrypted signature even when thinking
            // display is omitted. It is non-semantic for this single-turn
            // connector and is never echoed into a subsequent request.
            "thinking"
                if content.text.is_none()
                    && content.thinking.as_deref() == Some("")
                    && content
                        .signature
                        .as_deref()
                        .is_some_and(|value| !value.is_empty()) => {}
            _ => return Err(ProviderError::UnsupportedResponse),
        }
    }
    collect_output(output_text, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_contains_no_tool_definitions() {
        let body = AnthropicRequest {
            model: "configured-model",
            system: "read only",
            messages: [AnthropicMessage {
                role: "user",
                content: "evidence",
            }],
            max_tokens: 512,
        };
        let value = serde_json::to_value(body).expect("serializes");
        assert!(value.get("tools").is_none());
        assert_eq!(value["messages"][0]["role"], "user");
    }

    #[test]
    fn response_accepts_end_turn_text_and_omitted_fable_thinking() {
        let response: AnthropicResponse = serde_json::from_str(
            r#"{"type":"message","role":"assistant","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"","signature":"opaque"},{"type":"text","text":"report"}]}"#,
        )
        .expect("fixture parses");
        let output = extract_text(&response, Limits::default()).expect("text exists");
        assert_eq!(output, "report");
    }

    #[test]
    fn response_rejects_refusal_truncation_and_tool_use() {
        for (fixture, expected) in [
            (
                r#"{"type":"message","role":"assistant","stop_reason":"refusal","content":[{"type":"text","text":"declined"}]}"#,
                ProviderError::RejectedResponse,
            ),
            (
                r#"{"type":"message","role":"assistant","stop_reason":"max_tokens","content":[{"type":"text","text":"partial"}]}"#,
                ProviderError::IncompleteResponse,
            ),
            (
                r#"{"type":"message","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use"}]}"#,
                ProviderError::UnsupportedResponse,
            ),
        ] {
            let response: AnthropicResponse =
                serde_json::from_str(fixture).expect("fixture parses");
            assert_eq!(extract_text(&response, Limits::default()), Err(expected));
        }
    }
}
