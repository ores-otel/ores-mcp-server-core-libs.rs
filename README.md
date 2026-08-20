# ores-mcp-server-core-libs

Secure shared components for Rust MCP servers operated by `ores-otel` and
related organizations. The crate standardizes:

- a bounded, thread-safe asynchronous lifecycle state machine;
- bounded MCP stdio, stateless Streamable HTTP, TCP NDJSON, and WebSocket
  runners;
- configuration-driven OpenAI, Anthropic, and Gemini advisory connectors;
- JSON diagnostics on stderr, OpenTelemetry export, and low-cardinality MCP
  metrics;
- redaction, fixed resource limits, and conformance/security test helpers.

The crate targets Rust 2024 with MSRV 1.88 and pins `rmcp` 3.1.4,
OpenTelemetry 0.32, and `tracing-opentelemetry` 0.33. Downstream repositories
must pin a reviewed immutable commit rather than a floating branch.

## Security boundary

AI-assisted discovery and repair are advisory-only. Connectors accept a closed,
bounded runtime-evidence schema made from enums and numeric counters; there is
no arbitrary text, identity, domain payload, credential, filesystem, process,
shell, GitHub mutation, MCP tool invocation, or plan-application surface.
Provider output is bounded untrusted text and always requires separate human or
workflow authorization before any action is taken.

Provider clients use rustls and fixed HTTPS origins, disable redirects and
ambient proxies, keep keys in sensitive headers, and bound connection time,
whole-request time, serialized input, streamed response bytes, extracted text,
and output tokens. Prompts, completions, keys, headers, bodies, and model IDs
are never logged or attached to telemetry.

Every byte-stream transport enforces its frame limit before JSON
deserialization. Network transports bind to loopback by default. A remote bind
requires explicit configuration and still needs authentication, TLS, and
network policy at a trusted deployment boundary; this crate does not invent an
application-specific authorization policy.

## Dependency

```toml
[dependencies]
ores-mcp-server-core-libs = { git = "https://github.com/ores-otel/ores-mcp-server-core-libs.rs", rev = "<reviewed-commit-sha>" }
```

## Structured advisory connectors

```rust
use ores_mcp_server_core_libs::{
    ai::{
        AdvisoryEvidence, EvidenceTransport, ProviderKind, ProviderRegistry,
        RuntimeComponent, RuntimeEvidence, RuntimeOutcome, RuntimeSymptom,
    },
    bounds::Limits,
};

let providers = ProviderRegistry::from_env(Limits::default());
let evidence = AdvisoryEvidence::new(vec![RuntimeEvidence::new(
    RuntimeComponent::AiConnector,
    EvidenceTransport::Internal,
    RuntimeSymptom::ConnectionFailure,
    RuntimeOutcome::Unavailable,
)])?;

// These calls return advisory text only; they cannot apply changes.
let discovery = providers
    .discover(ProviderKind::OpenAi, &evidence)
    .await?;
let plan = providers
    .repair_plan(ProviderKind::Anthropic, &evidence)
    .await?;
# let _ = (discovery, plan);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`statuses()` is deliberately a local configuration check. `Ready` means only
that an API key and path-safe model ID were present and a hardened client was
built. It does not authenticate credentials or prove model availability,
quota, or vendor health.

| Provider API | API key | Required exact model ID | Fixed endpoint |
| --- | --- | --- | --- |
| OpenAI Responses | `OPENAI_API_KEY` | `OPENAI_MODEL` | `https://api.openai.com/v1/responses` |
| Anthropic Messages | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` | `https://api.anthropic.com/v1/messages` |
| Gemini generateContent | `GEMINI_API_KEY` | `GEMINI_MODEL` | `https://generativelanguage.googleapis.com/...:generateContent` |

No provider model has a compiled default. `claude-fable-5` is the documented
Claude Fable identifier. No official identifiers matching “ChatGPT 4.6 Sol”
or “Gemini Pro 3.6+” were verified when this repository was created, so the
library never silently substitutes another family, tier, preview, or Flash
model. Operators must configure an exact model available to their account.

## Lifecycle and formal checks

All handler instances and the selected transport share clones of one
`LifecycleController`. Transitions are serialized in a short synchronous
critical section that is never held across `.await`; successful transitions
receive monotonic revisions, publish ordered watch snapshots, and enter a
bounded audit ring containing only closed state/event fields.

The production transition function is exhaustively evaluated over every
state/event pair. Unit and async tests cover invalid edges, audit bounds,
cancellation, clone sharing, and concurrent publication. Loom explores
relevant transition interleavings. A companion TLA+ model is checked by a
checksum-pinned TLC 1.7.4 job in CI. See
[`docs/state-machine.md`](docs/state-machine.md) and [`formal/`](formal/).

## Transports

- stdio is standard MCP, the expected default, and reserves stdout solely for
  newline-delimited JSON-RPC frames;
- Streamable HTTP is standard stateless MCP at `/mcp`, with modern protocol
  metadata, Host/Origin validation, legacy sessions disabled, and a streaming
  body limit;
- TCP uses bounded newline-delimited JSON and is explicitly non-standard;
- WebSocket uses bounded text messages at `/mcp/ws`, requires exact Host and
  Origin matches, and is explicitly non-standard.

Each runner drives the same formal lifecycle through startup, readiness,
degradation, drain, and terminal shutdown. See the public types in
`transport` for loopback defaults and environment-backed configuration.

## Observability

Call `observability::init(service_name, service_namespace)` before starting a
transport and retain the returned guard through shutdown. JSON application
logs always target stderr. When `OTEL_EXPORTER_OTLP_ENDPOINT` is configured,
the library exports best-effort OTLP traces, metrics, and logs. The example
Collector routes traces to Tempo, metrics to Prometheus, and logs to Loki.

Only closed tool class/outcome fields are used as metric attributes. MCP
arguments/results, arbitrary errors, request/session/user identifiers, model
input/output, and secrets are excluded. See [`OBSERVABILITY.md`](OBSERVABILITY.md)
and [`deploy/otel-collector.yaml`](deploy/otel-collector.yaml).

## Development

```text
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

CI also verifies the TLA+ model with the tag-pinned, SHA-256-verified TLC jar.
The workflow has read-only repository permissions and pins checkout by commit.
