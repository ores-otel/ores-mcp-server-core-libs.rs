//! Shared, security-oriented building blocks for Rust MCP servers.
//!
//! The crate deliberately keeps AI-assisted discovery and repair advisory-only:
//! it can send bounded text to a configured provider and return bounded text,
//! but it has no filesystem, process, GitHub, or MCP tool-execution capability.

#![forbid(unsafe_code)]

pub mod ai;
pub mod bounds;
pub mod observability;
pub mod redaction;
pub mod state_machine;
pub mod transport;

/// The exact RMCP version used by the shared runtime.
///
/// Downstream servers can import protocol types through this re-export to
/// avoid accidentally selecting a different RMCP release.
pub use rmcp;
