use std::collections::BTreeMap;

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

const HOST: &str = "generativelanguage.googleapis.com";
const API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");

/// Fixed-origin connector for the Gemini `generateContent` API.
pub struct GeminiProvider {
    settings: ProviderSettings,
    http: SecureHttp,
    endpoint: Url,
    api_key: HeaderValue,
    limits: Limits,
}

impl GeminiProvider {
    /// Creates a connector using rustls roots and the fixed Google API origin.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Configuration`] if the model-specific fixed
    /// endpoint, secret header, or hardened HTTP client cannot be constructed.
    pub fn new(settings: ProviderSettings, limits: Limits) -> Result<Self, ProviderError> {
        let endpoint = format!(
            "https://{HOST}/v1beta/models/{}:generateContent",
            settings.model().as_str()
        );
        let endpoint = fixed_endpoint(&endpoint, HOST)?;
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
impl AiProvider for GeminiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }

    #[tracing::instrument(
        name = "ai.provider.request",
        skip_all,
        fields(ai.provider = "gemini")
    )]
    async fn generate(&self, request: &AiRequest) -> Result<AiResponse, ProviderError> {
        let body = GeminiRequest {
            system_instruction: GeminiContent {
                parts: [GeminiPart {
                    text: request.instructions(),
                }],
            },
            contents: [GeminiContent {
                parts: [GeminiPart {
                    text: request.input(),
                }],
            }],
            generation_config: GeminiGenerationConfig {
                max_output_tokens: request.max_output_tokens(),
            },
        };
        let encoded = self.http.encode(&body)?;
        let request = self
            .http
            .post(self.endpoint.clone())
            .header(API_KEY_HEADER, self.api_key.clone());
        let bytes = self.http.send(request, encoded).await?;
        let response: GeminiResponse =
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
#[serde(rename_all = "camelCase")]
struct GeminiRequest<'a> {
    system_instruction: GeminiContent<'a>,
    contents: [GeminiContent<'a>; 1],
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: [GeminiPart<'a>; 1],
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    max_output_tokens: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    prompt_feedback: Option<GeminiPromptFeedback>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: GeminiResponseContent,
    finish_reason: Option<String>,
    #[serde(default)]
    safety_ratings: Vec<GeminiSafetyRating>,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    role: String,
    #[serde(default)]
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPromptFeedback {
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiSafetyRating {
    blocked: Option<bool>,
}

fn extract_text(response: &GeminiResponse, limits: Limits) -> Result<String, ProviderError> {
    if response
        .prompt_feedback
        .as_ref()
        .and_then(|feedback| feedback.block_reason.as_deref())
        .is_some()
    {
        return Err(ProviderError::RejectedResponse);
    }
    let [candidate] = response.candidates.as_slice() else {
        return if response.candidates.is_empty() {
            Err(ProviderError::RejectedResponse)
        } else {
            Err(ProviderError::UnsupportedResponse)
        };
    };
    match candidate.finish_reason.as_deref() {
        Some("STOP") => {}
        Some("MAX_TOKENS") | None => return Err(ProviderError::IncompleteResponse),
        Some(
            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY",
        ) => return Err(ProviderError::RejectedResponse),
        _ => return Err(ProviderError::UnsupportedResponse),
    }
    if candidate
        .safety_ratings
        .iter()
        .any(|rating| rating.blocked == Some(true))
    {
        return Err(ProviderError::RejectedResponse);
    }
    if candidate.content.role != "model" {
        return Err(ProviderError::MalformedResponse);
    }
    let output_text = candidate
        .content
        .parts
        .iter()
        .map(|part| {
            if part.thought || !part.other.is_empty() {
                return Err(ProviderError::UnsupportedResponse);
            }
            part.text
                .as_deref()
                .ok_or(ProviderError::UnsupportedResponse)
        })
        .collect::<Result<Vec<_>, _>>()?;
    collect_output(output_text, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ModelId;
    use crate::redaction::Secret;

    #[test]
    fn model_changes_only_the_fixed_origin_path() {
        let provider = GeminiProvider::new(
            ProviderSettings::new(
                Secret::new("test-key").expect("valid secret"),
                ModelId::new("configured-model").expect("valid model"),
            ),
            Limits::default(),
        )
        .expect("provider builds");
        assert_eq!(provider.endpoint.scheme(), "https");
        assert_eq!(provider.endpoint.host_str(), Some(HOST));
        assert_eq!(
            provider.endpoint.path(),
            "/v1beta/models/configured-model:generateContent"
        );
        assert!(provider.endpoint.query().is_none());
    }

    #[test]
    fn request_contains_no_tools_or_api_key() {
        let body = GeminiRequest {
            system_instruction: GeminiContent {
                parts: [GeminiPart { text: "read only" }],
            },
            contents: [GeminiContent {
                parts: [GeminiPart { text: "evidence" }],
            }],
            generation_config: GeminiGenerationConfig {
                max_output_tokens: 512,
            },
        };
        let value = serde_json::to_value(body).expect("serializes");
        assert!(value.get("tools").is_none());
        assert!(!value.to_string().contains("api_key"));
        assert_eq!(value["generationConfig"]["maxOutputTokens"], 512);
    }

    #[test]
    fn response_accepts_one_naturally_completed_text_candidate() {
        let response: GeminiResponse = serde_json::from_str(
            r#"{"candidates":[{"finishReason":"STOP","safetyRatings":[{"blocked":false}],"content":{"role":"model","parts":[{"text":"one"},{"text":"two"}]}}]}"#,
        )
        .expect("fixture parses");
        let output = extract_text(&response, Limits::default()).expect("text exists");
        assert_eq!(output, "one\ntwo");
    }

    #[test]
    fn response_rejects_blocked_truncated_and_non_text_results() {
        for (fixture, expected) in [
            (
                r#"{"promptFeedback":{"blockReason":"SAFETY"},"candidates":[]}"#,
                ProviderError::RejectedResponse,
            ),
            (
                r#"{"candidates":[{"finishReason":"MAX_TOKENS","content":{"role":"model","parts":[{"text":"partial"}]}}]}"#,
                ProviderError::IncompleteResponse,
            ),
            (
                r#"{"candidates":[{"finishReason":"STOP","content":{"role":"model","parts":[{"functionCall":{"name":"mutate"}}]}}]}"#,
                ProviderError::UnsupportedResponse,
            ),
        ] {
            let response: GeminiResponse = serde_json::from_str(fixture).expect("fixture parses");
            assert_eq!(extract_text(&response, Limits::default()), Err(expected));
        }
    }
}
