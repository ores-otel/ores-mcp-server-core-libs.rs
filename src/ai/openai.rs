use async_trait::async_trait;
use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize};

use super::{
    AiProvider, AiRequest, AiResponse, ProviderError, ProviderKind, ProviderSettings,
    collect_output,
    http::{SecureHttp, fixed_endpoint, secret_header},
};
use crate::bounds::Limits;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const HOST: &str = "api.openai.com";

/// Fixed-origin connector for the `OpenAI` Responses API.
pub struct OpenAiProvider {
    settings: ProviderSettings,
    http: SecureHttp,
    endpoint: Url,
    authorization: HeaderValue,
    limits: Limits,
}

impl OpenAiProvider {
    /// Creates a connector using rustls roots and the fixed `OpenAI` API origin.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Configuration`] if the fixed endpoint, secret
    /// header, or hardened HTTP client cannot be constructed.
    pub fn new(settings: ProviderSettings, limits: Limits) -> Result<Self, ProviderError> {
        let endpoint = fixed_endpoint(ENDPOINT, HOST)?;
        let authorization = secret_header(settings.api_key(), Some("Bearer "))?;
        Ok(Self {
            settings,
            http: SecureHttp::new(limits)?,
            endpoint,
            authorization,
            limits,
        })
    }
}

#[async_trait]
impl AiProvider for OpenAiProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    #[tracing::instrument(
        name = "ai.provider.request",
        skip_all,
        fields(ai.provider = "openai")
    )]
    async fn generate(&self, request: &AiRequest) -> Result<AiResponse, ProviderError> {
        let body = OpenAiRequest {
            model: self.settings.model().as_str(),
            instructions: request.instructions(),
            input: request.input(),
            max_output_tokens: request.max_output_tokens(),
            store: false,
        };
        let encoded = self.http.encode(&body)?;
        let request = self
            .http
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, self.authorization.clone());
        let bytes = self.http.send(request, encoded).await?;
        let response: OpenAiResponse =
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
struct OpenAiRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a str,
    max_output_tokens: u32,
    store: bool,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    status: String,
    error: Option<serde_json::Value>,
    incomplete_details: Option<serde_json::Value>,
    #[serde(default)]
    output: Vec<OpenAiOutput>,
}

#[derive(Deserialize)]
struct OpenAiOutput {
    #[serde(rename = "type")]
    kind: String,
    role: Option<String>,
    status: Option<String>,
    #[serde(default)]
    content: Vec<OpenAiContent>,
}

#[derive(Deserialize)]
struct OpenAiContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

fn extract_text(response: &OpenAiResponse, limits: Limits) -> Result<String, ProviderError> {
    if response.status != "completed"
        || response.error.is_some()
        || response.incomplete_details.is_some()
    {
        return Err(ProviderError::IncompleteResponse);
    }

    let (output_text, message_count) = response.output.iter().try_fold(
        (Vec::new(), 0_usize),
        |(output_text, message_count), item| match item.kind.as_str() {
            // Reasoning items are metadata-only for this single-turn,
            // text-only connector. Any content attached to one is unexpected.
            "reasoning" if item.content.is_empty() => Ok((output_text, message_count)),
            "message" => {
                if item.role.as_deref() != Some("assistant")
                    || item.status.as_deref() != Some("completed")
                {
                    return Err(ProviderError::IncompleteResponse);
                }
                let texts = item
                    .content
                    .iter()
                    .map(|content| match content.kind.as_str() {
                        "output_text" => content
                            .text
                            .as_deref()
                            .ok_or(ProviderError::MalformedResponse),
                        "refusal" => Err(ProviderError::RejectedResponse),
                        _ => Err(ProviderError::UnsupportedResponse),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((
                    output_text.into_iter().chain(texts).collect(),
                    message_count + 1,
                ))
            }
            _ => Err(ProviderError::UnsupportedResponse),
        },
    )?;
    if message_count == 0 {
        return Err(ProviderError::EmptyResponse);
    }
    collect_output(output_text, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ModelId;
    use crate::redaction::Secret;

    fn provider() -> OpenAiProvider {
        OpenAiProvider::new(
            ProviderSettings::new(
                Secret::new("test-key").expect("valid secret"),
                ModelId::new("configured-model").expect("valid model"),
            ),
            Limits::default(),
        )
        .expect("provider builds")
    }

    #[test]
    fn request_disables_server_side_storage_and_has_no_tools() {
        let request =
            AiRequest::new("system", "evidence", Limits::default()).expect("valid request");
        let provider = provider();
        let body = OpenAiRequest {
            model: provider.settings.model().as_str(),
            instructions: request.instructions(),
            input: request.input(),
            max_output_tokens: request.max_output_tokens(),
            store: false,
        };
        let value = serde_json::to_value(body).expect("serializes");
        assert_eq!(value["store"], false);
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn response_accepts_only_complete_assistant_output_text() {
        let response: OpenAiResponse = serde_json::from_str(
            r#"{"status":"completed","error":null,"incomplete_details":null,"output":[{"type":"reasoning"},{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"plan"}]}]}"#,
        )
        .expect("fixture parses");
        let output = extract_text(&response, Limits::default()).expect("text exists");
        assert_eq!(output, "plan");
    }

    #[test]
    fn response_rejects_incomplete_refusal_and_tool_output() {
        let incomplete: OpenAiResponse = serde_json::from_str(
            r#"{"status":"incomplete","output":[{"type":"message","role":"assistant","status":"incomplete","content":[{"type":"output_text","text":"partial"}]}]}"#,
        )
        .expect("fixture parses");
        assert_eq!(
            extract_text(&incomplete, Limits::default()),
            Err(ProviderError::IncompleteResponse)
        );

        let refusal: OpenAiResponse = serde_json::from_str(
            r#"{"status":"completed","output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"refusal","text":"declined"}]}]}"#,
        )
        .expect("fixture parses");
        assert_eq!(
            extract_text(&refusal, Limits::default()),
            Err(ProviderError::RejectedResponse)
        );

        let tool: OpenAiResponse = serde_json::from_str(
            r#"{"status":"completed","output":[{"type":"function_call","name":"mutate"}]}"#,
        )
        .expect("fixture parses");
        assert_eq!(
            extract_text(&tool, Limits::default()),
            Err(ProviderError::UnsupportedResponse)
        );
    }
}
