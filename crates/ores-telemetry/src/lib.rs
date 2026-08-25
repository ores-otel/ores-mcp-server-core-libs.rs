//! Hardened OpenTelemetry ownership for ORES Rust applications.
//!
//! Applications configure telemetry through this crate instead of importing
//! OpenTelemetry SDK and exporter crates directly. The boundary owns provider
//! installation, OTLP endpoint validation, stderr-safe JSON logging, bounded
//! shutdown, and low-cardinality metric primitives.

#![forbid(unsafe_code)]

use std::{
    error::Error,
    ffi::OsStr,
    fmt::{Display, Formatter},
    time::Duration,
};

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter as OtelCounter, Histogram as OtelHistogram, Meter as OtelMeter},
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
const MAX_ATTRIBUTE_VALUE_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 256;
const MAX_UNIT_BYTES: usize = 32;

/// Closed local log levels accepted by the telemetry boundary.
///
/// The boundary intentionally excludes `debug` and `trace` so an environment
/// variable cannot enable high-volume or payload-rich dependency logging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    /// Disable a target.
    Off,
    /// Emit error events only.
    Error,
    /// Emit warning and error events.
    Warn,
    /// Emit informational, warning, and error events.
    Info,
}

impl LogLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
        }
    }
}

/// Process-wide telemetry configuration.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    service_name: &'static str,
    service_namespace: &'static str,
    service_version: &'static str,
    instrumentation_name: &'static str,
    default_log_level: LogLevel,
    target_levels: Vec<(&'static str, LogLevel)>,
    export_logs: bool,
}

impl TelemetryConfig {
    /// Creates a configuration with safe defaults.
    ///
    /// Local JSON goes to stderr, all three OTLP signals are enabled when a
    /// valid collector endpoint is present, and the default log level is info.
    #[must_use]
    pub fn new(service_name: &'static str, service_namespace: &'static str) -> Self {
        Self {
            service_name,
            service_namespace,
            service_version: "unknown",
            instrumentation_name: service_name,
            default_log_level: LogLevel::Info,
            target_levels: Vec::new(),
            export_logs: true,
        }
    }

    /// Sets the application version attached to the OpenTelemetry resource.
    #[must_use]
    pub const fn with_service_version(mut self, service_version: &'static str) -> Self {
        self.service_version = service_version;
        self
    }

    /// Sets the instrumentation scope used for tracers and meters.
    #[must_use]
    pub const fn with_instrumentation_name(mut self, instrumentation_name: &'static str) -> Self {
        self.instrumentation_name = instrumentation_name;
        self
    }

    /// Sets the default local filtering level.
    #[must_use]
    pub const fn with_default_log_level(mut self, level: LogLevel) -> Self {
        self.default_log_level = level;
        self
    }

    /// Adds a validated static target override.
    ///
    /// Invalid targets are rejected without changing the configuration. This
    /// prevents caller-controlled filter directives from being interpreted.
    #[must_use]
    pub fn with_target_level(mut self, target: &'static str, level: LogLevel) -> Self {
        if validated_identity(target).is_some() {
            self.target_levels.push((target, level));
        }
        self
    }

    /// Controls whether tracing events are additionally exported as OTLP logs.
    #[must_use]
    pub const fn with_otlp_logs(mut self, enabled: bool) -> Self {
        self.export_logs = enabled;
        self
    }
}

/// Initializes JSON stderr logging plus optional OTLP traces, metrics, and logs.
///
/// Exporter construction errors fail open to stderr and are reported without
/// endpoint or header details. Keep the returned guard alive through orderly
/// process shutdown so final OTLP batches can be flushed.
#[must_use = "keep the telemetry guard alive until service shutdown"]
pub fn init(service_name: &'static str, service_namespace: &'static str) -> TelemetryGuard {
    init_with_config(&TelemetryConfig::new(service_name, service_namespace))
}

/// Initializes telemetry using an explicit process-wide configuration.
#[must_use = "keep the telemetry guard alive until service shutdown"]
pub fn init_with_config(config: &TelemetryConfig) -> TelemetryGuard {
    let service_name = validated_identity(config.service_name).unwrap_or("ores-service");
    let service_namespace = validated_identity(config.service_namespace).unwrap_or("unknown");
    let service_version = validated_identity(config.service_version).unwrap_or("unknown");
    let instrumentation_name =
        validated_identity(config.instrumentation_name).unwrap_or(service_name);
    let resource = resource(service_name, service_namespace, service_version);
    let endpoint = otlp_endpoint_from_env();
    let endpoint_was_requested = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some();

    let (tracer_provider, tracer) = endpoint
        .as_deref()
        .and_then(|value| build_tracer_provider(value, resource.clone(), instrumentation_name).ok())
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
    let logger_provider = config
        .export_logs
        .then_some(endpoint.as_deref())
        .flatten()
        .and_then(|value| build_logger_provider(value, resource).ok());

    if endpoint_was_requested
        && (endpoint.is_none()
            || tracer_provider.is_none()
            || meter_provider.is_none()
            || (config.export_logs && logger_provider.is_none()))
    {
        eprintln!(
            "telemetry: one or more OTLP exporters could not be configured; continuing with JSON stderr"
        );
    }

    let subscriber_installed = install_subscriber(config, tracer, logger_provider.as_ref());
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
        service.version = service_version,
        otel.trace_exporter = status.trace_exporter,
        otel.metric_exporter = status.metric_exporter,
        otel.log_exporter = status.log_exporter,
        log.stream = "stderr",
        "ORES telemetry initialized"
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
    /// Returns initialization status without exposing collector configuration.
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

/// Error returned when static metric metadata violates the shared contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricMetadataError {
    field: &'static str,
}

impl MetricMetadataError {
    const fn new(field: &'static str) -> Self {
        Self { field }
    }
}

impl Display for MetricMetadataError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid static telemetry {}", self.field)
    }
}

impl Error for MetricMetadataError {}

/// A validated, low-cardinality metric attribute.
#[derive(Clone, Debug)]
pub struct Attribute(KeyValue);

impl Attribute {
    /// Creates a static string attribute.
    ///
    /// Requiring a static value prevents request-, user-, and payload-derived
    /// strings from entering metric labels through this API.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is malformed or sensitive, or when the
    /// static value is empty, oversized, or contains a control character.
    pub fn string(key: &'static str, value: &'static str) -> Result<Self, MetricMetadataError> {
        validate_attribute_key(key)?;
        if !valid_attribute_value(value) {
            return Err(MetricMetadataError::new("attribute value"));
        }
        Ok(Self(KeyValue::new(key, value)))
    }

    /// Creates a boolean attribute with a validated static key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is malformed or sensitive.
    pub fn boolean(key: &'static str, value: bool) -> Result<Self, MetricMetadataError> {
        validate_attribute_key(key)?;
        Ok(Self(KeyValue::new(key, value)))
    }

    /// Creates an integer attribute with a validated static key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is malformed or sensitive.
    pub fn integer(key: &'static str, value: i64) -> Result<Self, MetricMetadataError> {
        validate_attribute_key(key)?;
        Ok(Self(KeyValue::new(key, value)))
    }
}

/// A process-global meter that exposes only reviewed instrument shapes.
#[derive(Clone)]
pub struct Meter(OtelMeter);

impl Meter {
    /// Creates a meter for a validated static instrumentation scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the meter name is empty, oversized, or contains
    /// a character outside the stable identifier alphabet.
    pub fn new(name: &'static str) -> Result<Self, MetricMetadataError> {
        validate_metric_name(name, "meter name")?;
        Ok(Self(global::meter(name)))
    }

    /// Creates a monotonic unsigned counter.
    ///
    /// # Errors
    ///
    /// Returns an error when the static name, description, or unit violates
    /// the bounded metric metadata contract.
    pub fn u64_counter(
        &self,
        name: &'static str,
        description: &'static str,
        unit: &'static str,
    ) -> Result<U64Counter, MetricMetadataError> {
        validate_instrument_metadata(name, description, unit)?;
        Ok(U64Counter(
            self.0
                .u64_counter(name)
                .with_description(description)
                .with_unit(unit)
                .build(),
        ))
    }

    /// Creates a floating-point histogram.
    ///
    /// # Errors
    ///
    /// Returns an error when the static name, description, or unit violates
    /// the bounded metric metadata contract.
    pub fn f64_histogram(
        &self,
        name: &'static str,
        description: &'static str,
        unit: &'static str,
    ) -> Result<F64Histogram, MetricMetadataError> {
        validate_instrument_metadata(name, description, unit)?;
        Ok(F64Histogram(
            self.0
                .f64_histogram(name)
                .with_description(description)
                .with_unit(unit)
                .build(),
        ))
    }
}

/// A validated monotonic unsigned counter.
#[derive(Clone)]
pub struct U64Counter(OtelCounter<u64>);

impl U64Counter {
    /// Adds a value with validated, bounded-cardinality attributes.
    pub fn add(&self, value: u64, attributes: &[Attribute]) {
        self.0.add(value, &otel_attributes(attributes));
    }
}

/// A validated floating-point histogram.
#[derive(Clone)]
pub struct F64Histogram(OtelHistogram<f64>);

impl F64Histogram {
    /// Records a value with validated, bounded-cardinality attributes.
    pub fn record(&self, value: f64, attributes: &[Attribute]) {
        self.0.record(value, &otel_attributes(attributes));
    }
}

fn otel_attributes(attributes: &[Attribute]) -> Vec<KeyValue> {
    attributes.iter().map(|value| value.0.clone()).collect()
}

fn build_tracer_provider(
    endpoint: &str,
    resource: Resource,
    instrumentation_name: &'static str,
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
    let tracer = provider.tracer(instrumentation_name);
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
    config: &TelemetryConfig,
    tracer: Option<SdkTracer>,
    logger_provider: Option<&SdkLoggerProvider>,
) -> bool {
    let filter = safe_log_filter(config);
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

fn safe_log_filter(config: &TelemetryConfig) -> EnvFilter {
    let default_level = configured_log_level(
        std::env::var_os("ORES_LOG_LEVEL").as_deref(),
        config.default_log_level,
    );
    let mut directives = default_level.as_str().to_string();
    for (target, level) in &config.target_levels {
        directives.push(',');
        directives.push_str(target);
        directives.push('=');
        directives.push_str(level.as_str());
    }
    // Dependency targets that can contain protocol or peer metadata remain
    // bounded regardless of application target overrides. `RUST_LOG` is not
    // consumed because it accepts arbitrary directives.
    directives.push_str(
        ",rmcp=off,hyper=warn,hyper_util=warn,h2=warn,reqwest=warn,tonic=warn,tower=warn,tower_http=warn,axum=warn,tungstenite=warn,tokio_tungstenite=warn",
    );
    EnvFilter::new(directives)
}

fn configured_log_level(value: Option<&OsStr>, default: LogLevel) -> LogLevel {
    match value.and_then(OsStr::to_str) {
        Some("error") => LogLevel::Error,
        Some("info") => LogLevel::Info,
        Some(_) => LogLevel::Warn,
        None => default,
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

fn resource(service_name: &str, service_namespace: &str, service_version: &str) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", service_name.to_string()),
        KeyValue::new("service.namespace", service_namespace.to_string()),
        KeyValue::new("service.version", service_version.to_string()),
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
    let Ok(url) = url::Url::parse(value) else {
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
    if valid_identifier(value) {
        Some(value)
    } else {
        None
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTITY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_metric_name(
    value: &'static str,
    field: &'static str,
) -> Result<(), MetricMetadataError> {
    if valid_identifier(value) {
        Ok(())
    } else {
        Err(MetricMetadataError::new(field))
    }
}

fn validate_instrument_metadata(
    name: &'static str,
    description: &'static str,
    unit: &'static str,
) -> Result<(), MetricMetadataError> {
    validate_metric_name(name, "instrument name")?;
    if description.is_empty()
        || description.len() > MAX_DESCRIPTION_BYTES
        || description.chars().any(char::is_control)
    {
        return Err(MetricMetadataError::new("instrument description"));
    }
    if unit.is_empty() || unit.len() > MAX_UNIT_BYTES || unit.chars().any(char::is_control) {
        return Err(MetricMetadataError::new("instrument unit"));
    }
    Ok(())
}

fn validate_attribute_key(key: &'static str) -> Result<(), MetricMetadataError> {
    validate_metric_name(key, "attribute key")?;
    let normalized = key.to_ascii_lowercase();
    if [
        "authorization",
        "cookie",
        "password",
        "private_key",
        "secret",
        "session",
        "token",
    ]
    .iter()
    .any(|sensitive| normalized.contains(sensitive))
    {
        return Err(MetricMetadataError::new("sensitive attribute key"));
    }
    Ok(())
}

fn valid_attribute_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ATTRIBUTE_VALUE_BYTES
        && !value.chars().any(char::is_control)
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
    fn log_level_is_closed_and_rejects_arbitrary_directives() {
        assert_eq!(configured_log_level(None, LogLevel::Info), LogLevel::Info);
        assert_eq!(
            configured_log_level(Some(OsStr::new("error")), LogLevel::Info),
            LogLevel::Error
        );
        assert_eq!(
            configured_log_level(Some(OsStr::new("debug")), LogLevel::Info),
            LogLevel::Warn
        );
        assert_eq!(
            configured_log_level(Some(OsStr::new("rmcp=trace")), LogLevel::Info),
            LogLevel::Warn
        );
    }

    #[test]
    fn metric_attributes_reject_sensitive_and_dynamic_shapes() {
        assert!(Attribute::string("audit.outcome", "pass").is_ok());
        assert!(Attribute::boolean("audit.cached", true).is_ok());
        assert!(Attribute::string("user.token", "redacted").is_err());
        assert!(Attribute::string("bad key", "pass").is_err());
    }

    #[test]
    fn instrument_metadata_is_static_and_bounded() {
        assert!(Meter::new("canonical-cli").is_ok());
        assert!(Meter::new("bad meter").is_err());
        let meter = Meter::new("test-meter").expect("valid static meter");
        assert!(
            meter
                .u64_counter("audit.runs", "Completed audit runs", "{run}")
                .is_ok()
        );
        assert!(meter.u64_counter("bad name", "description", "1").is_err());
    }
}
