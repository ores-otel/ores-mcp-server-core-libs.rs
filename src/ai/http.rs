use std::time::Duration;

use reqwest::{
    Client, RequestBuilder, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
    redirect::Policy,
    tls::Version,
};
use serde::Serialize;

use super::ProviderError;
use crate::{bounds::Limits, redaction::Secret};

const USER_AGENT: &str = concat!("ores-mcp-server-core-libs/", env!("CARGO_PKG_VERSION"));

pub(crate) struct SecureHttp {
    client: Client,
    limits: Limits,
}

impl SecureHttp {
    pub(crate) fn new(limits: Limits) -> Result<Self, ProviderError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .https_only(true)
            .min_tls_version(Version::TLS_1_2)
            .connect_timeout(limits.connect_timeout())
            .timeout(limits.request_timeout())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| ProviderError::Configuration)?;
        Ok(Self { client, limits })
    }

    pub(crate) fn post(&self, endpoint: Url) -> RequestBuilder {
        self.client
            .post(endpoint)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
    }

    pub(crate) fn encode<T: Serialize>(&self, body: &T) -> Result<Vec<u8>, ProviderError> {
        let bytes = serde_json::to_vec(body).map_err(|_| ProviderError::Serialization)?;
        self.limits.check_request(bytes.len())?;
        Ok(bytes)
    }

    pub(crate) async fn send(
        &self,
        request: RequestBuilder,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, ProviderError> {
        let mut response = request
            .body(body)
            .send()
            .await
            .map_err(|error| classify_transport_error(&error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::HttpStatus(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > self.limits.max_response_bytes() as u64)
        {
            return Err(ProviderError::Bounds(
                crate::bounds::BoundsError::Exceeded { field: "response" },
            ));
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| classify_transport_error(&error))?
        {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(ProviderError::Bounds(
                    crate::bounds::BoundsError::Exceeded { field: "response" },
                ))?;
            self.limits.check_response(next_len)?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

pub(crate) fn fixed_endpoint(value: &str, host: &str) -> Result<Url, ProviderError> {
    let url = Url::parse(value).map_err(|_| ProviderError::Configuration)?;
    if url.scheme() != "https"
        || url.host_str() != Some(host)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::Configuration);
    }
    Ok(url)
}

pub(crate) fn secret_header(
    secret: &Secret,
    prefix: Option<&str>,
) -> Result<HeaderValue, ProviderError> {
    let value = match prefix {
        Some(prefix) => format!("{prefix}{}", secret.expose()),
        None => secret.expose().to_string(),
    };
    let mut header = HeaderValue::from_str(&value).map_err(|_| ProviderError::Configuration)?;
    header.set_sensitive(true);
    Ok(header)
}

fn classify_transport_error(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else if error.is_connect() {
        ProviderError::Connection
    } else {
        ProviderError::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_endpoints_reject_origin_or_url_component_changes() {
        assert!(fixed_endpoint("https://api.openai.com/v1/responses", "api.openai.com").is_ok());
        assert!(
            fixed_endpoint(
                "https://api.openai.com.attacker.example/v1/responses",
                "api.openai.com"
            )
            .is_err()
        );
        assert!(fixed_endpoint("http://api.openai.com/v1/responses", "api.openai.com").is_err());
        assert!(
            fixed_endpoint(
                "https://api.openai.com/v1/responses?key=secret",
                "api.openai.com"
            )
            .is_err()
        );
    }

    #[test]
    fn credential_headers_are_marked_sensitive() {
        let secret = Secret::new("credential").expect("valid secret");
        let header = secret_header(&secret, Some("Bearer ")).expect("valid header");
        assert!(header.is_sensitive());
        assert_eq!(format!("{header:?}"), "Sensitive");
    }
}
