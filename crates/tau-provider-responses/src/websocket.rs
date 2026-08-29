//! Public Responses WebSocket transport for one finite full-replay attempt.

use std::future::Future;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};

use super::decoded_event::DecodedEvent;
use super::{
    AttemptConfig, AttemptModel, AttemptProgress, AttemptUpdate, CANCELLATION_POLL_INTERVAL,
    DebugCapture, Error, MAX_EVENT_BYTES, MAX_RESPONSE_BYTES, RequestBody, State, deadlines,
    read_capped_error_body,
};

/// One semantically decoded WebSocket event after legacy error precedence.
enum DecodedWebSocketEvent<'a> {
    /// Event ready for exact-sidecar indexing and assembly.
    Apply(DecodedEvent<'a>),
    /// Provider terminal classified before sidecar indexing.
    ProviderError { value: Value, error: Error },
}

fn decode_websocket_event(raw: &str) -> Result<DecodedWebSocketEvent<'_>, Error> {
    let value = DecodedEvent::decode_value(raw).map_err(|_| Error::Json)?;
    if let Some(error) = provider_terminal_error(&value) {
        return Ok(DecodedWebSocketEvent::ProviderError { value, error });
    }
    DecodedEvent::from_value(raw, value)
        .map(DecodedWebSocketEvent::Apply)
        .map_err(|_| Error::Json)
}

/// Runs one fresh-socket WebSocket attempt.
///
/// Every logical retry reconnects and sends the full local transcript again.
/// Tau intentionally does not chain `previous_response_id`: an identifier from
/// a failed or closed `store=false` connection is not a valid continuation
/// authority, and compatible endpoints need not implement OpenAI's
/// connection-local cache. The selected transport never falls back to SSE.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream(
    prompt: &tau_proto::AgentPromptCreated,
    config: &AttemptConfig,
    model: &AttemptModel,
    body: &RequestBody,
    debug_capture: DebugCapture,
    on_update: &mut impl FnMut(AttemptUpdate),
    is_canceled: &mut impl FnMut() -> bool,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<State, (Error, AttemptProgress)> {
    let websocket_url =
        websocket_url(&config.base_url).map_err(|error| (error, State::default().progress()))?;
    let http_url = websocket_http_url(&websocket_url);
    let mut handshake = websocket_url
        .as_str()
        .into_client_request()
        .map_err(|_| (Error::InvalidRequest, State::default().progress()))?;
    if !config.api_key.trim().is_empty() {
        let value = format!("Bearer {}", config.api_key)
            .parse()
            .map_err(|_| (Error::InvalidRequest, State::default().progress()))?;
        handshake.headers_mut().insert("authorization", value);
    }
    let key = handshake
        .headers()
        .get("sec-websocket-key")
        .cloned()
        .ok_or_else(|| (Error::InvalidRequest, State::default().progress()))?;
    let client = network
        .client_for(&websocket_url)
        .map_err(|error| (Error::Outbound(error), State::default().progress()))?;
    let mut outbound = client.get(&http_url).version(reqwest::Version::HTTP_11);
    for (name, value) in handshake.headers() {
        outbound = outbound.header(name, value);
    }
    let deadline = Instant::now() + deadlines::REQUEST_CONNECT_HEADER_TIMEOUT;
    let mut send = Box::pin(outbound.send());
    let response = loop {
        tokio::select! {
            result = &mut send => break result.map_err(|error| {
                (Error::Outbound(network.reqwest_error(
                    &websocket_url, tau_provider::OutboundPhase::Request, &error,
                )), State::default().progress())
            })?,
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err((Error::StreamFailure, State::default().progress()));
            }
            () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                if is_canceled() {
                    return Err((Error::Canceled, State::default().progress()));
                }
                if deadline <= Instant::now() {
                    return Err((Error::StreamFailure, State::default().progress()));
                }
            }
        }
    };
    if deadline <= Instant::now() {
        return Err((Error::StreamFailure, State::default().progress()));
    }
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status().as_u16();
        if let Some(error) = network.proxy_response_error(&websocket_url, status) {
            return Err((Error::Outbound(error), State::default().progress()));
        }
        let body = read_capped_error_body(
            response,
            &websocket_url,
            is_canceled,
            network,
            deadlines::StreamDeadlines::new(Instant::now()),
        )
        .await
        .map_err(|error| (error, State::default().progress()))?;
        return Err((Error::Http(status, body), State::default().progress()));
    }
    validate_websocket_upgrade(&response, &key, network, &websocket_url)
        .map_err(|error| (error, State::default().progress()))?;
    let mut deadlines = deadlines::StreamDeadlines::new(Instant::now());
    let upgraded = match await_with_deadline(response.upgrade(), deadline, is_canceled).await {
        Ok(Ok(upgraded)) => upgraded,
        Ok(Err(_)) | Err(WaitError::Deadline) => {
            return Err((
                Error::Outbound(
                    network.protocol_error(&websocket_url, tau_provider::OutboundPhase::Request),
                ),
                State::default().progress(),
            ));
        }
        Err(WaitError::Canceled) => {
            return Err((Error::Canceled, State::default().progress()));
        }
    };
    let mut socket = configured_websocket_stream(upgraded).await;
    let mut envelope =
        serde_json::to_value(body).map_err(|_| (Error::Json, State::default().progress()))?;
    let object = envelope
        .as_object_mut()
        .ok_or_else(|| (Error::Json, State::default().progress()))?;
    object.remove("stream");
    object.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    let serialized =
        serde_json::to_string(&envelope).map_err(|_| (Error::Json, State::default().progress()))?;
    if is_canceled() {
        return Err((Error::Canceled, State::default().progress()));
    }
    debug_capture.submit_wire_request(prompt, config, model, &envelope);
    if is_canceled() {
        return Err((Error::Canceled, State::default().progress()));
    }
    on_update(AttemptUpdate::Dispatched(Instant::now()));
    send_bounded(
        &mut socket,
        Message::Text(serialized.into()),
        is_canceled,
        (Instant::now() + deadlines::REQUEST_CONNECT_HEADER_TIMEOUT).min(deadlines.next_deadline()),
        &State::default(),
    )
    .await?;

    let mut state = State {
        debug_capture,
        ..Default::default()
    };
    loop {
        if is_canceled() {
            return Err((Error::Canceled, state.progress()));
        }
        let deadline = deadlines.next_deadline();
        if deadlines.expired(Instant::now()) {
            return Err((Error::StreamFailure, state.progress()));
        }
        let frame = match await_with_deadline(socket.next(), deadline, is_canceled).await {
            Ok(frame) => frame,
            Err(WaitError::Canceled) => return Err((Error::Canceled, state.progress())),
            Err(WaitError::Deadline) => return Err((Error::StreamFailure, state.progress())),
        };
        if deadlines.expired(Instant::now()) {
            return Err((Error::StreamFailure, state.progress()));
        }
        match frame {
            Some(Ok(Message::Text(text))) => {
                let Some(bytes) = checked_response_bytes(state.bytes, text.len()) else {
                    return Err((Error::StreamFailure, state.progress()));
                };
                state.bytes = bytes;
                let decoded = match decode_websocket_event(text.as_ref())
                    .map_err(|error| (error, state.progress()))?
                {
                    DecodedWebSocketEvent::Apply(decoded) => decoded,
                    DecodedWebSocketEvent::ProviderError { value, error } => {
                        state.debug_capture.record_event(&value, text.as_ref());
                        return Err((error, state.progress()));
                    }
                };
                let qualifying_progress = state
                    .apply_decoded_event(&decoded, text.as_ref())
                    .map_err(|error| (error, state.progress()))?;
                on_update(AttemptUpdate::Progress(state.progress()));
                if qualifying_progress {
                    deadlines.renew_for_qualifying_progress(Instant::now());
                }
                if state.terminal.is_some() {
                    return Ok(state);
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                send_bounded(
                    &mut socket,
                    Message::Pong(payload),
                    is_canceled,
                    deadlines.next_deadline(),
                    &state,
                )
                .await?;
            }
            Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Binary(_))) | Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                return Err((Error::StreamFailure, state.progress()));
            }
        }
    }
}

/// Bounds frame allocation and fragmented-message assembly at tungstenite's
/// transport seam while retaining the application check as defense in depth.
fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(MAX_EVENT_BYTES))
        .max_message_size(Some(MAX_EVENT_BYTES))
}

/// Constructs the public Responses client socket with transport-level ingress
/// limits applied before tungstenite reads provider-controlled payloads.
async fn configured_websocket_stream<S>(stream: S) -> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    WebSocketStream::from_raw_socket(stream, Role::Client, Some(websocket_config())).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitError {
    Canceled,
    Deadline,
}

async fn await_with_deadline<T>(
    future: impl Future<Output = T>,
    deadline: Instant,
    is_canceled: &mut impl FnMut() -> bool,
) -> Result<T, WaitError> {
    if is_canceled() {
        return Err(WaitError::Canceled);
    }
    if deadline <= Instant::now() {
        return Err(WaitError::Deadline);
    }
    let mut future = Box::pin(future);
    loop {
        tokio::select! {
            result = &mut future => {
                if deadline <= Instant::now() {
                    return Err(WaitError::Deadline);
                }
                return Ok(result);
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(WaitError::Deadline);
            }
            () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                if is_canceled() {
                    return Err(WaitError::Canceled);
                }
            }
        }
    }
}

fn checked_response_bytes(current: u64, event: usize) -> Option<u64> {
    if MAX_EVENT_BYTES < event {
        return None;
    }
    let total = current.saturating_add(event as u64);
    (total <= MAX_RESPONSE_BYTES).then_some(total)
}

async fn send_bounded(
    socket: &mut WebSocketStream<reqwest::Upgraded>,
    message: Message,
    is_canceled: &mut impl FnMut() -> bool,
    deadline: Instant,
    state: &State,
) -> Result<(), (Error, AttemptProgress)> {
    let mut send = Box::pin(socket.send(message));
    loop {
        tokio::select! {
            result = &mut send => {
                if deadline <= Instant::now() {
                    return Err((Error::StreamFailure, state.progress()));
                }
                return result.map_err(|_| (Error::StreamFailure, state.progress()));
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err((Error::StreamFailure, state.progress()));
            }
            () = tokio::time::sleep(CANCELLATION_POLL_INTERVAL) => {
                if is_canceled() {
                    return Err((Error::Canceled, state.progress()));
                }
                if deadline <= Instant::now() {
                    return Err((Error::StreamFailure, state.progress()));
                }
            }
        }
    }
}

fn provider_terminal_error(event: &Value) -> Option<Error> {
    let ty = event.get("type").and_then(Value::as_str)?;
    if !matches!(ty, "error" | "response.failed" | "response.incomplete") {
        return None;
    }
    if ty == "response.incomplete" && super::incomplete_reason(event) == Some("max_output_tokens") {
        return None;
    }
    let status = event
        .get("status")
        .or_else(|| event.pointer("/error/status"))
        .or_else(|| event.pointer("/response/status"))
        .or_else(|| event.pointer("/response/error/status"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let code = [
        "/code",
        "/error/code",
        "/error/type",
        "/response/error/code",
        "/response/error/type",
        "/response/incomplete_details/reason",
    ]
    .into_iter()
    .find_map(|path| event.pointer(path).and_then(Value::as_str))
    .map(|code| code.chars().take(128).collect());
    Some(Error::Provider { status, code })
}

fn websocket_url(base_url: &str) -> Result<String, Error> {
    let base = base_url.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        Ok(format!("wss://{rest}/responses"))
    } else if let Some(rest) = base.strip_prefix("http://") {
        Ok(format!("ws://{rest}/responses"))
    } else {
        Err(Error::InvalidRequest)
    }
}

fn websocket_http_url(websocket_url: &str) -> String {
    websocket_url
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            websocket_url
                .strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .expect("validated WebSocket URL")
}

fn validate_websocket_upgrade(
    response: &reqwest::Response,
    key: &reqwest::header::HeaderValue,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
) -> Result<(), Error> {
    let headers = response.headers();
    if websocket_headers_valid(headers, key) {
        Ok(())
    } else {
        Err(Error::Outbound(
            network.protocol_error(url, tau_provider::OutboundPhase::Request),
        ))
    }
}

fn websocket_headers_valid(
    headers: &reqwest::header::HeaderMap,
    key: &reqwest::header::HeaderValue,
) -> bool {
    let upgrade_ok = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let connection_ok = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    let expected = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let accept_ok = headers
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected);
    let negotiation_absent = !headers.contains_key("sec-websocket-extensions")
        && !headers.contains_key("sec-websocket-protocol");
    upgrade_ok && connection_ok && accept_ok && negotiation_absent
}

#[cfg(test)]
mod tests;
