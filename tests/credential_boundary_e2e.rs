//! Negative contracts at the public MCP provider boundary; no network or secrets.
use std::time::Duration;

use ores_mcp_server_core_libs::{
    ai::{AiRequest, ModelId},
    bounds::{BoundsError, Limits, HARD_MAX_INPUT_BYTES},
    redaction::{is_sensitive_key, Secret},
};

#[test]
fn cloned_secret_never_discloses_unicode_payload_and_byte_ceiling_is_strict() {
    let secret_value = "€".repeat(1_365); // 4,095 UTF-8 bytes, not 1,365 bytes.
    let secret = Secret::new(secret_value.clone()).unwrap();
    for copy in [secret.clone(), secret] {
        let debug = format!("{copy:?}");
        let display = copy.to_string();
        assert!(!debug.contains(&secret_value));
        assert!(!display.contains(&secret_value));
        assert!(debug.contains("[REDACTED]"));
        assert_eq!(display, "[REDACTED]");
    }
    assert!(Secret::new("€".repeat(1_366)).is_err());
    for input in ["", "ab\ncd", "ab\u{0000}cd", "ab\u{007f}cd"] {
        assert!(Secret::new(input).is_err(), "invalid secret was accepted");
    }
}

#[test]
fn configured_model_identifiers_cannot_inject_url_segments_or_headers() {
    assert!(ModelId::new("a".repeat(128)).is_ok());
    assert!(ModelId::new("a".repeat(129)).is_err());
    for input in ["", "../other", "model/path", "bad?key=x", "model\nheader: injected", "模型", "space id"] {
        let error = ModelId::new(input).expect_err("unsafe model identifier accepted");
        assert!(!error.to_string().contains(input), "error exposed user input");
    }
    assert_eq!(ModelId::new("release-2026_11.v1").unwrap().as_str(), "release-2026_11.v1");
}

#[test]
fn request_byte_limits_count_utf8_not_characters_and_reject_token_overflow() {
    let limits = Limits::new(8, 8, 8, 8, Duration::from_secs(1), Duration::from_secs(2)).unwrap();
    let request = AiRequest::new("abcd", "wxyz", limits).unwrap();
    assert!(request.with_max_output_tokens(8_192).is_ok());
    for tokens in [0, 8_193, u32::MAX] {
        assert!(AiRequest::new("a", "b", limits).unwrap().with_max_output_tokens(tokens).is_err());
    }
    assert!(AiRequest::new("a", "€€€", limits).is_err());
    assert!(AiRequest::new("", "   ", limits).is_err());
    assert_eq!(
        Limits::new(HARD_MAX_INPUT_BYTES + 1, 1, 1, 1, Duration::from_secs(1), Duration::from_secs(1))
            .unwrap_err(),
        BoundsError::InvalidSize { field: "max_input_bytes" }
    );
}

#[test]
fn normalized_sensitive_metadata_keys_are_excluded_from_public_telemetry() {
    for key in [
        "http.request.header.authorization",
        "trace-attributes.session-token",
        "Db.CREDENTIAL",
        "api_key",
        "TLS.PRIVATE.KEY",
        "customer.email",
        "jwt_payload",
        "x-signing-key",
    ] {
        assert!(is_sensitive_key(key), "missed sensitive telemetry key: {key}");
    }
    for key in ["service.namespace", "status_code", "trace.span_id", "request.duration_ms"] {
        assert!(!is_sensitive_key(key), "over-redacted benign telemetry key: {key}");
    }
}
