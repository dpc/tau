//! Persistent-WebSocket transport for the Codex Responses API.
//!
//! The provider owns a small pool of these connections, keyed by the
//! upstream thread UUID derived from the prompt-cache key, so the
//! connection-local `previous_response_id` cache stays warm across turns of
//! the same conversation. The pool itself lives in [`super::pool`]; this
//! module handles a single connection's lifecycle and one-turn
//! streaming.
//!
//! Wire shape:
//! - Upgrade `wss://{base_url}/codex/responses` (same path as the HTTP+SSE
//!   endpoint, just `wss://`) with `Authorization`, `chatgpt-account-id`,
//!   `session-id`, `thread-id`, and the dated `OpenAI-Beta:
//!   responses_websockets=2026-02-06` header.
//! - Send one client text frame per turn: a `{ "type": "response.create", ...
//!   }` envelope produced by `super::build_ws_envelope`.
//! - Read server text frames as one decoded `response.*` event each and hand
//!   them to [`super::apply_event`]. Same event shape as SSE (the WS guide is
//!   explicit on this).
//! - On `response.completed`/`response.done` the connection stays open and idle
//!   for the next turn.
//!
//! Threading model: each connection has two tokio tasks behind the
//! scenes — a reader looping on `stream.next()` and a writer
//! draining an outbound channel + driving a periodic client-side
//! ping. The pings keep the upstream's keepalive timer happy
//! (default 25 s; the live Codex server reaps with a 1011
//! "keepalive ping timeout" close when no client pong has been seen
//! recently). The sync [`WsConn`] type holds the channel handles —
//! `run_turn` is sync, owned by the provider's main loop, and just
//! marshals envelopes to the writer task and pulls events back from
//! the reader.

use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};

use super::{
    DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT, ProviderRawEventStream, ResponsesConfig,
    apply_raw_json_event, build_ws_envelope, load_provider_stream_cassette,
    record_provider_raw_event_after, stream_idle_timeout_error,
};
use crate::common::{LlmError, PromptPayload, StreamState};
use crate::responses::ws_runtime;
use crate::{NeverAbort, TurnAbort};

/// Beta-feature header value the OpenAI WebSocket endpoint expects.
/// Dated by the server; will need a bump when OpenAI rolls a new
/// release. Pinned here as a single `const` so that bump is a
/// one-line change.
pub const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

/// How often the writer task sends an unsolicited client `Ping`.
///
/// The live Codex server reaps idle connections with a 1011
/// "keepalive ping timeout" close when its own ping cycle goes
/// pong-less. The empirical window between turn N completing and a
/// reap-triggered drop on turn N+1 is ~90 s (one ping interval plus
/// two missed-pong slots). 25 s keeps us comfortably inside that
/// budget *and* under common LB / NAT idle timeouts (60 s default
/// on AWS ALB, nginx, etc.) which would otherwise hang up the TCP
/// connection mid-idle.
///
/// Doubles as a flush trigger: `tungstenite` queues an outgoing
/// `Pong` whenever the reader half processes a server `Ping`, but
/// the queued bytes only leave the wire on the next sink write.
/// Periodic client pings ensure pongs don't sit buffered for a full
/// turn boundary.
const KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(25);

/// How long one WS turn may go without any provider event before Tau treats
/// the socket as wedged and returns a retryable WebSocket error to the caller.
const TURN_EVENT_TIMEOUT: Duration = DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT;

type SharedStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<SharedStream, Message>;
type Stream = SplitStream<SharedStream>;

/// Commands sent from sync land into a connection's writer task.
enum WsCommand {
    /// Send a text frame on the wire — used for `response.create`
    /// turn envelopes.
    SendText(String),
}

/// Events surfaced from the reader task to sync land.
///
/// The reader pre-parses text frames as JSON before forwarding so
/// the sync caller doesn't do JSON work on the runtime side, and so
/// unparseable frames can be quietly dropped at the source (same
/// behavior as the SSE line-level resync).
enum InboundEvent {
    /// One parsed `response.*` event and its upstream text frame.
    Event { text: Utf8Bytes },
    /// Server sent a `Close` frame (or the stream ended cleanly
    /// without one). The string is the close-frame reason for
    /// logging.
    Closed(String),
    /// Transport / protocol error mid-stream.
    Error(String),
    /// Harness cancellation state changed; the sync turn loop should re-check
    /// its abort source immediately.
    AbortWake,
}

/// One live WS connection to a Responses endpoint, as seen from the
/// provider's sync main loop.
///
/// The actual socket and `tungstenite` state machine live in two
/// tokio tasks (reader + writer) spawned at [`Self::connect`] time.
/// This struct just holds the channel ends and the abort handles —
/// `run_turn` is a thin sync wrapper that pushes the envelope onto
/// the outbound channel and pulls events off the inbound one.
pub struct WsConn {
    outbound_tx: UnboundedSender<WsCommand>,
    inbound_tx: std_mpsc::Sender<InboundEvent>,
    inbound_rx: std_mpsc::Receiver<InboundEvent>,
    /// Aborted on [`Drop`] so a `WsConn` falling out of scope cleanly
    /// tears down its background tasks. Cooperative cancellation via
    /// channel close would also work but adds latency on the path
    /// where we already know we want both tasks gone (the
    /// `is_recoverable_ws_error` retry path drops the conn
    /// immediately).
    reader_abort: AbortHandle,
    writer_abort: AbortHandle,
    /// Wall-clock time of the upgrade. Used by the pool to retire
    /// connections before the server's 60-minute hard cap fires
    /// mid-turn.
    pub opened_at: Instant,
    /// Bearer token the upgrade was authenticated with. The pool
    /// compares against the current resolved token on checkout — a
    /// mismatch means OAuth refreshed and this socket's auth is
    /// stale, so it gets dropped and reopened.
    pub bearer: String,
    /// Cached response id returned on this live socket. It is only sent as
    /// `previous_response_id` if request building finds it in the prompt
    /// context.
    cached_response_id: Option<String>,
}

pub(super) struct WsTurnResult {
    pub state: StreamState,
    pub request_body: Option<serde_json::Value>,
}

pub(super) fn recorded_request_body(
    envelope: &impl serde::Serialize,
    recording: bool,
) -> Result<Option<serde_json::Value>, LlmError> {
    if !recording {
        return Ok(None);
    }
    let mut request_body = serde_json::to_value(envelope).map_err(LlmError::Json)?;
    super::redact_image_data_urls(&mut request_body);
    Ok(Some(request_body))
}

impl WsConn {
    /// Open a fresh connection and perform the WS upgrade. Spawns
    /// the reader and writer tasks on the shared runtime so the
    /// connection is immediately ready to serve a turn — and
    /// already auto-pongs any server-initiated `Ping` even before
    /// the first `run_turn` call.
    ///
    /// `thread_id` is the prompt-cache UUID for the request bucket; it is sent
    /// as both `session-id` and `thread-id` on the upgrade.
    ///
    /// Errors:
    /// - `LlmError::HttpStatus(426, _)` — server rejected the upgrade.
    /// - `LlmError::HttpStatus(0, "stream error: ...")` — transient transport
    ///   hiccup, retryable.
    /// - Other 4xx — surface as-is.
    pub fn connect(config: &ResponsesConfig, thread_id: &str) -> Result<Self, LlmError> {
        let request = build_request(config, thread_id)?;
        let bearer = config.api_key.clone();
        let runtime = ws_runtime::handle();
        let (ws, _response) = runtime
            .block_on(async { tokio_tungstenite::connect_async(request).await })
            .map_err(map_ws_connect_error)?;

        let (sink, stream) = ws.split();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = std_mpsc::channel();
        // Both tasks share the inbound channel: the reader's
        // primary job is feeding events, but the writer also
        // surfaces send-side failures there so `run_turn` never
        // waits forever after a half-open socket
        // (write fails, read still pending). The writer signals
        // first when writes break; the reader's eventual close
        // event just stacks behind it in the buffer.
        let reader_abort = runtime
            .spawn(read_loop(stream, inbound_tx.clone()))
            .abort_handle();
        let writer_abort = runtime
            .spawn(write_loop(
                sink,
                outbound_rx,
                inbound_tx.clone(),
                KEEPALIVE_PING_INTERVAL,
            ))
            .abort_handle();

        Ok(Self {
            outbound_tx,
            inbound_tx,
            inbound_rx,
            reader_abort,
            writer_abort,
            opened_at: Instant::now(),
            bearer,
            cached_response_id: None,
        })
    }

    /// Send one `response.create` envelope and stream events back
    /// until `response.completed` / `response.done`. Returns the
    /// accumulated [`StreamState`]; leaves the socket open for the
    /// next turn.
    ///
    /// Mid-stream WS close or IO error surfaces as a retryable
    /// `LlmError` (code 0, body prefixed with `"stream error:"`) so
    /// the provider's outer retry loop reopens on the next attempt.
    pub(super) fn run_turn(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<WsTurnResult, LlmError> {
        let cached_response_id = self.cached_response_id.as_deref();
        let envelope = build_ws_envelope(config, request, cached_response_id, None);
        let request_body = recorded_request_body(&envelope, recording_stream.is_some())?;
        super::maybe_debug_write_provider_request(
            agent_prompt_id,
            config,
            request,
            tau_proto::ProviderBackendTransport::Websocket,
            &envelope,
        );
        let state = self.run_envelope(
            agent_prompt_id,
            envelope,
            recording_stream,
            abort,
            on_update,
        )?;
        self.cached_response_id = state.response_id.clone();
        Ok(WsTurnResult {
            state,
            request_body,
        })
    }

    /// Send one non-generating prewarm envelope and wait for the
    /// provider to finish accepting it. No Tau-visible response
    /// events are emitted by this layer's caller.
    pub fn run_prewarm(
        &mut self,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
    ) -> Result<StreamState, LlmError> {
        let envelope = build_ws_envelope(config, request, None, Some(false));
        let mut abort = NeverAbort;
        self.run_envelope("<prewarm>", envelope, None, &mut abort, &mut |_| {})
    }

    fn run_envelope(
        &mut self,
        agent_prompt_id: &str,
        envelope: super::WsResponseCreate,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<StreamState, LlmError> {
        self.run_envelope_with_idle_timeout(
            agent_prompt_id,
            envelope,
            recording_stream,
            abort,
            TURN_EVENT_TIMEOUT,
            on_update,
        )
    }

    fn run_envelope_with_idle_timeout(
        &mut self,
        agent_prompt_id: &str,
        envelope: super::WsResponseCreate,
        mut recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        idle_timeout: Duration,
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<StreamState, LlmError> {
        let text = serde_json::to_string(&envelope).map_err(LlmError::Json)?;
        self.outbound_tx
            .send(WsCommand::SendText(text))
            .map_err(|_| LlmError::HttpStatus(0, "stream error: ws writer task gone".to_owned()))?;

        let mut state = StreamState::new();
        let turn_started_at = Instant::now();
        let mut last_event_at = Instant::now();
        let _abort_waker = {
            let inbound_tx = self.inbound_tx.clone();
            abort.register_waker(Arc::new(move || {
                let _ = inbound_tx.send(InboundEvent::AbortWake);
            }))
        };
        loop {
            if abort.is_aborted() {
                return Err(LlmError::Canceled);
            }
            let remaining = idle_timeout.saturating_sub(last_event_at.elapsed());
            let wait = remaining.min(Duration::from_secs(1));
            let event = match self.inbound_rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(std_mpsc::RecvTimeoutError::Timeout)
                    if last_event_at.elapsed() < idle_timeout =>
                {
                    // Provider-owned response liveness is deadline-driven, not
                    // upstream-event-driven. Wake the outer sampled emitter
                    // during quiet WebSocket waits; it enforces the 1Hz cadence.
                    on_update(&state);
                    continue;
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    return Err(stream_idle_timeout_error(
                        tau_proto::ProviderBackendTransport::Websocket,
                        agent_prompt_id,
                        turn_started_at,
                        last_event_at,
                        idle_timeout,
                        &state,
                        None,
                    ));
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LlmError::HttpStatus(
                        0,
                        "stream error: ws reader task gone".to_owned(),
                    ));
                }
            };
            match event {
                InboundEvent::Event { text } => {
                    let now = Instant::now();
                    let delta = now.saturating_duration_since(last_event_at);
                    last_event_at = now;
                    state.record_transport_response_bytes(text.len());
                    on_update(&state);
                    if let Some(stream) = recording_stream.as_deref_mut() {
                        record_provider_raw_event_after(stream, delta, text.to_string());
                    }
                    if apply_raw_json_event(&mut state, text.as_ref(), on_update)? {
                        return Ok(state);
                    }
                }
                InboundEvent::Closed(reason) => {
                    return Err(LlmError::HttpStatus(
                        0,
                        format!("stream error: ws closed mid-stream ({reason})"),
                    ));
                }
                InboundEvent::Error(msg) => {
                    return Err(LlmError::HttpStatus(0, format!("stream error: {msg}")));
                }
                InboundEvent::AbortWake => continue,
            }
        }
    }
}
impl Drop for WsConn {
    fn drop(&mut self) {
        // Stops the two background tasks at the next await point —
        // the runtime then closes the underlying socket as a side
        // effect of dropping its owned `WebSocketStream` halves.
        self.reader_abort.abort();
        self.writer_abort.abort();
    }
}

pub(super) fn run_vcr_replay_turn(
    vcr_config: &tau_vcr::VcrConfig,
    config: &ResponsesConfig,
    agent_prompt_id: &str,
    request: &PromptPayload<'_>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<Option<StreamState>, LlmError> {
    let mut conn = WsReplayConn {
        cached_response_id: latest_response_id(request).map(str::to_owned),
    };
    conn.run_turn(vcr_config, config, agent_prompt_id, request, on_update)
}

struct WsReplayConn {
    cached_response_id: Option<String>,
}
impl WsReplayConn {
    pub(super) fn run_turn(
        &mut self,
        vcr_config: &tau_vcr::VcrConfig,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<Option<StreamState>, LlmError> {
        let cached_response_id = self.cached_response_id.as_deref();
        let envelope = build_ws_envelope(config, request, cached_response_id, None);
        let mut request_body = serde_json::to_value(&envelope).map_err(LlmError::Json)?;
        super::redact_image_data_urls(&mut request_body);
        let Some(cassette) = load_provider_stream_cassette(
            vcr_config,
            request,
            agent_prompt_id,
            tau_proto::ProviderBackendTransport::Websocket,
            &request_body,
        )?
        else {
            return Ok(None);
        };
        let state = run_replay(&cassette.stream, on_update)?;
        self.cached_response_id = state.response_id.clone();
        Ok(Some(state))
    }
}

fn latest_response_id<'a>(request: &'a PromptPayload<'_>) -> Option<&'a str> {
    request
        .context
        .blocks
        .iter()
        .rev()
        .find_map(|block| match block {
            tau_proto::ContextBlock::AssistantResponse(response) => {
                response.provider_response_id.as_deref()
            }
            tau_proto::ContextBlock::UserInput(_) | tau_proto::ContextBlock::ToolResults(_) => None,
        })
}
fn run_replay(
    stream: &ProviderRawEventStream,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let mut state = StreamState::new();
    for event in &stream.raw_events {
        std::thread::sleep(super::scale_delay(
            std::time::Duration::from_micros(event.delta_micros),
            100.0,
        ));
        if apply_raw_json_event(&mut state, &event.raw, on_update)? {
            break;
        }
    }
    Ok(state)
}

/// Build the client `Request` for the WS upgrade — URL + bearer +
/// Codex-specific headers.
fn build_request(config: &ResponsesConfig, thread_id: &str) -> Result<Request, LlmError> {
    let url = build_ws_url(&config.base_url)?;
    let mut request: Request = url
        .as_str()
        .into_client_request()
        .map_err(|error| LlmError::ReloadableConfig(format!("WebSocket request: {error}")))?;
    set_header(
        request.headers_mut(),
        "Authorization",
        &format!("Bearer {}", config.api_key),
    )?;
    set_header(request.headers_mut(), "OpenAI-Beta", OPENAI_BETA_WS)?;
    set_header(request.headers_mut(), "session-id", thread_id)?;
    set_header(request.headers_mut(), "thread-id", thread_id)?;
    if let Some(account_id) = config.account_id.as_deref() {
        set_header(request.headers_mut(), "chatgpt-account-id", account_id)?;
    }
    Ok(request)
}

/// Reader task. Pumps server frames into the inbound channel until
/// the stream ends, the channel receiver is dropped (WsConn went
/// away), or the task is aborted on Drop. Auto-pongs are handled
/// transparently inside `tungstenite`'s state machine — they're
/// buffered on the sink half and flushed by the writer task's next
/// send (the periodic ping in the steady state).
async fn read_loop(mut stream: Stream, tx: std_mpsc::Sender<InboundEvent>) {
    while let Some(item) = stream.next().await {
        let (event, terminal) = match item {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<serde_json::Value>(text.as_ref()) {
                    Ok(_) => (InboundEvent::Event { text }, false),
                    // Unparseable frames are skipped on the SSE path too
                    // (line-level resync). Mirror it here.
                    Err(_) => continue,
                }
            }
            Ok(Message::Close(frame)) => {
                let reason = frame
                    .as_ref()
                    .map(|f| format!("code={} reason={}", f.code, f.reason))
                    .unwrap_or_else(|| "no close frame".to_owned());
                tracing::info!(
                    target: crate::LOG_TARGET,
                    %reason,
                    "ws server sent close frame — connection will be reopened on next turn",
                );
                (InboundEvent::Closed(reason), true)
            }
            Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                // Codex never sends binary. Ping/Pong are protocol
                // control frames — tungstenite surfaces them after
                // auto-handling, no caller action needed.
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: crate::LOG_TARGET,
                    error = %e,
                    "ws read failed — connection will be reopened on next turn",
                );
                (InboundEvent::Error(format!("{e}")), true)
            }
        };
        if tx.send(event).is_err() {
            // Receiver dropped — WsConn went away mid-stream. We're
            // done.
            return;
        }
        if terminal {
            return;
        }
    }
    // Stream ended without a close frame (clean EOF). Surface it as
    // a `Closed` so the next `run_turn` call returns a retryable
    // error rather than hanging on `blocking_recv`.
    let _ = tx.send(InboundEvent::Closed("stream ended".to_owned()));
}

/// Writer task. Drains outbound commands and emits periodic client
/// pings to keep the upstream's keepalive timer happy. Exits when
/// the command channel is closed (WsConn was dropped) or when the
/// sink errors (server hung up mid-write); on the latter, signals
/// the failure through `inbound_tx` so a sync `run_turn` waiting on
/// events wakes immediately rather than waiting on the
/// reader to independently notice the close (which it might miss
/// entirely on a half-open socket).
async fn write_loop(
    mut sink: Sink,
    mut rx: UnboundedReceiver<WsCommand>,
    inbound_tx: std_mpsc::Sender<InboundEvent>,
    ping_interval: Duration,
) {
    let mut ticker = tokio::time::interval(ping_interval);
    // First tick fires immediately by default — skip it. Pinging
    // right after a freshly-completed upgrade burns RTT for no
    // benefit; the upstream's timer just reset.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(WsCommand::SendText(text)) => {
                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                        let _ = inbound_tx.send(InboundEvent::Error(format!("ws send failed: {e}")));
                        return;
                    }
                }
                // Command channel closed — WsConn was dropped.
                // Close the sink gracefully so the server gets a
                // proper close frame instead of a torn TCP socket.
                // No `inbound_tx` signal: the receiver was dropped
                // alongside the sender (both live on `WsConn`).
                None => {
                    let _ = sink.close().await;
                    return;
                }
            },
            _ = ticker.tick() => {
                match sink.send(Message::Ping(Vec::new().into())).await {
                    Ok(()) => {
                        // Pings are 25 s apart — info isn't spammy at
                        // that cadence, and a runtime log that suddenly
                        // *stops* showing them is the clearest signal
                        // that the writer task is stuck (and that the
                        // upstream's reap timer is therefore counting
                        // down toward a 1011 close). When we're confident
                        // the keepalive path is solid, demote to debug.
                        tracing::info!(
                            target: crate::LOG_TARGET,
                            "ws keepalive ping sent",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: crate::LOG_TARGET,
                            error = %e,
                            "ws keepalive ping failed — writer task exiting, next turn will reopen",
                        );
                        let _ = inbound_tx.send(InboundEvent::Error(format!("ws keepalive failed: {e}")));
                        return;
                    }
                }
            }
        }
    }
}

/// Map the configured HTTP base URL to a `ws://` / `wss://` URL
/// pointing at the WebSocket endpoint. The Codex backend lives at
/// the same path as the HTTP+SSE endpoint (`/codex/responses`) — the
/// only delta is the scheme.
fn build_ws_url(base_url: &str) -> Result<String, LlmError> {
    let base = base_url.trim_end_matches('/');
    let rest = if let Some(rest) = base.strip_prefix("https://") {
        return Ok(format!("wss://{rest}/codex/responses"));
    } else if let Some(rest) = base.strip_prefix("http://") {
        rest
    } else {
        return Err(LlmError::ReloadableConfig(format!(
            "WebSocket scheme unsupported in base_url: {base_url}"
        )));
    };
    Ok(format!("ws://{rest}/codex/responses"))
}

fn set_header(
    headers: &mut tungstenite::http::HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), LlmError> {
    let header_value = value
        .parse()
        .map_err(|error| LlmError::ReloadableConfig(format!("WebSocket header {name}: {error}")))?;
    headers.insert(name, header_value);
    Ok(())
}

fn map_ws_connect_error(e: tungstenite::Error) -> LlmError {
    if let tungstenite::Error::Http(response) = &e {
        let code = response.status().as_u16();
        let body = response
            .body()
            .as_ref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(str::to_owned)
            .unwrap_or_default();
        return LlmError::HttpStatus(code, body);
    }
    // Network / TLS / protocol — treat as retryable transport.
    LlmError::HttpStatus(0, format!("stream error: ws connect: {e}"))
}

#[cfg(test)]
mod tests;
