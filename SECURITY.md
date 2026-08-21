# Security policy

## Reporting

Report suspected vulnerabilities privately through this repository's GitHub
Security Advisories. Do not open a public issue containing credentials,
prompts, provider responses, collector headers, or exploit details. Revoke any
credential that may have been exposed before sharing a redacted report.

## Supported versions

Until a stable release series exists, only the current `main` branch is
supported. Downstream MCP servers should pin a reviewed commit SHA and update
only after their tests and security review pass.

## Trust boundaries

- API keys and OTLP authorization headers are secrets. Load them from a secret
  manager or process environment and never commit them.
- Repository metadata, issue text, logs, model input, and model output are
  untrusted. A model response is advisory text, not executable instructions.
- The provider registry's `Ready` state is configuration-only. It is not an
  authenticated health check and does not prove model access or quota.
- This crate intentionally has no filesystem, process, shell, GitHub mutation,
  tool execution, or repair-application capability.

## Secret files

Only SOPS-encrypted dotenv artifacts matching `env/enc/*.env.enc` may be
committed. Decrypted `env/dec/*.env` files, the active `.sops.yaml`, `.age/`,
and conventional local age-key filenames are ignored. Keep private age
identities outside the repository whenever possible and restrict any local
identity file to mode `0600`.

The `encrypt-all` and `decrypt-all` recipes set `umask 077`, process only
direct regular files with the documented suffixes, write to a same-directory
mode-`0600` temporary file, and atomically replace a destination only after
SOPS succeeds. Shell tracing is disabled, SOPS diagnostics are suppressed, and
the recipes do not intentionally print file contents or secret values. Inspect
staged changes before every commit; Git ignore rules are a backstop, not a
secret scanner. A leaked credential must be revoked and rotated even if the
plaintext file is subsequently removed.

## Network controls

Provider connections use rustls, WebPKI roots, fixed HTTPS origins, disabled
redirects, disabled proxies, and bounded time and memory. API keys are sent in
sensitive headers. Gemini model IDs are restricted to a path-safe alphabet so
configuration cannot change the origin or inject URL components.

The OTLP Collector endpoint is operator-controlled and may use HTTP for a
local or private collector. It rejects embedded credentials, query strings,
fragments, and non-HTTP schemes. Protect remote collectors with TLS and network
policy.

MCP network transports bind to loopback by default. Non-loopback binds fail
closed unless `allow_remote` is explicit. Streamable HTTP validates `Host` and
every present `Origin`; WebSocket requires and validates both headers. Raw TCP
NDJSON and WebSocket are non-standard compatibility transports and contain no
built-in TLS or authentication. Remote use requires TLS, authentication,
authorization, rate limiting, and network policy at a trusted reverse proxy or
equivalent boundary.

Stdio, TCP NDJSON, WebSocket text, and Streamable HTTP bodies are bounded to at
most 256 KiB before JSON deserialization. TCP and WebSocket additionally cap
live connections. Operators may lower these limits but cannot raise them past
the compiled ceiling.

## Logging and telemetry

The library never records model input, model output, HTTP bodies, headers, API
keys, or model IDs. Downstream code must preserve that contract. Use only the
closed `ToolClass` and `ToolOutcome` enums for metric labels; do not add user,
repository, request, error-message, or payload values as metric attributes.

`RUST_LOG` is intentionally ignored. `ORES_LOG_LEVEL` accepts only `error`,
`warn`, or `info`, and fixed filter directives disable RMCP events while
holding network/protocol dependencies at `warn`. This prevents caller-supplied
peer metadata and notification bodies from being re-enabled in stderr or OTLP
logs by an environment directive.

MCP stdio uses stdout for protocol frames. Application logs must remain on
stderr or be exported through OTLP. Writing logs to stdout can corrupt the MCP
session. Transport code logs only closed event names and configuration-safe
listener metadata; it never logs frames, HTTP bodies, headers, or peer payloads.
