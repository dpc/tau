//! Gateway-client socket protocol for the Telegram sidecar.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::MAX_GATEWAY_RESPONSE_BYTES;

/// Version of the private gateway socket protocol used by sidecars.
///
/// This stays at zero under `GATE-no-backward-compatibility`.
const SOCKET_PROTOCOL_VERSION: u32 = 0;

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

    /// Send `hello`, replacing any previous connection.
    pub(crate) fn connect(&self) -> Result<GatewaySocketResponse, String> {
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|error| format!("connecting Telegram gateway socket: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(35)))
            .map_err(|error| format!("configuring Telegram gateway socket timeout: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("configuring Telegram gateway socket timeout: {error}"))?;
        *self.stream.lock().expect("gateway stream lock") = Some(stream);
        self.request(GatewayRequestKind::Hello, GatewayClientRequest::default())
    }

    /// Send one heartbeat and drain queued gateway deliveries.
    pub(crate) fn heartbeat(&self) -> Result<GatewaySocketResponse, String> {
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
    ) -> Result<GatewaySocketResponse, String> {
        self.request(
            GatewayRequestKind::RegisterAgent,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: None,
                display_name,
            },
        )
    }

    /// Unregister one local agent route from the gateway.
    pub(crate) fn unregister_agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<GatewaySocketResponse, String> {
        self.request(
            GatewayRequestKind::UnregisterAgent,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: None,
                display_name: None,
            },
        )
    }

    /// Send one outbound Telegram message through the gateway.
    pub(crate) fn send_message(
        &self,
        session_id: &str,
        agent_id: &str,
        message: &str,
    ) -> Result<GatewaySocketResponse, String> {
        self.request(
            GatewayRequestKind::SendMessage,
            GatewayClientRequest {
                session_id: Some(session_id.to_owned()),
                agent_id: Some(agent_id.to_owned()),
                message: Some(message.to_owned()),
                display_name: None,
            },
        )
    }

    /// Send a best-effort goodbye and drop the current connection.
    pub(crate) fn goodbye(&self) {
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = self.request(GatewayRequestKind::Goodbye, GatewayClientRequest::default());
        *self.stream.lock().expect("gateway stream lock") = None;
    }

    /// Serialize one JSON-line gateway request and parse its response.
    fn request(
        &self,
        kind: GatewayRequestKind,
        request: GatewayClientRequest,
    ) -> Result<GatewaySocketResponse, String> {
        let mut guard = self.stream.lock().expect("gateway stream lock");
        let stream = guard
            .as_mut()
            .ok_or_else(|| "Telegram gateway socket is not connected".to_owned())?;
        let request = GatewayWireRequest {
            protocol_version: SOCKET_PROTOCOL_VERSION,
            kind: kind.as_str(),
            session_id: request.session_id,
            agent_id: request.agent_id,
            message: request.message,
            display_name: request.display_name,
        };
        let request = serde_json::to_string(&request)
            .map_err(|error| format!("encoding Telegram gateway request: {error}"))?;
        writeln!(stream, "{request}")
            .and_then(|()| stream.flush())
            .map_err(|error| format!("writing Telegram gateway request: {error}"))?;
        let mut line = String::new();
        BufReader::new(
            stream
                .try_clone()
                .map_err(|error| format!("cloning Telegram gateway socket: {error}"))?
                .take(MAX_GATEWAY_RESPONSE_BYTES as u64 + 1),
        )
        .read_line(&mut line)
        .map_err(|error| format!("reading Telegram gateway response: {error}"))?;
        if line.len() > MAX_GATEWAY_RESPONSE_BYTES {
            return Err("Telegram gateway response is too large".to_owned());
        }
        if line.trim().is_empty() {
            return Err("Telegram gateway closed the socket".to_owned());
        }
        let response: GatewaySocketResponse = serde_json::from_str(&line)
            .map_err(|error| format!("decoding Telegram gateway response: {error}"))?;
        if response.protocol_version != SOCKET_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported Telegram gateway socket protocol version {}",
                response.protocol_version
            ));
        }
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "Telegram gateway request failed".to_owned()));
        }
        if let Some(seconds) = response.heartbeat_interval_seconds {
            *self.heartbeat_interval.lock().expect("heartbeat lock") =
                Duration::from_secs(seconds.max(1));
        }
        let mut generation = self.generation.lock().expect("generation lock");
        if let Some(new_generation) = &response.gateway_generation {
            *generation = Some(new_generation.clone());
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
            Self::Goodbye => "goodbye",
        }
    }
}

/// High-level gateway request fields.
#[derive(Default)]
struct GatewayClientRequest {
    /// Optional Tau session id.
    session_id: Option<String>,
    /// Optional Tau agent id.
    agent_id: Option<String>,
    /// Optional outbound Telegram message body.
    message: Option<String>,
    /// Optional display name metadata.
    display_name: Option<String>,
}

/// JSON-line request sent by the gateway-client sidecar.
#[derive(serde::Serialize)]
struct GatewayWireRequest<'a> {
    /// Local socket protocol version.
    protocol_version: u32,
    /// Request kind understood by the gateway.
    kind: &'a str,
    /// Optional Tau session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    /// Optional Tau agent id.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    /// Optional outbound Telegram message body.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Optional display name metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
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
    gateway_generation: Option<String>,
    /// Whether the gateway asks clients to reannounce registrations.
    #[serde(default)]
    pub(crate) reannounce_required: bool,
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
