//! Hardened MCP transport runners shared by organization servers.
//!
//! Streamable HTTP at `/mcp` is a standard MCP transport. Stdio uses the
//! standard newline-delimited framing. Raw TCP NDJSON and WebSocket at
//! `/mcp/ws` are explicitly non-standard compatibility transports. Every
//! byte-stream path enforces a 256 KiB ceiling before JSON deserialization.
//! Network transports bind to loopback by default and require an explicit
//! `allow_remote` opt-in for any non-loopback address.

use std::{env, future::IntoFuture, io, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{
        HeaderMap, Request, StatusCode, Uri,
        header::{HOST, ORIGIN},
        uri::Authority,
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        Transport,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
        },
    },
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::state_machine::{LifecycleController, LifecycleEvent, LifecycleState};

/// Cancellation primitive accepted by every transport runner.
pub use tokio_util::sync::CancellationToken;

/// Hard pre-deserialization ceiling for one MCP JSON message or HTTP body.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// Standard Streamable HTTP endpoint.
pub const STREAMABLE_HTTP_PATH: &str = "/mcp";
/// Non-standard WebSocket compatibility endpoint.
pub const WEBSOCKET_PATH: &str = "/mcp/ws";

const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const MAX_SHUTDOWN_GRACE: Duration = Duration::from_secs(300);
const DEFAULT_MAX_CONNECTIONS: usize = 64;
const MAX_CONNECTIONS: usize = 4_096;

/// Configuration for the standard stateless Streamable HTTP transport.
#[derive(Clone, Debug)]
pub struct HttpTransportConfig {
    /// Socket address on which `/mcp` is served.
    pub bind: SocketAddr,
    /// Exact allowed HTTP `Host` authorities.
    pub allowed_hosts: Vec<String>,
    /// Exact allowed browser origins. Missing `Origin` remains valid for
    /// non-browser MCP clients; a present origin must match.
    pub allowed_origins: Vec<String>,
    /// Maximum request body, never greater than [`MAX_MESSAGE_BYTES`].
    pub max_body_bytes: usize,
    /// Required opt-in for a non-loopback bind.
    pub allow_remote: bool,
    /// Maximum graceful-shutdown duration.
    pub shutdown_grace: Duration,
}

impl Default for HttpTransportConfig {
    fn default() -> Self {
        Self::for_bind(SocketAddr::from(([127, 0, 0, 1], 3_000)))
    }
}

impl HttpTransportConfig {
    /// Builds safe allowlists for an explicit bind address.
    #[must_use]
    pub fn for_bind(bind: SocketAddr) -> Self {
        let (allowed_hosts, allowed_origins) = local_header_defaults(bind);
        Self {
            bind,
            allowed_hosts,
            allowed_origins,
            max_body_bytes: MAX_MESSAGE_BYTES,
            allow_remote: false,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Reads optional `MCP_HTTP_*` environment overrides.
    ///
    /// Supported suffixes are `BIND`, `ALLOWED_HOSTS`, `ALLOWED_ORIGINS`,
    /// `MAX_BODY_BYTES`, `ALLOW_REMOTE`, and `SHUTDOWN_GRACE_SECONDS`.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed or unsafe configuration.
    pub fn from_env() -> io::Result<Self> {
        let bind = env_socket("MCP_HTTP_BIND")?.unwrap_or_else(|| Self::default().bind);
        let mut config = Self::for_bind(bind);
        override_list("MCP_HTTP_ALLOWED_HOSTS", &mut config.allowed_hosts)?;
        override_list("MCP_HTTP_ALLOWED_ORIGINS", &mut config.allowed_origins)?;
        override_usize("MCP_HTTP_MAX_BODY_BYTES", &mut config.max_body_bytes)?;
        override_bool("MCP_HTTP_ALLOW_REMOTE", &mut config.allow_remote)?;
        override_duration(
            "MCP_HTTP_SHUTDOWN_GRACE_SECONDS",
            &mut config.shutdown_grace,
        )?;
        config.validate()?;
        Ok(config)
    }

    /// Validates all bounds and network trust-boundary controls.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when a bound or allowlist is unsafe.
    pub fn validate(&self) -> io::Result<()> {
        validate_network_bind(self.bind, self.allow_remote)?;
        validate_message_limit(self.max_body_bytes)?;
        validate_grace(self.shutdown_grace)?;
        HeaderPolicy::new(&self.allowed_hosts, &self.allowed_origins, false)?;
        Ok(())
    }
}

/// Configuration for the non-standard raw TCP NDJSON transport.
#[derive(Clone, Debug)]
pub struct TcpTransportConfig {
    /// TCP listen address.
    pub bind: SocketAddr,
    /// Maximum simultaneous connections.
    pub max_connections: usize,
    /// Maximum line size before JSON deserialization.
    pub max_message_bytes: usize,
    /// Required opt-in for a non-loopback bind.
    pub allow_remote: bool,
    /// Maximum graceful-shutdown duration.
    pub shutdown_grace: Duration,
}

impl Default for TcpTransportConfig {
    fn default() -> Self {
        Self::for_bind(SocketAddr::from(([127, 0, 0, 1], 3_001)))
    }
}

impl TcpTransportConfig {
    /// Builds a loopback-safe configuration for an explicit address.
    #[must_use]
    pub const fn for_bind(bind: SocketAddr) -> Self {
        Self {
            bind,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_message_bytes: MAX_MESSAGE_BYTES,
            allow_remote: false,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Reads optional `MCP_TCP_*` environment overrides.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed or unsafe configuration.
    pub fn from_env() -> io::Result<Self> {
        let bind = env_socket("MCP_TCP_BIND")?.unwrap_or_else(|| Self::default().bind);
        let mut config = Self::for_bind(bind);
        override_usize("MCP_TCP_MAX_CONNECTIONS", &mut config.max_connections)?;
        override_usize("MCP_TCP_MAX_MESSAGE_BYTES", &mut config.max_message_bytes)?;
        override_bool("MCP_TCP_ALLOW_REMOTE", &mut config.allow_remote)?;
        override_duration("MCP_TCP_SHUTDOWN_GRACE_SECONDS", &mut config.shutdown_grace)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates all bounds and network trust-boundary controls.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when a bound or cap is unsafe.
    pub fn validate(&self) -> io::Result<()> {
        validate_network_bind(self.bind, self.allow_remote)?;
        validate_connections(self.max_connections)?;
        validate_message_limit(self.max_message_bytes)?;
        validate_grace(self.shutdown_grace)
    }
}

/// Configuration for the non-standard WebSocket compatibility transport.
#[derive(Clone, Debug)]
pub struct WebSocketTransportConfig {
    /// Socket address on which `/mcp/ws` is served.
    pub bind: SocketAddr,
    /// Exact allowed handshake `Host` authorities.
    pub allowed_hosts: Vec<String>,
    /// Exact allowed handshake origins. WebSocket handshakes must include one.
    pub allowed_origins: Vec<String>,
    /// Maximum simultaneous upgraded connections.
    pub max_connections: usize,
    /// Maximum text frame/message size before JSON deserialization.
    pub max_message_bytes: usize,
    /// Required opt-in for a non-loopback bind.
    pub allow_remote: bool,
    /// Maximum graceful-shutdown duration.
    pub shutdown_grace: Duration,
}

impl Default for WebSocketTransportConfig {
    fn default() -> Self {
        Self::for_bind(SocketAddr::from(([127, 0, 0, 1], 3_002)))
    }
}

impl WebSocketTransportConfig {
    /// Builds safe allowlists for an explicit bind address.
    #[must_use]
    pub fn for_bind(bind: SocketAddr) -> Self {
        let (allowed_hosts, allowed_origins) = local_header_defaults(bind);
        Self {
            bind,
            allowed_hosts,
            allowed_origins,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_message_bytes: MAX_MESSAGE_BYTES,
            allow_remote: false,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Reads optional `MCP_WS_*` environment overrides.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed or unsafe configuration.
    pub fn from_env() -> io::Result<Self> {
        let bind = env_socket("MCP_WS_BIND")?.unwrap_or_else(|| Self::default().bind);
        let mut config = Self::for_bind(bind);
        override_list("MCP_WS_ALLOWED_HOSTS", &mut config.allowed_hosts)?;
        override_list("MCP_WS_ALLOWED_ORIGINS", &mut config.allowed_origins)?;
        override_usize("MCP_WS_MAX_CONNECTIONS", &mut config.max_connections)?;
        override_usize("MCP_WS_MAX_MESSAGE_BYTES", &mut config.max_message_bytes)?;
        override_bool("MCP_WS_ALLOW_REMOTE", &mut config.allow_remote)?;
        override_duration("MCP_WS_SHUTDOWN_GRACE_SECONDS", &mut config.shutdown_grace)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates all bounds and network trust-boundary controls.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when a bound or allowlist is unsafe.
    pub fn validate(&self) -> io::Result<()> {
        validate_network_bind(self.bind, self.allow_remote)?;
        validate_connections(self.max_connections)?;
        validate_message_limit(self.max_message_bytes)?;
        validate_grace(self.shutdown_grace)?;
        HeaderPolicy::new(&self.allowed_hosts, &self.allowed_origins, true)?;
        Ok(())
    }
}

fn local_header_defaults(bind: SocketAddr) -> (Vec<String>, Vec<String>) {
    let mut hosts = vec![bind.to_string()];
    let mut origins = vec![format!("http://{bind}")];
    if bind.ip().is_loopback() {
        hosts.push(format!("localhost:{}", bind.port()));
        origins.push(format!("http://localhost:{}", bind.port()));
    }
    (hosts, origins)
}

fn invalid_config(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_network_bind(bind: SocketAddr, allow_remote: bool) -> io::Result<()> {
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(invalid_config(
            "non-loopback bind requires explicit allow_remote=true",
        ));
    }
    Ok(())
}

fn validate_message_limit(value: usize) -> io::Result<()> {
    if value == 0 || value > MAX_MESSAGE_BYTES {
        return Err(invalid_config("message limit must be in 1..=262144"));
    }
    Ok(())
}

fn validate_connections(value: usize) -> io::Result<()> {
    if value == 0 || value > MAX_CONNECTIONS {
        return Err(invalid_config("connection cap must be in 1..=4096"));
    }
    Ok(())
}

fn validate_grace(value: Duration) -> io::Result<()> {
    if value.is_zero() || value > MAX_SHUTDOWN_GRACE {
        return Err(invalid_config("shutdown grace must be in 1..=300 seconds"));
    }
    Ok(())
}

fn env_value(name: &'static str) -> io::Result<Option<String>> {
    env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| invalid_config("transport environment value is not UTF-8"))
        })
        .transpose()
}

fn env_socket(name: &'static str) -> io::Result<Option<SocketAddr>> {
    env_value(name)?
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid_config("transport bind environment value is invalid"))
        })
        .transpose()
}

fn override_list(name: &'static str, target: &mut Vec<String>) -> io::Result<()> {
    if let Some(value) = env_value(name)? {
        let parsed: Vec<_> = value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if parsed.is_empty() {
            return Err(invalid_config("transport allowlist must not be empty"));
        }
        *target = parsed;
    }
    Ok(())
}

fn override_usize(name: &'static str, target: &mut usize) -> io::Result<()> {
    if let Some(value) = env_value(name)? {
        *target = value
            .parse()
            .map_err(|_| invalid_config("transport numeric environment value is invalid"))?;
    }
    Ok(())
}

fn override_bool(name: &'static str, target: &mut bool) -> io::Result<()> {
    if let Some(value) = env_value(name)? {
        *target = match value.as_str() {
            "1" | "true" | "TRUE" => true,
            "0" | "false" | "FALSE" => false,
            _ => {
                return Err(invalid_config(
                    "transport boolean environment value is invalid",
                ));
            }
        };
    }
    Ok(())
}

fn override_duration(name: &'static str, target: &mut Duration) -> io::Result<()> {
    if let Some(value) = env_value(name)? {
        let seconds = value
            .parse()
            .map_err(|_| invalid_config("transport duration environment value is invalid"))?;
        *target = Duration::from_secs(seconds);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedHost {
    host: String,
    port: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedOrigin {
    scheme: String,
    host: String,
    effective_port: u16,
}

#[derive(Clone, Debug)]
struct HeaderPolicy {
    hosts: Vec<NormalizedHost>,
    origins: Vec<NormalizedOrigin>,
    require_origin: bool,
}

#[derive(Clone, Copy, Debug)]
enum HeaderRejection {
    BadRequest,
    Forbidden,
}

impl HeaderRejection {
    fn response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest => (StatusCode::BAD_REQUEST, "invalid transport headers"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "transport headers are not allowed"),
        };
        (status, message).into_response()
    }
}

impl HeaderPolicy {
    fn new(hosts: &[String], origins: &[String], require_origin: bool) -> io::Result<Self> {
        if hosts.is_empty() || origins.is_empty() {
            return Err(invalid_config(
                "Host and Origin allowlists must not be empty",
            ));
        }
        let hosts = hosts
            .iter()
            .map(|value| parse_host(value).ok_or_else(|| invalid_config("invalid allowed Host")))
            .collect::<io::Result<Vec<_>>>()?;
        let origins = origins
            .iter()
            .map(|value| {
                parse_origin(value).ok_or_else(|| invalid_config("invalid allowed Origin"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            hosts,
            origins,
            require_origin,
        })
    }

    fn validate(&self, headers: &HeaderMap) -> Result<(), HeaderRejection> {
        let host = headers
            .get(HOST)
            .ok_or(HeaderRejection::BadRequest)?
            .to_str()
            .ok()
            .and_then(parse_host)
            .ok_or(HeaderRejection::BadRequest)?;
        if !self.hosts.contains(&host) {
            return Err(HeaderRejection::Forbidden);
        }

        match headers.get(ORIGIN) {
            Some(value) => {
                let origin = value
                    .to_str()
                    .ok()
                    .and_then(parse_origin)
                    .ok_or(HeaderRejection::BadRequest)?;
                if !self.origins.contains(&origin) {
                    return Err(HeaderRejection::Forbidden);
                }
            }
            None if self.require_origin => return Err(HeaderRejection::BadRequest),
            None => {}
        }
        Ok(())
    }
}

fn parse_host(value: &str) -> Option<NormalizedHost> {
    let authority = Authority::from_str(value.trim()).ok()?;
    Some(NormalizedHost {
        host: normalize_ip_or_name(authority.host()),
        port: authority.port_u16(),
    })
}

fn parse_origin(value: &str) -> Option<NormalizedOrigin> {
    let uri = Uri::from_str(value.trim()).ok()?;
    // `http::Uri` normalizes an absent absolute-URI path to `/`.
    if uri.path() != "/" || uri.query().is_some() {
        return None;
    }
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = uri.authority()?;
    Some(NormalizedOrigin {
        scheme,
        host: normalize_ip_or_name(authority.host()),
        effective_port: authority.port_u16().unwrap_or(default_port),
    })
}

fn normalize_ip_or_name(value: &str) -> String {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

async fn strict_header_middleware(
    policy: Arc<HeaderPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Err(rejection) = policy.validate(request.headers()) {
        return rejection.response();
    }
    next.run(request).await
}

fn lifecycle_error(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn transition(lifecycle: &LifecycleController, event: LifecycleEvent) -> io::Result<()> {
    lifecycle
        .transition(event)
        .map(|_| ())
        .map_err(|_| lifecycle_error("invalid transport lifecycle transition"))
}

fn mark_degraded(lifecycle: &LifecycleController) {
    if lifecycle
        .snapshot()
        .is_ok_and(|snapshot| snapshot.state() == LifecycleState::Ready)
    {
        let _ = lifecycle.transition(LifecycleEvent::Degrade);
    }
}

fn mark_recovered(lifecycle: &LifecycleController) {
    if lifecycle
        .snapshot()
        .is_ok_and(|snapshot| snapshot.state() == LifecycleState::Degraded)
    {
        let _ = lifecycle.transition(LifecycleEvent::Recover);
    }
}

fn finish_lifecycle(lifecycle: &LifecycleController) -> io::Result<()> {
    let state = lifecycle
        .snapshot()
        .map_err(|_| lifecycle_error("transport lifecycle is unavailable"))?
        .state();
    match state {
        LifecycleState::Created | LifecycleState::Draining => {
            transition(lifecycle, LifecycleEvent::Stop)
        }
        LifecycleState::Starting => transition(lifecycle, LifecycleEvent::Drain)
            .and_then(|()| transition(lifecycle, LifecycleEvent::Stop)),
        LifecycleState::Ready | LifecycleState::Degraded => {
            transition(lifecycle, LifecycleEvent::Drain)
                .and_then(|()| transition(lifecycle, LifecycleEvent::Stop))
        }
        LifecycleState::Stopped => Ok(()),
    }
}

#[derive(Debug, Error)]
enum WireTransportError {
    #[error("transport input/output failed")]
    Io,
    #[error("transport JSON serialization failed")]
    Serialization,
    #[error("transport message exceeded the configured bound")]
    MessageTooLarge,
    #[error("websocket transport failed")]
    WebSocket,
}

struct BoundedNdjsonTransport<R, W> {
    reader: FramedRead<R, LinesCodec>,
    writer: Arc<Mutex<Option<FramedWrite<W, LinesCodec>>>>,
    max_message_bytes: usize,
}

impl<R, W> BoundedNdjsonTransport<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    fn new(reader: R, writer: W, max_message_bytes: usize) -> Self {
        Self {
            reader: FramedRead::new(reader, LinesCodec::new_with_max_length(max_message_bytes)),
            writer: Arc::new(Mutex::new(Some(FramedWrite::new(
                writer,
                LinesCodec::new_with_max_length(max_message_bytes),
            )))),
            max_message_bytes,
        }
    }
}

impl<R, W> Transport<RoleServer> for BoundedNdjsonTransport<R, W>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = WireTransportError;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        let max_message_bytes = self.max_message_bytes;
        async move {
            let encoded =
                serde_json::to_string(&item).map_err(|_| WireTransportError::Serialization)?;
            if encoded.len() > max_message_bytes {
                return Err(WireTransportError::MessageTooLarge);
            }
            let mut writer = writer.lock().await;
            writer
                .as_mut()
                .ok_or(WireTransportError::Io)?
                .send(encoded)
                .await
                .map_err(|_| WireTransportError::Io)
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let Ok(frame) = self.reader.next().await? else {
            tracing::warn!(
                transport_event = "frame_rejected",
                "transport frame rejected"
            );
            return None;
        };
        let Ok(message) = serde_json::from_str(&frame) else {
            tracing::warn!(transport_event = "json_rejected", "transport JSON rejected");
            return None;
        };
        Some(message)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        if let Some(mut writer) = self.writer.lock().await.take() {
            futures::SinkExt::<String>::close(&mut writer)
                .await
                .map_err(|_| WireTransportError::Io)?;
        }
        Ok(())
    }
}

/// Runs standard MCP stdio with bounded newline-delimited JSON framing.
///
/// Stdout is reserved exclusively for MCP frames. Application logs must stay
/// on stderr (the crate's observability initializer already enforces that).
///
/// # Errors
///
/// Returns an error when lifecycle setup, handler construction, protocol
/// initialization, or the underlying service task fails.
pub async fn run_stdio<S, F>(
    factory: F,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<()>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    transition(&lifecycle, LifecycleEvent::Start)?;
    let service = match factory() {
        Ok(service) => service,
        Err(error) => {
            finish_lifecycle(&lifecycle)?;
            return Err(error);
        }
    };
    let transport =
        BoundedNdjsonTransport::new(tokio::io::stdin(), tokio::io::stdout(), MAX_MESSAGE_BYTES);
    transition(&lifecycle, LifecycleEvent::Started)?;
    let result = match service.serve_with_ct(transport, cancellation).await {
        Ok(running) => running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|_| io::Error::other("MCP stdio service task failed")),
        Err(_) => Err(io::Error::other("MCP stdio initialization failed")),
    };
    if result.is_err() {
        mark_degraded(&lifecycle);
    }
    finish_lifecycle(&lifecycle)?;
    result
}

fn build_http_router<S, F>(
    factory: F,
    config: &HttpTransportConfig,
    lifecycle: &LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<Router>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    let policy = Arc::new(HeaderPolicy::new(
        &config.allowed_hosts,
        &config.allowed_origins,
        false,
    )?);
    let factory_lifecycle = LifecycleController::clone(lifecycle);
    let rmcp_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancellation)
        .with_allowed_hosts(config.allowed_hosts.clone())
        .with_allowed_origins(config.allowed_origins.clone())
        .with_max_request_body_bytes(config.max_body_bytes)
        .with_stateless_protocol_metadata_required(true);
    let service = StreamableHttpService::new(
        move || {
            let result = factory();
            if result.is_err() {
                mark_degraded(&factory_lifecycle);
            } else {
                mark_recovered(&factory_lifecycle);
            }
            result
        },
        Arc::new(NeverSessionManager::default()),
        rmcp_config,
    );
    Ok(Router::new()
        .route_service(STREAMABLE_HTTP_PATH, service)
        .layer(middleware::from_fn(move |request, next| {
            strict_header_middleware(Arc::clone(&policy), request, next)
        })))
}

/// Runs the official modern, stateless Streamable HTTP transport at `/mcp`.
///
/// The RMCP service uses `NeverSessionManager`, disables legacy sessions,
/// requests JSON responses, requires stateless protocol metadata, and enforces
/// the body ceiling while streaming before JSON deserialization.
///
/// # Errors
///
/// Returns an error for invalid configuration, lifecycle failure, bind or
/// serving failure, or a graceful-shutdown timeout.
pub async fn run_streamable_http<S, F>(
    factory: F,
    config: HttpTransportConfig,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<()>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    config.validate()?;
    transition(&lifecycle, LifecycleEvent::Start)?;
    let listener = match TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            finish_lifecycle(&lifecycle)?;
            return Err(error);
        }
    };
    warn_remote(config.bind, config.allow_remote, "streamable_http");
    let app =
        match build_http_router::<S, F>(factory, &config, &lifecycle, cancellation.child_token()) {
            Ok(app) => app,
            Err(error) => {
                finish_lifecycle(&lifecycle)?;
                return Err(error);
            }
        };
    transition(&lifecycle, LifecycleEvent::Started)?;
    tracing::info!(
        transport = "streamable_http",
        bind = %config.bind,
        path = STREAMABLE_HTTP_PATH,
        "MCP transport ready"
    );

    let shutdown = cancellation.clone().cancelled_owned();
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .into_future();
    tokio::pin!(serving);
    let mut cancelled = false;
    let result = tokio::select! {
        result = &mut serving => result,
        () = cancellation.cancelled() => {
            cancelled = true;
            match transition(&lifecycle, LifecycleEvent::Drain) {
                Ok(()) => match tokio::time::timeout(config.shutdown_grace, &mut serving).await {
                    Ok(result) => result,
                    Err(_) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "HTTP transport shutdown timed out",
                    )),
                },
                Err(error) => Err(error),
            }
        }
    };
    if result.is_err() && !cancelled {
        mark_degraded(&lifecycle);
    }
    finish_lifecycle(&lifecycle)?;
    result
}

fn warn_remote(bind: SocketAddr, allow_remote: bool, transport: &'static str) {
    if allow_remote && !bind.ip().is_loopback() {
        tracing::warn!(
            transport,
            "remote transport enabled; require TLS, authentication, and network policy at a trusted proxy or boundary"
        );
    }
}

async fn serve_tcp_connection<S, F>(
    stream: TcpStream,
    factory: Arc<F>,
    max_message_bytes: usize,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
) where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    let Ok(service) = factory() else {
        mark_degraded(&lifecycle);
        return;
    };
    mark_recovered(&lifecycle);
    let (reader, writer) = stream.into_split();
    let transport = BoundedNdjsonTransport::new(reader, writer, max_message_bytes);
    match service.serve_with_ct(transport, cancellation).await {
        Ok(running) => {
            if running.waiting().await.is_err() {
                mark_degraded(&lifecycle);
            }
        }
        Err(_) => {
            tracing::warn!(
                transport_event = "initialization_failed",
                "TCP MCP initialization failed"
            );
        }
    }
}

async fn drain_tasks(tasks: &mut JoinSet<()>, grace: Duration) {
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    while !tasks.is_empty() {
        tokio::select! {
            () = &mut deadline => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return;
            }
            _ = tasks.join_next() => {}
        }
    }
}

/// Runs a non-standard raw TCP transport using one bounded NDJSON MCP frame
/// per line.
///
/// This compatibility transport has no built-in TLS or authentication. A
/// remote bind therefore requires explicit opt-in and a trusted external
/// security boundary.
///
/// # Errors
///
/// Returns an error for invalid configuration, lifecycle failure, listener
/// failure, or an accept-loop failure.
pub async fn run_tcp_ndjson<S, F>(
    factory: F,
    config: TcpTransportConfig,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<()>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    config.validate()?;
    transition(&lifecycle, LifecycleEvent::Start)?;
    let listener = match TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            finish_lifecycle(&lifecycle)?;
            return Err(error);
        }
    };
    warn_remote(config.bind, config.allow_remote, "tcp_ndjson_nonstandard");
    tracing::warn!(
        transport = "tcp_ndjson_nonstandard",
        "non-standard MCP compatibility transport enabled"
    );
    transition(&lifecycle, LifecycleEvent::Started)?;
    tracing::info!(
        transport = "tcp_ndjson_nonstandard",
        bind = %config.bind,
        "MCP transport ready"
    );

    let permits = Arc::new(Semaphore::new(config.max_connections));
    let factory = Arc::new(factory);
    let connection_cancellation = cancellation.child_token();
    let mut tasks = JoinSet::new();
    let mut live_error = None;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if completed.is_some_and(|result| result.is_err()) {
                    mark_degraded(&lifecycle);
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    mark_degraded(&lifecycle);
                    live_error = accepted.err();
                    break;
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let task_factory = Arc::clone(&factory);
                let task_lifecycle = lifecycle.clone();
                let task_cancellation = connection_cancellation.child_token();
                tasks.spawn(async move {
                    let _permit = permit;
                    serve_tcp_connection::<S, F>(
                        stream,
                        task_factory,
                        config.max_message_bytes,
                        task_lifecycle,
                        task_cancellation,
                    )
                    .await;
                });
            }
        }
    }

    transition(&lifecycle, LifecycleEvent::Drain)?;
    connection_cancellation.cancel();
    drain_tasks(&mut tasks, config.shutdown_grace).await;
    finish_lifecycle(&lifecycle)?;
    if let Some(error) = live_error {
        Err(error)
    } else {
        Ok(())
    }
}

struct WebSocketMcpTransport {
    reader: SplitStream<WebSocket>,
    writer: Arc<Mutex<Option<SplitSink<WebSocket, Message>>>>,
    max_message_bytes: usize,
}

impl WebSocketMcpTransport {
    fn new(socket: WebSocket, max_message_bytes: usize) -> Self {
        let (writer, reader) = socket.split();
        Self {
            reader,
            writer: Arc::new(Mutex::new(Some(writer))),
            max_message_bytes,
        }
    }
}

impl Transport<RoleServer> for WebSocketMcpTransport {
    type Error = WireTransportError;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);
        let max_message_bytes = self.max_message_bytes;
        async move {
            let encoded =
                serde_json::to_string(&item).map_err(|_| WireTransportError::Serialization)?;
            if encoded.len() > max_message_bytes {
                return Err(WireTransportError::MessageTooLarge);
            }
            writer
                .lock()
                .await
                .as_mut()
                .ok_or(WireTransportError::WebSocket)?
                .send(Message::Text(encoded.into()))
                .await
                .map_err(|_| WireTransportError::WebSocket)
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        loop {
            match self.reader.next().await? {
                Ok(Message::Text(text)) => {
                    if text.len() > self.max_message_bytes {
                        tracing::warn!(
                            transport_event = "frame_rejected",
                            "WebSocket frame rejected"
                        );
                        return None;
                    }
                    let Ok(message) = serde_json::from_str(text.as_str()) else {
                        tracing::warn!(
                            transport_event = "json_rejected",
                            "WebSocket JSON rejected"
                        );
                        return None;
                    };
                    return Some(message);
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(Message::Binary(_)) => {
                    tracing::warn!(
                        transport_event = "binary_rejected",
                        "WebSocket binary frame rejected"
                    );
                    return None;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        if let Some(mut writer) = self.writer.lock().await.take() {
            writer
                .send(Message::Close(None))
                .await
                .map_err(|_| WireTransportError::WebSocket)?;
            writer
                .close()
                .await
                .map_err(|_| WireTransportError::WebSocket)?;
        }
        Ok(())
    }
}

struct WebSocketState<F> {
    factory: Arc<F>,
    policy: HeaderPolicy,
    max_message_bytes: usize,
    permits: Arc<Semaphore>,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
}

async fn websocket_upgrade<S, F>(
    State(state): State<Arc<WebSocketState<F>>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    if state.cancellation.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if let Err(rejection) = state.policy.validate(&headers) {
        return rejection.response();
    }
    let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let max_message_bytes = state.max_message_bytes;
    upgrade
        .max_frame_size(max_message_bytes)
        .max_message_size(max_message_bytes)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let Ok(service) = (state.factory)() else {
                mark_degraded(&state.lifecycle);
                return;
            };
            mark_recovered(&state.lifecycle);
            let transport = WebSocketMcpTransport::new(socket, max_message_bytes);
            let Ok(running) = service
                .serve_with_ct(transport, state.cancellation.child_token())
                .await
            else {
                tracing::warn!(
                    transport_event = "initialization_failed",
                    "WebSocket MCP initialization failed"
                );
                return;
            };
            if running.waiting().await.is_err() {
                mark_degraded(&state.lifecycle);
            }
        })
        .into_response()
}

fn build_websocket_router<S, F>(
    factory: F,
    config: &WebSocketTransportConfig,
    lifecycle: &LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<Router>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    let state = Arc::new(WebSocketState {
        factory: Arc::new(factory),
        policy: HeaderPolicy::new(&config.allowed_hosts, &config.allowed_origins, true)?,
        max_message_bytes: config.max_message_bytes,
        permits: Arc::new(Semaphore::new(config.max_connections)),
        lifecycle: LifecycleController::clone(lifecycle),
        cancellation,
    });
    Ok(Router::new()
        .route(WEBSOCKET_PATH, get(websocket_upgrade::<S, F>))
        .with_state(state))
}

/// Runs a non-standard WebSocket compatibility transport at `/mcp/ws`.
///
/// Only bounded text messages are accepted. Every handshake requires exact
/// `Host` and `Origin` allowlist matches. The listener is plain HTTP; remote
/// use requires TLS and authentication at a trusted reverse proxy.
///
/// # Errors
///
/// Returns an error for invalid configuration, lifecycle failure, bind or
/// serving failure, or a graceful-shutdown timeout.
pub async fn run_websocket<S, F>(
    factory: F,
    config: WebSocketTransportConfig,
    lifecycle: LifecycleController,
    cancellation: CancellationToken,
) -> io::Result<()>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> io::Result<S> + Send + Sync + 'static,
{
    config.validate()?;
    transition(&lifecycle, LifecycleEvent::Start)?;
    let listener = match TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(error) => {
            finish_lifecycle(&lifecycle)?;
            return Err(error);
        }
    };
    warn_remote(config.bind, config.allow_remote, "websocket_nonstandard");
    tracing::warn!(
        transport = "websocket_nonstandard",
        "non-standard MCP compatibility transport enabled"
    );
    let app = match build_websocket_router::<S, F>(
        factory,
        &config,
        &lifecycle,
        cancellation.child_token(),
    ) {
        Ok(app) => app,
        Err(error) => {
            finish_lifecycle(&lifecycle)?;
            return Err(error);
        }
    };
    transition(&lifecycle, LifecycleEvent::Started)?;
    tracing::info!(
        transport = "websocket_nonstandard",
        bind = %config.bind,
        path = WEBSOCKET_PATH,
        "MCP transport ready"
    );

    let shutdown = cancellation.clone().cancelled_owned();
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .into_future();
    tokio::pin!(serving);
    let mut cancelled = false;
    let result = tokio::select! {
        result = &mut serving => result,
        () = cancellation.cancelled() => {
            cancelled = true;
            match transition(&lifecycle, LifecycleEvent::Drain) {
                Ok(()) => match tokio::time::timeout(config.shutdown_grace, &mut serving).await {
                    Ok(result) => result,
                    Err(_) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "WebSocket transport shutdown timed out",
                    )),
                },
                Err(error) => Err(error),
            }
        }
    };
    if result.is_err() && !cancelled {
        mark_degraded(&lifecycle);
    }
    finish_lifecycle(&lifecycle)?;
    result
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{
            HeaderValue, Method, Request,
            header::{ACCEPT, CONTENT_TYPE},
        },
    };
    use rmcp::{
        handler::server::ServerHandler,
        model::{Implementation, ServerCapabilities, ServerInfo},
        transport::Transport,
    };
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message as ClientWebSocketMessage, client::IntoClientRequest},
    };
    use tower::ServiceExt as _;

    use super::*;

    #[derive(Clone)]
    struct TestServer;

    impl ServerHandler for TestServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().build())
                .with_server_info(Implementation::new("transport-test", "1.0.0"))
        }
    }

    #[test]
    fn defaults_are_loopback_and_bounded() {
        let http = HttpTransportConfig::default();
        let tcp = TcpTransportConfig::default();
        let websocket = WebSocketTransportConfig::default();
        assert!(http.bind.ip().is_loopback());
        assert!(tcp.bind.ip().is_loopback());
        assert!(websocket.bind.ip().is_loopback());
        assert_eq!(http.max_body_bytes, MAX_MESSAGE_BYTES);
        assert_eq!(tcp.max_message_bytes, MAX_MESSAGE_BYTES);
        assert_eq!(websocket.max_message_bytes, MAX_MESSAGE_BYTES);
        http.validate().expect("valid HTTP defaults");
        tcp.validate().expect("valid TCP defaults");
        websocket.validate().expect("valid WebSocket defaults");
    }

    #[test]
    fn remote_bind_requires_explicit_opt_in() {
        let mut config = HttpTransportConfig::for_bind(SocketAddr::from(([0, 0, 0, 0], 3_000)));
        assert!(config.validate().is_err());
        config.allow_remote = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn connection_and_message_caps_are_fail_closed() {
        let zero_connections = TcpTransportConfig {
            max_connections: 0,
            ..TcpTransportConfig::default()
        };
        assert!(zero_connections.validate().is_err());
        let oversized = TcpTransportConfig {
            max_connections: 1,
            max_message_bytes: MAX_MESSAGE_BYTES + 1,
            ..TcpTransportConfig::default()
        };
        assert!(oversized.validate().is_err());

        let too_many_connections = WebSocketTransportConfig {
            max_connections: MAX_CONNECTIONS + 1,
            ..WebSocketTransportConfig::default()
        };
        assert!(too_many_connections.validate().is_err());
    }

    #[test]
    fn host_and_origin_are_exactly_allowlisted() {
        let policy = HeaderPolicy::new(
            &["localhost:3000".to_owned()],
            &["https://console.example:443".to_owned()],
            true,
        )
        .expect("valid policy");
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("localhost:3000"));
        headers.insert(ORIGIN, HeaderValue::from_static("https://console.example"));
        assert!(policy.validate(&headers).is_ok());

        headers.insert(HOST, HeaderValue::from_static("attacker.example:3000"));
        assert!(matches!(
            policy.validate(&headers),
            Err(HeaderRejection::Forbidden)
        ));
        headers.insert(HOST, HeaderValue::from_static("localhost:3000"));
        headers.insert(ORIGIN, HeaderValue::from_static("https://attacker.example"));
        assert!(matches!(
            policy.validate(&headers),
            Err(HeaderRejection::Forbidden)
        ));
    }

    #[test]
    fn websocket_requires_origin_but_http_allows_non_browser_client() {
        let hosts = vec!["localhost:3000".to_owned()];
        let origins = vec!["http://localhost:3000".to_owned()];
        let websocket = HeaderPolicy::new(&hosts, &origins, true).expect("valid policy");
        let http = HeaderPolicy::new(&hosts, &origins, false).expect("valid policy");
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("localhost:3000"));
        assert!(matches!(
            websocket.validate(&headers),
            Err(HeaderRejection::BadRequest)
        ));
        assert!(http.validate(&headers).is_ok());
    }

    #[test]
    fn origins_with_paths_or_queries_are_rejected() {
        assert!(parse_origin("https://example.com/path").is_none());
        assert!(parse_origin("https://example.com?query=yes").is_none());
        assert!(parse_origin("https://example.com").is_some());
    }

    #[tokio::test]
    async fn ndjson_transport_accepts_one_bounded_mcp_frame() {
        let (mut client, server) = tokio::io::duplex(4_096);
        let (reader, writer) = split(server);
        let mut transport = BoundedNdjsonTransport::new(reader, writer, 4_096);
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            }
        });
        client
            .write_all(format!("{initialize}\n").as_bytes())
            .await
            .expect("write frame");
        assert!(
            Transport::<RoleServer>::receive(&mut transport)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn ndjson_transport_rejects_oversize_before_serde() {
        let (mut client, server) = tokio::io::duplex(512);
        let (reader, writer) = split(server);
        let mut transport = BoundedNdjsonTransport::new(reader, writer, 64);
        let write = tokio::spawn(async move {
            let mut oversized = vec![b'{'; 65];
            oversized.push(b'\n');
            client.write_all(&oversized).await.expect("write frame");
        });
        assert!(
            Transport::<RoleServer>::receive(&mut transport)
                .await
                .is_none()
        );
        write.await.expect("writer task");
    }

    fn initialize_request(config: &HttpTransportConfig, body: Body) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(STREAMABLE_HTTP_PATH)
            .header(HOST, &config.allowed_hosts[0])
            .header(ORIGIN, &config.allowed_origins[0])
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .expect("request")
    }

    fn initialize_body() -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            }
        })
        .to_string()
    }

    fn legacy_initialize_body() -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            }
        })
        .to_string()
    }

    fn initialized_notification() -> String {
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
        .to_string()
    }

    fn unused_loopback_address() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve address");
        listener.local_addr().expect("local address")
    }

    async fn wait_until_ready(lifecycle: &LifecycleController) {
        lifecycle
            .wait_for(
                LifecycleState::Ready,
                Duration::from_secs(3),
                CancellationToken::new(),
            )
            .await
            .expect("transport ready");
    }

    #[tokio::test]
    async fn streamable_http_protocol_smoke() {
        let config = HttpTransportConfig::default();
        let lifecycle = LifecycleController::new(16).expect("lifecycle");
        let app = build_http_router::<TestServer, _>(
            || Ok(TestServer),
            &config,
            &lifecycle,
            CancellationToken::new(),
        )
        .expect("router");
        let response = app
            .oneshot(initialize_request(&config, Body::from(initialize_body())))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn streamable_http_rejects_oversize_body() {
        let config = HttpTransportConfig {
            max_body_bytes: 64,
            ..HttpTransportConfig::default()
        };
        let lifecycle = LifecycleController::new(16).expect("lifecycle");
        let app = build_http_router::<TestServer, _>(
            || Ok(TestServer),
            &config,
            &lifecycle,
            CancellationToken::new(),
        )
        .expect("router");
        let response = app
            .oneshot(initialize_request(&config, Body::from(initialize_body())))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn cancelled_http_runner_reaches_stopped() {
        let config = HttpTransportConfig::for_bind(SocketAddr::from(([127, 0, 0, 1], 0)));
        let lifecycle = LifecycleController::new(16).expect("lifecycle");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        run_streamable_http::<TestServer, _>(
            || Ok(TestServer),
            config,
            lifecycle.clone(),
            cancellation,
        )
        .await
        .expect("clean shutdown");
        assert_eq!(
            lifecycle.snapshot().expect("snapshot").state(),
            LifecycleState::Stopped
        );
    }

    #[tokio::test]
    async fn tcp_runner_protocol_and_lifecycle_smoke() {
        let address = unused_loopback_address();
        let config = TcpTransportConfig::for_bind(address);
        let lifecycle = LifecycleController::new(32).expect("lifecycle");
        let cancellation = CancellationToken::new();
        let runner_lifecycle = lifecycle.clone();
        let runner_cancellation = cancellation.clone();
        let runner = tokio::spawn(async move {
            run_tcp_ndjson::<TestServer, _>(
                || Ok(TestServer),
                config,
                runner_lifecycle,
                runner_cancellation,
            )
            .await
        });
        wait_until_ready(&lifecycle).await;

        let stream = TcpStream::connect(address).await.expect("connect TCP");
        let mut client = BufReader::new(stream);
        client
            .get_mut()
            .write_all(format!("{}\n", legacy_initialize_body()).as_bytes())
            .await
            .expect("send initialize");
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_line(&mut response))
            .await
            .expect("response timeout")
            .expect("read response");
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("JSON initialize response");
        assert_eq!(response["id"], 1);
        client
            .get_mut()
            .write_all(format!("{}\n", initialized_notification()).as_bytes())
            .await
            .expect("send initialized");

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .expect("runner shutdown timeout")
            .expect("runner task")
            .expect("runner result");
        assert_eq!(
            lifecycle.snapshot().expect("snapshot").state(),
            LifecycleState::Stopped
        );
    }

    #[tokio::test]
    async fn websocket_runner_text_protocol_and_lifecycle_smoke() {
        let address = unused_loopback_address();
        let config = WebSocketTransportConfig::for_bind(address);
        let origin = config.allowed_origins[0].clone();
        let lifecycle = LifecycleController::new(32).expect("lifecycle");
        let cancellation = CancellationToken::new();
        let runner_lifecycle = lifecycle.clone();
        let runner_cancellation = cancellation.clone();
        let runner = tokio::spawn(async move {
            run_websocket::<TestServer, _>(
                || Ok(TestServer),
                config,
                runner_lifecycle,
                runner_cancellation,
            )
            .await
        });
        wait_until_ready(&lifecycle).await;

        let mut request = format!("ws://{address}{WEBSOCKET_PATH}")
            .into_client_request()
            .expect("WebSocket request");
        request.headers_mut().insert(
            ORIGIN,
            HeaderValue::from_str(&origin).expect("Origin header"),
        );
        let (mut client, _) = connect_async(request).await.expect("connect WebSocket");
        client
            .send(ClientWebSocketMessage::Text(
                legacy_initialize_body().into(),
            ))
            .await
            .expect("send initialize");
        let response = tokio::time::timeout(Duration::from_secs(3), client.next())
            .await
            .expect("response timeout")
            .expect("response frame")
            .expect("valid frame");
        let ClientWebSocketMessage::Text(response) = response else {
            panic!("expected text response");
        };
        let response: serde_json::Value =
            serde_json::from_str(response.as_str()).expect("JSON initialize response");
        assert_eq!(response["id"], 1);
        client
            .send(ClientWebSocketMessage::Text(
                initialized_notification().into(),
            ))
            .await
            .expect("send initialized");

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .expect("runner shutdown timeout")
            .expect("runner task")
            .expect("runner result");
        assert_eq!(
            lifecycle.snapshot().expect("snapshot").state(),
            LifecycleState::Stopped
        );
    }
}
