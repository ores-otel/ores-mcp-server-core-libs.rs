//! Secret-bearing types and conservative sensitive-key detection.

use std::fmt;

use thiserror::Error;

const MAX_SECRET_BYTES: usize = 4096;
const REDACTED: &str = "[REDACTED]";

/// An in-memory secret whose `Debug` and `Display` representations are redacted.
///
/// The clear value is intentionally available only inside this crate, so a
/// downstream server cannot accidentally interpolate it into logs.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(Box<str>);

impl Secret {
    /// Wraps a non-empty, control-character-free secret.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the value is empty, contains control
    /// characters, or exceeds the hard in-memory secret bound.
    pub fn new(value: impl Into<String>) -> Result<Self, SecretError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_SECRET_BYTES || value.chars().any(char::is_control)
        {
            return Err(SecretError);
        }
        Ok(Self(value.into_boxed_str()))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Secret").field(&REDACTED).finish()
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// Returned when a secret does not meet the non-disclosing input policy.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("secret does not meet the required input policy")]
pub struct SecretError;

/// Returns whether a metadata key appears to describe sensitive data.
///
/// This is intended as a deny-list backstop for telemetry attributes. Payloads
/// and arbitrary headers should not be added to telemetry in the first place.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    [
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "credential",
        "email",
        "jwt",
        "passphrase",
        "passwd",
        "password",
        "private_key",
        "pwd",
        "secret",
        "session",
        "signing_key",
        "token",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_reveal_the_secret() {
        let secret = Secret::new("top-secret-value").expect("valid secret");
        let debug = format!("{secret:?}");
        let display = secret.to_string();
        assert!(!debug.contains("top-secret-value"));
        assert!(!display.contains("top-secret-value"));
        assert!(debug.contains(REDACTED));
        assert_eq!(display, REDACTED);
    }

    #[test]
    fn secret_rejects_empty_and_control_characters() {
        assert_eq!(Secret::new(""), Err(SecretError));
        assert_eq!(Secret::new("line\nbreak"), Err(SecretError));
    }

    #[test]
    fn sensitive_key_detection_normalizes_common_spellings() {
        assert!(is_sensitive_key("http.request.header.authorization"));
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("session-token"));
        assert!(!is_sensitive_key("service.namespace"));
    }
}
