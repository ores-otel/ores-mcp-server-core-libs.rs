//! Central hard limits for all remote AI interactions.

use std::time::Duration;

use thiserror::Error;

/// Absolute input ceiling accepted by this crate.
pub const HARD_MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Absolute serialized-request ceiling accepted by this crate.
pub const HARD_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// Absolute response-body ceiling accepted by this crate.
pub const HARD_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Absolute extracted-text ceiling accepted by this crate.
pub const HARD_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Absolute network timeout accepted by this crate.
pub const HARD_MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// Limits applied before, during, and after an AI-provider request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    max_input_bytes: usize,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_output_bytes: usize,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl Limits {
    /// Creates a validated limit set.
    ///
    /// Every byte limit and timeout must be non-zero and no greater than its
    /// hard ceiling. The request timeout must be at least the connect timeout.
    ///
    /// # Errors
    ///
    /// Returns [`BoundsError`] if a bound is zero, exceeds a hard ceiling, or
    /// if the request timeout is shorter than the connection timeout.
    pub fn new(
        max_input_bytes: usize,
        max_request_bytes: usize,
        max_response_bytes: usize,
        max_output_bytes: usize,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, BoundsError> {
        check_size("max_input_bytes", max_input_bytes, HARD_MAX_INPUT_BYTES)?;
        check_size(
            "max_request_bytes",
            max_request_bytes,
            HARD_MAX_REQUEST_BYTES,
        )?;
        check_size(
            "max_response_bytes",
            max_response_bytes,
            HARD_MAX_RESPONSE_BYTES,
        )?;
        check_size("max_output_bytes", max_output_bytes, HARD_MAX_OUTPUT_BYTES)?;
        check_timeout("connect_timeout", connect_timeout)?;
        check_timeout("request_timeout", request_timeout)?;
        if request_timeout < connect_timeout {
            return Err(BoundsError::InvalidRelationship);
        }

        Ok(Self {
            max_input_bytes,
            max_request_bytes,
            max_response_bytes,
            max_output_bytes,
            connect_timeout,
            request_timeout,
        })
    }

    /// Maximum combined bytes allowed in instructions and user input.
    #[must_use]
    pub const fn max_input_bytes(self) -> usize {
        self.max_input_bytes
    }

    /// Maximum serialized JSON request size.
    #[must_use]
    pub const fn max_request_bytes(self) -> usize {
        self.max_request_bytes
    }

    /// Maximum provider HTTP response body size.
    #[must_use]
    pub const fn max_response_bytes(self) -> usize {
        self.max_response_bytes
    }

    /// Maximum extracted model-output text size.
    #[must_use]
    pub const fn max_output_bytes(self) -> usize {
        self.max_output_bytes
    }

    /// TLS/TCP connection timeout.
    #[must_use]
    pub const fn connect_timeout(self) -> Duration {
        self.connect_timeout
    }

    /// Whole-request timeout, including reading the bounded response body.
    #[must_use]
    pub const fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    pub(crate) fn check_input(self, byte_count: usize) -> Result<(), BoundsError> {
        check_observed("input", byte_count, self.max_input_bytes)
    }

    pub(crate) fn check_request(self, byte_count: usize) -> Result<(), BoundsError> {
        check_observed("request", byte_count, self.max_request_bytes)
    }

    pub(crate) fn check_response(self, byte_count: usize) -> Result<(), BoundsError> {
        check_observed("response", byte_count, self.max_response_bytes)
    }

    pub(crate) fn check_output(self, byte_count: usize) -> Result<(), BoundsError> {
        check_observed("output", byte_count, self.max_output_bytes)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 128 * 1024,
            max_request_bytes: 256 * 1024,
            max_response_bytes: 1024 * 1024,
            max_output_bytes: 128 * 1024,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(45),
        }
    }
}

/// A rejected configuration or observed payload size.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BoundsError {
    /// A configured size is zero or above its hard ceiling.
    #[error("configured byte limit is outside the permitted range: {field}")]
    InvalidSize {
        /// Stable field name; never contains user data.
        field: &'static str,
    },
    /// A configured timeout is zero or above the hard timeout ceiling.
    #[error("configured timeout is outside the permitted range: {field}")]
    InvalidTimeout {
        /// Stable field name; never contains user data.
        field: &'static str,
    },
    /// The request timeout is shorter than the connection timeout.
    #[error("request timeout must be at least the connection timeout")]
    InvalidRelationship,
    /// An observed request component exceeded its configured limit.
    #[error("bounded value exceeds the configured limit: {field}")]
    Exceeded {
        /// Stable value category; never contains payload text.
        field: &'static str,
    },
}

fn check_size(field: &'static str, value: usize, maximum: usize) -> Result<(), BoundsError> {
    if value == 0 || value > maximum {
        Err(BoundsError::InvalidSize { field })
    } else {
        Ok(())
    }
}

fn check_timeout(field: &'static str, value: Duration) -> Result<(), BoundsError> {
    if value.is_zero() || value > HARD_MAX_TIMEOUT {
        Err(BoundsError::InvalidTimeout { field })
    } else {
        Ok(())
    }
}

fn check_observed(field: &'static str, value: usize, maximum: usize) -> Result<(), BoundsError> {
    if value > maximum {
        Err(BoundsError::Exceeded { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inside_hard_ceilings() {
        let value = Limits::default();
        assert!(value.max_input_bytes() <= HARD_MAX_INPUT_BYTES);
        assert!(value.max_request_bytes() <= HARD_MAX_REQUEST_BYTES);
        assert!(value.max_response_bytes() <= HARD_MAX_RESPONSE_BYTES);
        assert!(value.max_output_bytes() <= HARD_MAX_OUTPUT_BYTES);
        assert!(value.request_timeout() <= HARD_MAX_TIMEOUT);
    }

    #[test]
    fn zero_and_oversized_limits_are_rejected() {
        let error = Limits::new(0, 1, 1, 1, Duration::from_secs(1), Duration::from_secs(1))
            .expect_err("zero must fail");
        assert_eq!(
            error,
            BoundsError::InvalidSize {
                field: "max_input_bytes"
            }
        );

        assert!(
            Limits::default()
                .check_input(HARD_MAX_INPUT_BYTES + 1)
                .is_err()
        );
    }

    #[test]
    fn request_timeout_cannot_be_shorter_than_connect_timeout() {
        let error = Limits::new(1, 1, 1, 1, Duration::from_secs(2), Duration::from_secs(1))
            .expect_err("invalid timeout relationship must fail");
        assert_eq!(error, BoundsError::InvalidRelationship);
    }
}
