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
//! - Upgrade `wss://{base_url}/codex/responses` with `Authorization`,
//!   `chatgpt-account-id`, `session-id`, `thread-id`, and the dated
//!   `OpenAI-Beta: responses_websockets=2026-02-06` header.
//! - Send one client text frame per turn: a `{ "type": "response.create", ...
//!   }` envelope produced by `super::build_ws_envelope`.
//! - Read server text frames as one decoded `response.*` event each and hand
//!   them to [`super::apply_event`].
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

use std::future::Future;
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{WebSocketStream, tungstenite};

use super::{
    CachedResponseAnchor, DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT, ProviderRawEventStream,
    ResponsesConfig, apply_raw_json_event, build_ws_envelope,
    load_provider_stream_cassette_candidates, record_provider_raw_event_after,
    stream_idle_timeout_error,
};
use crate::TurnAbort;
use crate::common::{LlmError, PromptPayload, StreamState};
use crate::responses::ws_runtime;

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
/// Absolute bound for a best-effort cache prewarm after its request is sent.
pub(super) const PREWARM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time allowed for DNS, TCP, TLS, and the WebSocket HTTP upgrade.
///
/// Provider-frame idle time is governed separately after the upgrade succeeds.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider event timing bounds for one WebSocket envelope.
struct EnvelopeTimeouts {
    /// Maximum quiet interval between provider frames.
    idle: Duration,
    /// Optional absolute bound regardless of provider frame activity.
    absolute: Option<Duration>,
}

/// Applies the WebSocket-only rate-limit side channel before delegating
/// ordinary Responses events to the transport-neutral event parser.
fn apply_ws_raw_json_event(
    state: &mut StreamState,
    data: &str,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    if let Some(observation) = crate::quota::parse_ws_event(data) {
        state.quota_observation = Some(observation);
        on_update(state);
        return Ok(false);
    }
    apply_raw_json_event(state, data, on_update)
}

type SharedStream = WebSocketStream<reqwest::Upgraded>;
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
/// unparseable frames can be quietly dropped at the source.
enum InboundEvent {
    /// One parsed `response.*` event and its upstream text frame.
    Event { text: Utf8Bytes },
    /// Server sent a `Close` frame (or the stream ended cleanly
    /// without one). The string is the close-frame reason for
    /// logging.
    Closed,
    /// Transport / protocol error mid-stream.
    Error {
        /// Fixed local protocol/transport diagnostic.
        detail: String,
        /// Raw provider frame bytes rejected before semantic parsing.
        response_bytes: usize,
    },
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
    /// Cached response id and exact represented prefix for this live socket.
    cached_response_anchor: Option<CachedResponseAnchor>,
    /// Successful cache-only response that may anchor one compatible real turn.
    prewarm_baseline: Option<Box<PrewarmBaseline>>,
    /// Bytes retained from the one discarded transport-repair attempt.
    carried_response_bytes: u64,
}

/// Exact request shape and lowered input accepted by one cache-only response.
struct PrewarmBaseline {
    /// Provider response id valid only on this live socket.
    response_id: String,
    /// Canonical non-input request fields, excluding the prewarm-only flag.
    fingerprint: serde_json::Value,
    /// Exact lowered input prefix represented by `response_id`.
    input_prefix: Vec<serde_json::Value>,
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
    /// # Errors
    ///
    /// Returns typed cancellation, reloadable configuration, outbound
    /// route/phase/category, or provider HTTP rejection errors. The 30-second
    /// upgrade deadline is `Outbound(Deadline)`, while malformed negotiation is
    /// `Outbound(Protocol)`.
    pub fn connect(
        config: &ResponsesConfig,
        thread_id: &str,
        network: &tau_provider::OutboundNetworkPolicy,
        abort: &mut impl TurnAbort,
    ) -> Result<Self, LlmError> {
        Self::connect_with_timeout(config, thread_id, network, abort, CONNECT_TIMEOUT)
    }

    pub(super) fn connect_with_timeout(
        config: &ResponsesConfig,
        thread_id: &str,
        network: &tau_provider::OutboundNetworkPolicy,
        abort: &mut impl TurnAbort,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let websocket_url = build_ws_url(&config.base_url)?;
        let request = build_request(config, thread_id)?;
        let bearer = config.api_key.clone();
        let runtime = ws_runtime::handle();
        let (ws, _response) = wait_for_connect(
            &runtime,
            abort,
            timeout,
            connect_with_policy(request, network),
        )
        .map_err(|error| map_connect_wait_error(error, network, &websocket_url))?;

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
            cached_response_anchor: None,
            prewarm_baseline: None,
            carried_response_bytes: 0,
        })
    }

    /// Send one `response.create` envelope and stream events back
    /// until `response.completed` / `response.done`. Returns the
    /// accumulated [`StreamState`]; leaves the socket open for the
    /// next turn.
    ///
    /// A cached-socket close before semantic output may spend the logical
    /// attempt's sole fresh-socket repair. Parsed model output prohibits
    /// replay; the typed attempt surfaces it so the extension clears
    /// tentative output. Transport bytes from a discarded repair remain
    /// cumulative.
    #[expect(
        clippy::too_many_arguments,
        reason = "transport lifecycle callbacks remain separate typed boundaries"
    )]
    pub(super) fn run_turn(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<WsTurnResult, LlmError> {
        let mut envelope =
            build_ws_envelope(config, request, self.cached_response_anchor.as_ref(), None);
        let eligible_previous_input_tokens = envelope
            .body
            .previous_response_id
            .as_ref()
            .and(self.cached_response_anchor.as_ref())
            .and_then(|anchor| anchor.prompt_input_tokens);
        if self.cached_response_anchor.is_none()
            && let Some(baseline) = self.prewarm_baseline.take()
        {
            let full = build_ws_envelope(config, request, None, None);
            if prewarm_compatible(&baseline, &full) {
                envelope = full;
                envelope.body.previous_response_id = Some(baseline.response_id);
                envelope.body.input.drain(..baseline.input_prefix.len());
            }
        }
        let request_body = recorded_request_body(&envelope, recording_stream.is_some())?;
        super::maybe_debug_submit_provider_request(
            agent_prompt_id,
            config,
            request,
            tau_proto::ProviderBackendTransport::Websocket,
            &envelope,
        );
        let mut state = self.run_envelope(
            agent_prompt_id,
            envelope,
            recording_stream,
            abort,
            on_dispatched,
            on_update,
        )?;
        if supports_cache_read_ceiling(config, state.compaction_update().is_some()) {
            state.prompt_cache_read_ceiling_tokens =
                eligible_previous_input_tokens.map(Self::cache_read_ceiling);
        }
        self.cached_response_anchor = state.response_id.clone().and_then(|response_id| {
            let mut represented_prefix = request.context.flatten();
            represented_prefix.extend(state.output_items_snapshot());
            CachedResponseAnchor::new_with_input_tokens(
                response_id,
                &represented_prefix,
                state.input_tokens,
            )
        });
        Ok(WsTurnResult {
            state,
            request_body,
        })
    }

    /// Applies the provider-local ChatGPT cache reporting geometry.
    fn cache_read_ceiling(tokens: u64) -> u64 {
        const MINIMUM: u64 = 1_536;
        const STEP: u64 = 1_024;
        const OFFSET: u64 = 512;

        if tokens < MINIMUM {
            return 0;
        }
        OFFSET + ((tokens - OFFSET) / STEP) * STEP
    }

    /// Sends one non-generating prewarm envelope with an absolute response
    /// bound.
    ///
    /// No Tau-visible response events are emitted. `abort` wakes a silent peer
    /// and the response wait ends after 30 seconds regardless of nonterminal
    /// provider activity.
    pub fn run_prewarm(
        &mut self,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
        abort: &mut impl TurnAbort,
        deadline: Option<Instant>,
    ) -> Result<StreamState, LlmError> {
        let envelope = build_ws_envelope(config, request, None, Some(false));
        let baseline_shape = prewarm_shape(&envelope);
        let response_timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(PREWARM_RESPONSE_TIMEOUT);
        if response_timeout.is_zero() {
            return Err(LlmError::HttpStatus(
                0,
                "websocket prewarm response timeout".to_owned(),
            ));
        }
        let state = self.run_envelope_with_timeouts(
            "<prewarm>",
            envelope,
            None,
            abort,
            EnvelopeTimeouts {
                idle: response_timeout,
                absolute: Some(response_timeout),
            },
            &mut |_| {},
            &mut |_| {},
        )?;
        self.cached_response_anchor = None;
        self.prewarm_baseline = state.response_id.clone().map(|response_id| {
            Box::new(PrewarmBaseline {
                response_id,
                fingerprint: baseline_shape.0,
                input_prefix: baseline_shape.1,
            })
        });
        Ok(state)
    }

    fn run_envelope(
        &mut self,
        agent_prompt_id: &str,
        envelope: super::WsResponseCreate,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<StreamState, LlmError> {
        self.run_envelope_with_timeouts(
            agent_prompt_id,
            envelope,
            recording_stream,
            abort,
            EnvelopeTimeouts {
                idle: TURN_EVENT_TIMEOUT,
                absolute: None,
            },
            on_dispatched,
            on_update,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "transport lifecycle callbacks remain separate typed boundaries"
    )]
    fn run_envelope_with_timeouts(
        &mut self,
        agent_prompt_id: &str,
        envelope: super::WsResponseCreate,
        mut recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        timeouts: EnvelopeTimeouts,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<StreamState, LlmError> {
        if abort.is_aborted() {
            return Err(LlmError::Canceled);
        }
        let text = serde_json::to_string(&envelope).map_err(LlmError::Json)?;
        on_dispatched(Instant::now());
        self.outbound_tx
            .send(WsCommand::SendText(text))
            .map_err(|_| LlmError::HttpStatus(0, "stream error: ws writer task gone".to_owned()))?;

        let mut state = StreamState::new();
        state.carry_transport_response_bytes(std::mem::take(&mut self.carried_response_bytes));
        let turn_started_at = Instant::now();
        let mut last_event_at = Instant::now();
        let _abort_waker = {
            let inbound_tx = self.inbound_tx.clone();
            abort.register_waker(Arc::new(move || {
                // This call is intentionally best-effort; preserve the existing discarded
                // result. ast-grep-ignore: let-underscore-call
                let _ = inbound_tx.send(InboundEvent::AbortWake);
            }))
        };
        loop {
            if abort.is_aborted() {
                return Err(LlmError::Canceled);
            }
            if timeouts
                .absolute
                .is_some_and(|timeout| timeout <= turn_started_at.elapsed())
            {
                return Err(LlmError::HttpStatus(
                    0,
                    "websocket prewarm response timeout".to_owned(),
                ));
            }
            let remaining = timeouts.idle.saturating_sub(last_event_at.elapsed()).min(
                timeouts
                    .absolute
                    .map(|timeout| timeout.saturating_sub(turn_started_at.elapsed()))
                    .unwrap_or(Duration::MAX),
            );
            let wait = remaining.min(Duration::from_secs(1));
            let event = match self.inbound_rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(std_mpsc::RecvTimeoutError::Timeout)
                    if last_event_at.elapsed() < timeouts.idle =>
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
                        timeouts.idle,
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
                        record_provider_raw_event_after(stream, delta, text.to_string())?;
                    }
                    if apply_ws_raw_json_event(&mut state, text.as_ref(), on_update)? {
                        return Ok(state);
                    }
                }
                InboundEvent::Closed => {
                    return Err(LlmError::HttpStatus(
                        0,
                        "stream error: ws closed mid-stream".to_owned(),
                    ));
                }
                InboundEvent::Error {
                    detail,
                    response_bytes,
                } => {
                    state.record_transport_response_bytes(response_bytes);
                    on_update(&state);
                    return Err(LlmError::HttpStatus(0, format!("stream error: {detail}")));
                }
                InboundEvent::AbortWake => continue,
            }
        }
    }

    /// Carries transport bytes into the immediately following repair attempt.
    pub(super) fn carry_response_bytes(&mut self, bytes: u64) {
        self.carried_response_bytes = bytes;
    }
}

/// Matches the single initial route contract that can establish an exact
/// cache-read ceiling for a non-compaction response.
fn supports_cache_read_ceiling(config: &ResponsesConfig, compaction: bool) -> bool {
    !compaction
        && config.base_url == "https://chatgpt.com/backend-api"
        && config.mode == super::ResponsesMode::Standard
        && config.model_id == "gpt-5.6-sol"
}

fn prewarm_shape(
    envelope: &super::WsResponseCreate,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut value = serde_json::to_value(envelope).expect("Responses envelope serializes");
    let object = value
        .as_object_mut()
        .expect("Responses envelope serializes as an object");
    let input = object
        .remove("input")
        .and_then(|input| input.as_array().cloned())
        .unwrap_or_default();
    object.remove("generate");
    object.remove("previous_response_id");
    (value, input)
}

fn prewarm_compatible(baseline: &PrewarmBaseline, envelope: &super::WsResponseCreate) -> bool {
    let (fingerprint, input) = prewarm_shape(envelope);
    fingerprint == baseline.fingerprint
        && input.len() >= baseline.input_prefix.len()
        && input[..baseline.input_prefix.len()] == baseline.input_prefix
}

async fn connect_with_policy(
    request: Request,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<(SharedStream, tungstenite::handshake::client::Response), tungstenite::Error> {
    let websocket_url = request.uri().to_string();
    let http_url = if let Some(rest) = websocket_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = websocket_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        return Err(tungstenite::Error::Url(
            tungstenite::error::UrlError::UnsupportedUrlScheme,
        ));
    };
    let key = request
        .headers()
        .get("sec-websocket-key")
        .cloned()
        .ok_or_else(|| {
            tungstenite::Error::Protocol(tungstenite::error::ProtocolError::MissingSecWebSocketKey)
        })?;
    let client = network
        .client_for(&websocket_url)
        .map_err(|error| tungstenite::Error::Io(std::io::Error::other(error)))?;
    let mut outbound = client.get(&http_url).version(reqwest::Version::HTTP_11);
    for (name, value) in request.headers() {
        outbound = outbound.header(name, value);
    }
    let response = outbound.send().await.map_err(|error| {
        tungstenite::Error::Io(std::io::Error::other(network.reqwest_error(
            &websocket_url,
            tau_provider::OutboundPhase::Request,
            &error,
        )))
    })?;
    let status = response.status();
    if status != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        if let Some(error) = network.proxy_response_error(&websocket_url, status.as_u16()) {
            return Err(tungstenite::Error::Io(std::io::Error::other(error)));
        }
        return Err(tungstenite::Error::Http(Box::new(
            tungstenite::http::Response::builder()
                .status(status)
                .body(None)
                .expect("valid provider status"),
        )));
    }
    let headers = response.headers();
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
    let expected_accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let accept_ok = headers
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected_accept);
    let negotiation_absent = !headers.contains_key("sec-websocket-extensions")
        && !headers.contains_key("sec-websocket-protocol");
    if !(upgrade_ok && connection_ok && accept_ok && negotiation_absent) {
        return Err(tungstenite::Error::Io(std::io::Error::other(
            network.protocol_error(&websocket_url, tau_provider::OutboundPhase::Request),
        )));
    }
    let response_headers = response.headers().clone();
    let upgraded = response.upgrade().await.map_err(|_| {
        tungstenite::Error::Io(std::io::Error::other(
            network.protocol_error(&websocket_url, tau_provider::OutboundPhase::Request),
        ))
    })?;
    let stream =
        WebSocketStream::from_raw_socket(upgraded, tungstenite::protocol::Role::Client, None).await;
    let mut handshake = tungstenite::http::Response::builder().status(status);
    *handshake
        .headers_mut()
        .expect("response builder exposes headers") = response_headers;
    Ok((stream, handshake.body(None).expect("valid response")))
}

enum ConnectWaitError<E> {
    Canceled,
    Timeout,
    Connect(E),
}

fn map_connect_wait_error(
    error: ConnectWaitError<tungstenite::Error>,
    network: &tau_provider::OutboundNetworkPolicy,
    target: &str,
) -> LlmError {
    match error {
        ConnectWaitError::Canceled => LlmError::Canceled,
        ConnectWaitError::Timeout => {
            LlmError::Outbound(network.deadline_error(target, tau_provider::OutboundPhase::Connect))
        }
        ConnectWaitError::Connect(error) => map_ws_connect_error(error),
    }
}

fn wait_for_connect<F, T, E>(
    runtime: &tokio::runtime::Handle,
    abort: &mut impl TurnAbort,
    timeout: Duration,
    connect: F,
) -> Result<T, ConnectWaitError<E>>
where
    F: Future<Output = Result<T, E>>,
{
    if abort.is_aborted() {
        return Err(ConnectWaitError::Canceled);
    }
    let canceled = Arc::new(Notify::new());
    let wake_canceled = Arc::clone(&canceled);
    let _abort_waker = abort.register_waker(Arc::new(move || wake_canceled.notify_one()));
    if abort.is_aborted() {
        return Err(ConnectWaitError::Canceled);
    }
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            () = canceled.notified() => Err(ConnectWaitError::Canceled),
            result = tokio::time::timeout(timeout, connect) => match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(ConnectWaitError::Connect(error)),
                Err(_) => Err(ConnectWaitError::Timeout),
            },
        }
    });
    if abort.is_aborted() {
        Err(ConnectWaitError::Canceled)
    } else {
        result
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
    // VCR has no live socket proof. Try the only two historical live shapes:
    // causal mismatch/fresh socket sent full context, while a compatible warm
    // socket sent a transcript-derived chained delta.
    let full = build_replay_request_body(config, request, None)?;
    let replay_anchor = CachedResponseAnchor::latest_from_context(request.context);
    let chained = build_replay_request_body(config, request, replay_anchor.as_ref())?;
    let request_bodies = if chained == full {
        vec![full]
    } else {
        vec![full, chained]
    };
    let Some(cassette) = load_provider_stream_cassette_candidates(
        vcr_config,
        request,
        agent_prompt_id,
        tau_proto::ProviderBackendTransport::Websocket,
        &request_bodies,
    )?
    else {
        return Ok(None);
    };
    run_replay(&cassette.stream, on_update).map(Some)
}

fn build_replay_request_body(
    config: &ResponsesConfig,
    request: &PromptPayload<'_>,
    anchor: Option<&CachedResponseAnchor>,
) -> Result<serde_json::Value, LlmError> {
    let envelope = build_ws_envelope(config, request, anchor, None);
    let mut request_body = serde_json::to_value(&envelope).map_err(LlmError::Json)?;
    super::redact_image_data_urls(&mut request_body);
    Ok(request_body)
}
pub(super) fn run_replay(
    stream: &ProviderRawEventStream,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let mut state = StreamState::new();
    for (index, event) in stream.raw_events.iter().enumerate() {
        if apply_ws_raw_json_event(&mut state, &event.raw, on_update)? {
            if index + 1 != stream.raw_events.len() {
                return Err(super::replay_unconsumed_frames_error(
                    tau_proto::ProviderBackendTransport::Websocket,
                    stream.raw_events.len() - index - 1,
                ));
            }
            return Ok(state);
        }
    }
    let now = std::time::Instant::now();
    Err(super::stream_ended_without_terminal_error(
        tau_proto::ProviderBackendTransport::Websocket,
        "vcr-replay",
        now,
        now,
        &state,
    ))
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
                    Err(_) => {
                        let response_bytes = text.len();
                        (
                            InboundEvent::Error {
                                detail: "malformed JSON text frame".to_owned(),
                                response_bytes,
                            },
                            true,
                        )
                    }
                }
            }
            Ok(Message::Close(_)) => {
                tracing::info!(
                    target: crate::LOG_TARGET,
                    "ws server closed connection; it will be reopened on the next turn",
                );
                (InboundEvent::Closed, true)
            }
            Ok(Message::Binary(bytes)) => (
                InboundEvent::Error {
                    detail: "unexpected binary frame".to_owned(),
                    response_bytes: bytes.len(),
                },
                true,
            ),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                // Ping/Pong are protocol control frames — tungstenite surfaces them after
                // auto-handling, no caller action needed.
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: crate::LOG_TARGET,
                    error = %e,
                    "ws read failed — connection will be reopened on next turn",
                );
                (
                    InboundEvent::Error {
                        detail: format!("{e}"),
                        response_bytes: 0,
                    },
                    true,
                )
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
    // This call is intentionally best-effort; preserve the existing discarded
    // result. ast-grep-ignore: let-underscore-call
    let _ = tx.send(InboundEvent::Closed);
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
                        let _ = inbound_tx.send(InboundEvent::Error {
                            detail: format!("ws send failed: {e}"),
                            response_bytes: 0,
                        });
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
                        let _ = inbound_tx.send(InboundEvent::Error {
                            detail: format!("ws keepalive failed: {e}"),
                            response_bytes: 0,
                        });
                        return;
                    }
                }
            }
        }
    }
}

/// Map the configured HTTP base URL to a `ws://` / `wss://` URL
/// pointing at the Codex Responses endpoint.
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
        if code == 426 {
            return LlmError::WsUpgradeRequired;
        }
        let body = response
            .body()
            .as_ref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(str::to_owned)
            .unwrap_or_default();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                tau_provider::retry_policy::parse_retry_after(value, std::time::SystemTime::now())
            });
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: match-option-verbose
        return match retry_after {
            Some(delay) => LlmError::HttpStatusRetryAfter(code, body, delay),
            None => LlmError::HttpStatus(code, body),
        };
    }
    if let tungstenite::Error::Io(error) = &e
        && let Some(outbound) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<tau_provider::OutboundError>())
    {
        return LlmError::Outbound(outbound.clone());
    }
    // Network / TLS / protocol — treat as retryable transport.
    let _ = e;
    LlmError::HttpStatus(0, "stream error: websocket connection failed".to_owned())
}

#[cfg(test)]
mod tests;
