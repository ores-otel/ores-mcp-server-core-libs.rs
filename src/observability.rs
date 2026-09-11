//! MCP-specific observability built on the shared ORES telemetry boundary.
//!
//! Provider ownership, OTLP configuration, stderr-safe JSON logging, resource
//! attributes, and bounded shutdown live in the lightweight `ores-telemetry`
//! package. This module retains the MCP-specific closed metric vocabulary.

use std::time::{Duration, Instant};

use ores_telemetry::{Attribute, F64Histogram, Meter, U64Counter};

pub use ores_telemetry::{
    LogLevel, MetricMetadataError, TelemetryConfig, TelemetryGuard, TelemetryStatus, init,
    init_with_config,
};

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
    calls: U64Counter,
    duration: F64Histogram,
}

impl ToolMetrics {
    /// Creates instruments from the process-global meter provider.
    ///
    /// # Panics
    ///
    /// Panics only if this module's compile-time MCP metric names, descriptions,
    /// units, or attribute vocabulary are changed to violate the shared static
    /// metadata contract.
    #[must_use]
    pub fn global() -> Self {
        let meter = Meter::new("ores-mcp-server").expect("static MCP meter name is valid");
        let calls = meter
            .u64_counter(
                "mcp.server.tool.calls",
                "Number of MCP tool calls completed",
                "{call}",
            )
            .expect("static MCP counter metadata is valid");
        let duration = meter
            .f64_histogram("mcp.server.tool.duration", "MCP tool-call duration", "ms")
            .expect("static MCP histogram metadata is valid");
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
            Attribute::string("mcp.tool.class", class.as_str())
                .expect("closed MCP tool class is valid"),
            Attribute::string("mcp.tool.outcome", outcome.as_str())
                .expect("closed MCP tool outcome is valid"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_labels_are_closed_enums() {
        assert_eq!(ToolClass::Discovery.as_str(), "discovery");
        assert_eq!(ToolOutcome::Error.as_str(), "error");
    }
}
