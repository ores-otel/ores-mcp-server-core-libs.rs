//! Stdio-safe JSON logging, optional OTLP export, and bounded-cardinality metrics.
//!
//! MCP protocol data owns stdout, so this module never configures a stdout
//! writer. When `OTEL_EXPORTER_OTLP_ENDPOINT` is a valid HTTP(S) collector
//! endpoint, traces, metrics, and logs are additionally exported over OTLP.

use std::{
    ffi::OsStr,
    time::{Duration, Instant},
};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
    trace::TracerProvider as _,
};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::SdkMeterProvider,
    trace::{SdkTracer, SdkTracerProvider},
};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IDENTITY_BYTES: usize = 128;
const MAX_ENDPOINT_BYTES: usize = 2_048;

/// Initializes JSON stderr logging plus optional OTLP traces, metrics, and logs.
///
/// Exporter construction errors fail open to stderr and are reported without
/// endpoint or header details. Keep the returned guard alive until orderly
/// process shutdown so final OTLP batches can be flushed.
#[must_use = "keep the telemetry guard alive until service shutdown"]
pub fn init(service_name: &'static str, service_namespace: &'static str) -> TelemetryGuard {
    let service_name = validated_identity(service_name).unwrap_or("mcp-server");
    let service_namespace = validated_identity(service_namespace).unwrap_or("unknown");
    let resource = resource(service_name, service_namespace);
    let endpoint = otlp_endpoint_from_env();
    let endpoint_was_requested = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some();

    let (tracer_provider, tracer) = endpoint
        .as_deref()
        .and_then(|value| build_tracer_provider(value, resource.clone()).ok())
        .map_or((None, None), |(provider, tracer)| {
            global::set_tracer_provider(provider.clone());
            (Some(provider), Some(tracer))
        });
    let meter_provider = endpoint
        .as_deref()
        .and_then(|value| build_meter_provider(value, resource.clone()).ok());
    if let Some(provider) = meter_provider.as_ref() {
        global::set_meter_provider(provider.clone());
    }
    let logger_provider = endpoint
        .as_deref()
        .and_then(|value| build_logger_provider(value, resource).ok());

    if endpoint_was_requested
        && (endpoint.is_none()
            || tracer_provider.is_none()
            || meter_provider.is_none()
            || logger_provider.is_none())
    {
        eprintln!(
            "telemetry: one or more OTLP exporters could not be configured; continuing with JSON stderr"
        );
    }

    let subscriber_installed = install_subscriber(tracer, logger_provider.as_ref());
    if !subscriber_installed {
        eprintln!("telemetry: subscriber already initialized; keeping existing subscriber");
    }

    let status = TelemetryStatus {
        subscriber_installed,
        trace_exporter: tracer_provider.is_some(),
        metric_exporter: meter_provider.is_some(),
        log_exporter: logger_provider.is_some(),
    };
    tracing::info!(
        service.name = service_name,
        service.namespace = service_namespace,
        otel.trace_exporter = status.trace_exporter,
        otel.metric_exporter = status.metric_exporter,
        otel.log_exporter = status.log_exporter,
        log.stream = "stderr",
        "MCP telemetry initialized"
    );

    TelemetryGuard {
        tracer_provider,
        meter_provider,
        logger_provider,
        status,
    }
}

/// Owns OpenTelemetry providers and flushes them when dropped.
#[must_use = "dropping the guard shuts down telemetry exporters"]
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    status: TelemetryStatus,
}

impl TelemetryGuard {
    /// Returns the initialization result without revealing collector settings.
    #[must_use]
    pub const fn status(&self) -> TelemetryStatus {
        self.status
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let tracer_provider = self.tracer_provider.take();
        let meter_provider = self.meter_provider.take();
        let logger_provider = self.logger_provider.take();
        if tracer_provider.is_none() && meter_provider.is_none() && logger_provider.is_none() {
            return;
        }

        let shutdown = std::thread::Builder::new()
            .name("otel-shutdown".to_string())
            .spawn(move || {
                if let Some(provider) = logger_provider {
                    let _ = provider.shutdown_with_timeout(EXPORT_TIMEOUT);
                }
                if let Some(provider) = meter_provider {
                    let _ = provider.shutdown_with_timeout(EXPORT_TIMEOUT);
                }
                if let Some(provider) = tracer_provider {
                    let _ = provider.shutdown_with_timeout(EXPORT_TIMEOUT);
                }
            });
        match shutdown {
            Ok(handle) => {
                if handle.join().is_err() {
                    eprintln!(
                        "telemetry: shutdown flush panicked; final batches may be incomplete"
                    );
                }
            }
            Err(_) => {
                eprintln!(
                    "telemetry: shutdown worker could not start; final batches were not flushed"
                );
            }
        }
    }
}

/// Non-sensitive exporter and subscriber initialization status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TelemetryStatus {
    subscriber_installed: bool,
    trace_exporter: bool,
    metric_exporter: bool,
    log_exporter: bool,
}

impl TelemetryStatus {
    /// Whether this call installed the process tracing subscriber.
    #[must_use]
    pub const fn subscriber_installed(self) -> bool {
        self.subscriber_installed
    }

    /// Whether an OTLP trace exporter was built.
    #[must_use]
    pub const fn trace_exporter(self) -> bool {
        self.trace_exporter
    }

    /// Whether an OTLP metric exporter was built.
    #[must_use]
    pub const fn metric_exporter(self) -> bool {
        self.metric_exporter
    }

    /// Whether an OTLP log exporter was built.
    #[must_use]
    pub const fn log_exporter(self) -> bool {
        self.log_exporter
    }
}

/// Stable classes accepted as metric labels for MCP tools.
///
/// Arbitrary tool names are deliberately not accepted, which prevents an
/// unbounded metric-label surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolClass {
    /// Fleet or repository inventory.
    Inventory,
    /// Details about one already-selected item.
    Details,
    /// Health or configuration status.
    Health,
    /// AI-assisted read-only discovery.
    Discovery,
    /// AI-assisted read-only repair planning.
    RepairPlan,
    /// Any other tool category.
    Other,
}

impl ToolClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Details => "details",
            Self::Health => "health",
            Self::Discovery => "discovery",
            Self::RepairPlan => "repair_plan",
            Self::Other => "other",
        }
    }
}

/// Stable completion outcomes accepted as metric labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    /// Tool completed successfully.
    Ok,
    /// Input or policy rejected the call before work began.
    Rejected,
    /// Tool completed with an application or protocol error.
    Error,
    /// Tool future was abandoned before explicit completion.
    Cancelled,
}

impl ToolOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "rejected",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Low-cardinality OpenTelemetry instruments for MCP tool calls.
#[derive(Clone)]
pub struct ToolMetrics {
    calls: Counter<u64>,
    duration: Histogram<f64>,
}

impl ToolMetrics {
    /// Creates instruments from the process-global meter provider.
    #[must_use]
    pub fn global() -> Self {
        let meter = global::meter("ores-mcp-server");
        let calls = meter
            .u64_counter("mcp.server.tool.calls")
            .with_description("Number of MCP tool calls completed")
            .with_unit("{call}")
            .build();
        let duration = meter
            .f64_histogram("mcp.server.tool.duration")
            .with_description("MCP tool-call duration")
            .with_unit("ms")
            .build();
        Self { calls, duration }
    }

    /// Starts a timer that records `cancelled` if dropped without `finish`.
    #[must_use = "finish the timer with a stable outcome"]
    pub fn start(&self, class: ToolClass) -> ToolTimer {
        ToolTimer {
            metrics: self.clone(),
            class,
            started: Instant::now(),
            finished: false,
        }
    }

    fn record(&self, class: ToolClass, outcome: ToolOutcome, elapsed: Duration) {
        let attributes = [
            KeyValue::new("mcp.tool.class", class.as_str()),
            KeyValue::new("mcp.tool.outcome", outcome.as_str()),
        ];
        self.calls.add(1, &attributes);
        self.duration
            .record(elapsed.as_secs_f64() * 1_000.0, &attributes);
    }
}

/// In-flight low-cardinality tool metric timer.
pub struct ToolTimer {
    metrics: ToolMetrics,
    class: ToolClass,
    started: Instant,
    finished: bool,
}

impl ToolTimer {
    /// Records the duration and explicit completion outcome.
    pub fn finish(mut self, outcome: ToolOutcome) {
        self.metrics
            .record(self.class, outcome, self.started.elapsed());
        self.finished = true;
    }
}

impl Drop for ToolTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.metrics
                .record(self.class, ToolOutcome::Cancelled, self.started.elapsed());
        }
    }
}

fn build_tracer_provider(
    endpoint: &str,
    resource: Resource,
) -> Result<(SdkTracerProvider, SdkTracer), ()> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|_| ())?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("ores-mcp-server");
    Ok((provider, tracer))
}

fn build_meter_provider(endpoint: &str, resource: Resource) -> Result<SdkMeterProvider, ()> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|_| ())?;
    Ok(SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource)
        .build())
}

fn build_logger_provider(endpoint: &str, resource: Resource) -> Result<SdkLoggerProvider, ()> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|_| ())?;
    Ok(SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

fn install_subscriber(
    tracer: Option<SdkTracer>,
    logger_provider: Option<&SdkLoggerProvider>,
) -> bool {
    let filter = safe_log_filter();
    let tracing_layer = tracer.map(|value| tracing_opentelemetry::layer().with_tracer(value));
    let logging_layer = logger_provider.map(OpenTelemetryTracingBridge::new);
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_json_layer())
        .with(tracing_layer)
        .with(logging_layer)
        .try_init()
        .is_ok()
}

fn safe_log_filter() -> EnvFilter {
    let level = configured_log_level(std::env::var_os("ORES_LOG_LEVEL").as_deref());
    // RMCP emits peer metadata and complete notifications at info level. It is
    // disabled here rather than trusting an operator-supplied target directive;
    // otherwise caller-controlled protocol fields can reach stderr and OTLP.
    // Other protocol/network crates are held at warn regardless of the closed
    // application level. `RUST_LOG` is intentionally not consumed.
    EnvFilter::new(format!(
        "{level},rmcp=off,hyper=warn,hyper_util=warn,h2=warn,reqwest=warn,tonic=warn,tower=warn,tower_http=warn,axum=warn,tungstenite=warn,tokio_tungstenite=warn"
    ))
}

fn configured_log_level(value: Option<&OsStr>) -> &'static str {
    match value.and_then(OsStr::to_str) {
        Some("error") => "error",
        Some("info") | None => "info",
        Some(_) => "warn",
    }
}

fn stderr_json_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_ansi(false)
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_writer(std::io::stderr)
}

fn resource(service_name: &str, service_namespace: &str) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", service_name.to_string()),
        KeyValue::new("service.namespace", service_namespace.to_string()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    if let Ok(value) = std::env::var("DEPLOYMENT_ENV") {
        if valid_attribute_value(&value) {
            attributes.push(KeyValue::new("deployment.environment.name", value));
        }
    }
    Resource::builder_empty()
        .with_attributes(attributes)
        .build()
}

fn otlp_endpoint_from_env() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|value| valid_otlp_endpoint(value))
}

fn valid_otlp_endpoint(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_ENDPOINT_BYTES {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn validated_identity(value: &'static str) -> Option<&'static str> {
    if !value.is_empty()
        && value.len() <= MAX_IDENTITY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Some(value)
    } else {
        None
    }
}

fn valid_attribute_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_endpoint_rejects_credentials_and_query_strings() {
        assert!(valid_otlp_endpoint("http://collector:4317"));
        assert!(valid_otlp_endpoint("https://collector.example:4317"));
        assert!(!valid_otlp_endpoint("file:///tmp/telemetry"));
        assert!(!valid_otlp_endpoint("https://user:password@collector:4317"));
        assert!(!valid_otlp_endpoint("https://collector:4317?token=secret"));
    }

    #[test]
    fn service_identity_is_low_cardinality_ascii() {
        assert_eq!(
            validated_identity("elenkos-mcp-server"),
            Some("elenkos-mcp-server")
        );
        assert_eq!(validated_identity("contains a space"), None);
        assert_eq!(validated_identity("contains/slash"), None);
    }

    #[test]
    fn metric_labels_are_closed_enums() {
        assert_eq!(ToolClass::Discovery.as_str(), "discovery");
        assert_eq!(ToolOutcome::Error.as_str(), "error");
    }

    #[test]
    fn log_level_is_a_closed_non_debugging_enum() {
        assert_eq!(configured_log_level(None), "info");
        assert_eq!(configured_log_level(Some(OsStr::new("error"))), "error");
        assert_eq!(configured_log_level(Some(OsStr::new("debug"))), "warn");
        assert_eq!(configured_log_level(Some(OsStr::new("rmcp=trace"))), "warn");
    }
}
