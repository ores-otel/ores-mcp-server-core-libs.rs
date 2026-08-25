# Observability contract

The workspace's lightweight `ores-telemetry` package owns provider setup,
validated configuration, and bounded shutdown for every Rust application.
The MCP crate re-exports that boundary through
`observability::init(service_name, service_namespace)` and adds only the
closed MCP tool metric vocabulary. Both entry points install a JSON `tracing`
subscriber that writes to stderr and return a `TelemetryGuard`. Retain the
guard until process shutdown; its `Drop` implementation flushes and shuts down
trace, metric, and log providers with bounded timeouts.

## MCP stdio safety

Stdout is exclusively for MCP protocol messages. Never point a tracing writer,
Collector debug exporter, panic formatter, or application logger at the MCP
server's stdout stream. The library's local JSON layer is hard-wired to stderr.

## OTLP

When `OTEL_EXPORTER_OTLP_ENDPOINT` contains a valid HTTP(S) collector endpoint,
the library independently attempts to build exporters for all three signals:

| Signal | Intended backend path |
| --- | --- |
| Traces | OTLP Collector to Tempo |
| Metrics | OTLP Collector to Prometheus-compatible exporter/backend |
| Logs | OTLP Collector to Loki native OTLP |

Each exporter can fail independently. Failures do not stop the MCP server and
do not print endpoint or authorization details. `TelemetryGuard::status()`
reports only booleans describing which exporters were built.

Use TLS for collectors outside a trusted local network. The endpoint must not
contain userinfo, query parameters, or fragments. Configure authentication
through Collector/network policy and standard OTLP header environment rather
than embedding credentials in the URL.

## Local JSON events

The library itself emits only stable service identity and exporter booleans.
It never emits MCP arguments/results, AI prompts/responses, model IDs, HTTP
bodies, headers, secrets, repository names, error bodies, or arbitrary user
metadata. Downstream spans and events must follow the same rule.

## Tool metrics

`ToolMetrics` records:

- `mcp.server.tool.calls` counter;
- `mcp.server.tool.duration` histogram in milliseconds.

The only attributes are `mcp.tool.class` and `mcp.tool.outcome`, both closed
enums. This prevents user-controlled or repository-controlled label growth.
Dropping a `ToolTimer` without calling `finish` records `cancelled`.

## Collector example

[`deploy/otel-collector.yaml`](deploy/otel-collector.yaml) is an illustrative
OpenTelemetry Collector Contrib configuration. Adjust service DNS names, TLS,
authentication, retention, and tenancy headers for the deployment. The
Prometheus exporter exposes a scrape endpoint on port 9464; it is not a
durable metrics store by itself.
