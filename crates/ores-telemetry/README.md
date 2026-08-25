# ores-telemetry

`ores-telemetry` is the lightweight OpenTelemetry ownership boundary for ORES
Rust applications. Application repositories depend on this package instead of
directly depending on `opentelemetry`, `opentelemetry_sdk`,
`opentelemetry-otlp`, or `tracing-opentelemetry`.

The package owns:

- validated OTLP collector configuration;
- process-global trace, metric, and optional log providers;
- JSON diagnostics written only to stderr;
- bounded best-effort provider shutdown;
- a closed local filter grammar that does not consume arbitrary `RUST_LOG`;
- validated metric instruments and static string attributes.

Pin a reviewed immutable commit:

```toml
[dependencies]
ores-telemetry = { git = "https://github.com/ores-otel/ores-mcp-server-core-libs.rs", rev = "<reviewed-commit-sha>" }
```

Initialize once and retain the guard until shutdown:

```rust
use ores_telemetry::{LogLevel, TelemetryConfig, init_with_config};

let config = TelemetryConfig::new("example-service", "example-org")
    .with_service_version(env!("CARGO_PKG_VERSION"))
    .with_default_log_level(LogLevel::Warn)
    .with_target_level("example_service", LogLevel::Info);
let telemetry = init_with_config(&config);
# let _ = telemetry;
```

Metric string attributes accept only `&'static str`, so request-, user-, and
payload-derived strings cannot be attached through this API. Keys that look
sensitive are rejected. Numeric values remain available for observations and
bounded numeric labels.

Applications continue to own their domain metric vocabulary. The shared crate
owns the provider lifecycle and enforces the transport, filtering, metadata,
and cardinality boundaries.
