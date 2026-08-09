//! Gateway-client socket protocol for the Telegram sidecar.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::MAX_GATEWAY_RESPONSE_BYTES;

/// Version of the private gateway socket protocol used by sidecars.
///
/// This stays at zero under `GATE-no-backward-compatibility`.
const SOCKET_PROTOCOL_VERSION: u32 = 0;
/// Maximum time one socket read may delay cancellation or reconfiguration.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum time one socket write may delay cancellation or reconfiguration.
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum time one connect attempt may run without supervisor cancellation.
const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

/// Gateway request failure with connection-retirement guidance.
#[derive(Debug)]
pub(crate) struct GatewayClientError {
    /// Sanitized operation diagnostic.
    message: String,
    /// Whether the socket can no longer retain valid lease authority.
    connection_fatal: bool,
}

impl GatewayClientError {
    /// Build a connection-fatal transport or protocol failure.
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            connection_fatal: true,
        }
    }

    /// Return whether callers must retire this connection.
    pub(crate) fn is_connection_fatal(&self) -> bool {
        self.connection_fatal
    }
}

impl std::fmt::Display for GatewayClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GatewayClientError {}

/// Configuration for the no-poll gateway-client sidecar mode.
#[derive(Clone, Debug)]
pub(crate) struct GatewayClientConfig {
    /// Private Unix socket path exposed by `tau-telegram-gateway`.
    pub(crate) socket_path: PathBuf,
}

/// Persistent client connection to the standalone Telegram gateway.
pub(crate) struct GatewayClient {
    /// Gateway socket path used for reconnects.
    socket_path: PathBuf,
    /// Shared socket stream; serialized because the protocol is
    /// request/response.
    stream: Mutex<Option<UnixStream>>,
    /// Current heartbeat interval requested by the gateway.
    heartbeat_interval: Mutex<Duration>,
    /// Last observed gateway generation.
    generation: Mutex<Option<String>>,
}

impl GatewayClient {
    /// Create a disconnected gateway client for the configured socket path.
    pub(crate) fn new(config: GatewayClientConfig) -> Self {
        Self {
            socket_path: config.socket_path,
            stream: Mutex::new(None),
            heartbeat_interval: Mutex::new(Duration::from_secs(10)),
            generation: Mutex::new(None),
        }
    }

    /// Return the current heartbeat interval advertised by the gateway.
    pub(crate) fn heartbeat_interval(&self) -> Duration {
        *self.heartbeat_interval.lock().expect("heartbeat lock")
    }

    /// Return the last generation reported by the gateway.
    pub(crate) fn generation(&self) -> Option<String> {
        self.generation.lock().expect("generation lock").clone()
    }

    /// Connect and send hello while checking supervisor cancellation.
    pub(crate) fn connect_cancellable(
        &self,
        cancelled: impl Fn() -> bool,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|error| {
                GatewayClientError::fatal(format!("creating Telegram gateway socket: {error}"))
            })?;
        let address = socket2::SockAddr::unix(&self.socket_path).map_err(|error| {
            GatewayClientError::fatal(format!("addressing Telegram gateway socket: {error}"))
        })?;
        if cancelled() {
            return Err(GatewayClientError::fatal(
                "Telegram gateway connect cancelled",
            ));
        }
        socket
            .connect_timeout(&address, SOCKET_CONNECT_TIMEOUT)
            .map_err(|error| {
                GatewayClientError::fatal(format!("connecting Telegram gateway socket: {error}"))
            })?;
        if cancelled() {
            return Err(GatewayClientError::fatal(
                "Telegram gateway connect cancelled",
            ));
        }
        let stream = UnixStream::from(OwnedFd::from(socket));
        stream
            .set_read_timeout(Some(SOCKET_READ_TIMEOUT))
            .map_err(|error| {
                GatewayClientError::fatal(format!(
                    "configuring Telegram gateway socket timeout: {error}"
                ))
            })?;
        stream
            .set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))
            .map_err(|error| {
                GatewayClientError::fatal(format!(
                    "configuring Telegram gateway socket timeout: {error}"
                ))
            })?;
        *self.stream.lock().expect("gateway stream lock") = Some(stream);
        let response = self.request(GatewayRequestKind::Hello, GatewayClientRequest::default())?;
        if response
            .gateway_generation
            .as_deref()
            .is_none_or(str::is_empty)
        {
            self.disconnect();
            return Err(GatewayClientError::fatal(
                "Telegram gateway hello omitted its generation",
            ));
        }
        *self.generation.lock().expect("generation lock") = response.gateway_generation.clone();
        Ok(response)
    }

    /// Send one heartbeat and drain queued gateway deliveries.
    pub(crate) fn heartbeat(&self) -> Result<GatewaySocketResponse, GatewayClientError> {
        self.request(
            GatewayRequestKind::Heartbeat,
            GatewayClientRequest::default(),
        )
    }

    /// Register one local agent route with the gateway.
    pub(crate) fn register_agent(
        &self,
        session_id: &str,
        agent_id: &str,
        display_name: Option<String>,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        self.request(
            GatewayRequestKind::RegisterAgent,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: None,
                display_name,
                report_id: None,
            },
        )
    }

    /// Unregister one local agent route from the gateway.
    pub(crate) fn unregister_agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        self.request(
            GatewayRequestKind::UnregisterAgent,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: None,
                display_name: None,
                report_id: None,
            },
        )
    }

    /// Send one outbound Telegram message through the gateway.
    pub(crate) fn send_message(
        &self,
        session_id: &str,
        agent_id: &str,
        message: &str,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        self.request(
            GatewayRequestKind::SendMessage,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: Some(message.to_owned()),
                display_name: None,
                report_id: None,
            },
        )
    }

    /// Confirm that one exact gateway delivery reached its canonical harness
    /// fact.
    pub(crate) fn acknowledge_delivery(
        &self,
        report_id: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        self.request(
            GatewayRequestKind::AcknowledgeDelivery,
            GatewayClientRequest {
                report_id: Some(report_id.to_owned()),
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                ..GatewayClientRequest::default()
            },
        )
    }

    /// Send a best-effort goodbye and drop the current connection.
    pub(crate) fn goodbye(&self) {
        let _ = self.request(GatewayRequestKind::Goodbye, GatewayClientRequest::default());
        self.disconnect();
    }

    /// Close the current socket without sending another protocol request.
    pub(crate) fn disconnect(&self) {
        *self.stream.lock().expect("gateway stream lock") = None;
    }

    /// Serialize one JSON-line gateway request and parse its response.
    fn request(
        &self,
        kind: GatewayRequestKind,
        request: GatewayClientRequest,
    ) -> Result<GatewaySocketResponse, GatewayClientError> {
        let mut guard = self.stream.lock().expect("gateway stream lock");
        let stream = guard
            .as_mut()
            .ok_or_else(|| GatewayClientError::fatal("Telegram gateway socket is not connected"))?;
        let request = GatewayWireRequest {
            protocol_version: SOCKET_PROTOCOL_VERSION,
            kind: kind.as_str(),
            session_id: request.session_id,
            agent_id: request.agent_id,
            message: request.message,
            display_name: request.display_name,
            report_id: request.report_id,
        };
        let request = serde_json::to_string(&request).map_err(|error| {
            GatewayClientError::fatal(format!("encoding Telegram gateway request: {error}"))
        })?;
        writeln!(stream, "{request}")
            .and_then(|()| stream.flush())
            .map_err(|error| {
                GatewayClientError::fatal(format!("writing Telegram gateway request: {error}"))
            })?;
        let mut line = String::new();
        BufReader::new(
            stream
                .try_clone()
                .map_err(|error| {
                    GatewayClientError::fatal(format!("cloning Telegram gateway socket: {error}"))
                })?
                .take(MAX_GATEWAY_RESPONSE_BYTES as u64 + 1),
        )
        .read_line(&mut line)
        .map_err(|error| {
            GatewayClientError::fatal(format!("reading Telegram gateway response: {error}"))
        })?;
        if line.len() > MAX_GATEWAY_RESPONSE_BYTES {
            return Err(GatewayClientError::fatal(
                "Telegram gateway response is too large",
            ));
        }
        if line.trim().is_empty() {
            return Err(GatewayClientError::fatal(
                "Telegram gateway closed the socket",
            ));
        }
        let response: GatewaySocketResponse = serde_json::from_str(&line).map_err(|error| {
            GatewayClientError::fatal(format!("decoding Telegram gateway response: {error}"))
        })?;
        if response.protocol_version != SOCKET_PROTOCOL_VERSION {
            return Err(GatewayClientError::fatal(format!(
                "unsupported Telegram gateway socket protocol version {}",
                response.protocol_version
            )));
        }
        if !response.ok {
            return Err(GatewayClientError {
                message: response
                    .error
                    .unwrap_or_else(|| "Telegram gateway request failed".to_owned()),
                connection_fatal: !response.keep_connection,
            });
        }
        if let Some(seconds) = response.heartbeat_interval_seconds {
            *self.heartbeat_interval.lock().expect("heartbeat lock") =
                Duration::from_secs(seconds.max(1));
        }
        Ok(response)
    }
}

/// Request kind sent to the gateway.
#[derive(Clone, Copy)]
enum GatewayRequestKind {
    /// Initial sidecar hello.
    Hello,
    /// Lease heartbeat.
    Heartbeat,
    /// Register one agent route.
    RegisterAgent,
    /// Unregister one agent route.
    UnregisterAgent,
    /// Send one Telegram message from a registered agent.
    SendMessage,
    /// Persist canonical acknowledgement for one inbound delivery.
    AcknowledgeDelivery,
    /// Close this sidecar connection.
    Goodbye,
}

impl GatewayRequestKind {
    /// Return the wire name for this request kind.
    fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Heartbeat => "heartbeat",
            Self::RegisterAgent => "register_agent",
            Self::UnregisterAgent => "unregister_agent",
            Self::SendMessage => "send_message",
            Self::AcknowledgeDelivery => "ack_delivery",
            Self::Goodbye => "goodbye",
        }
    }
}

/// High-level gateway request fields.
#[derive(Default)]
struct GatewayClientRequest {
    /// Optional Tau session id for route-bound operations.
    session_id: Option<String>,
    /// Optional Tau agent id for route-bound operations.
    agent_id: Option<String>,
    /// Optional outbound Telegram message body.
    message: Option<String>,
    /// Optional display name metadata.
    display_name: Option<String>,
    /// Opaque canonical report identity.
    report_id: Option<String>,
}

/// JSON-line request sent by the gateway-client sidecar.
#[derive(serde::Serialize)]
struct GatewayWireRequest<'a> {
    /// Local socket protocol version.
    protocol_version: u32,
    /// Request kind understood by the gateway.
    kind: &'a str,
    /// Optional Tau session id for route-bound operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// Optional Tau agent id for route-bound operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    /// Optional outbound Telegram message body.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Optional display name metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    /// Opaque canonical report identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    report_id: Option<String>,
}

/// JSON-line response returned by the gateway socket.
#[derive(serde::Deserialize)]
pub(crate) struct GatewaySocketResponse {
    /// Local socket protocol version.
    protocol_version: u32,
    /// Whether the gateway accepted the request.
    #[serde(default)]
    pub(crate) ok: bool,
    /// Optional error detail for rejected requests.
    error: Option<String>,
    /// Optional requested heartbeat interval.
    heartbeat_interval_seconds: Option<u64>,
    /// Optional gateway process generation.
    pub(crate) gateway_generation: Option<String>,
    /// Whether the gateway asks clients to reannounce registrations.
    #[serde(default)]
    pub(crate) reannounce_required: bool,
    /// Whether an ordinary rejected operation leaves this connection valid.
    #[serde(default)]
    keep_connection: bool,
    /// Queued inbound delivery records for this sidecar.
    #[serde(default)]
    pub(crate) deliveries: Vec<GatewayMessageDelivery>,
}

/// Queued inbound delivery record received from the gateway.
#[derive(serde::Deserialize)]
pub(crate) struct GatewayMessageDelivery {
    /// Gateway request correlation id.
    pub(crate) request_id: String,
    /// Target Tau session id.
    pub(crate) session_id: String,
    /// Target Tau agent id.
    pub(crate) agent_id: String,
    /// Publisher-scoped Telegram message identity.
    pub(crate) message_id: String,
    /// Stable numeric Telegram sender id.
    pub(crate) sender_id: String,
    /// Sanitized Telegram source label.
    pub(crate) source: String,
    /// Stable numeric Telegram conversation id.
    pub(crate) conversation_id: String,
    /// Original Telegram message body without a transport prefix.
    pub(crate) text: String,
}

#[cfg(test)]
mod tests;
