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
//! WebSocket control ping. The control pings keep the upstream's keepalive
//! timer happy (default 25 s; the live Codex server reaps with a 1011
//! "keepalive ping timeout" close when no client pong has been seen
//! recently). They are transport control frames, not Responses requests, so
//! they cannot run inference or refresh a prompt cache. The sync [`WsConn`]
//! type holds the channel handles — `run_turn` is sync, owned by the provider's
//! main loop, and just marshals envelopes to the writer task and pulls events
//! back from the reader.

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io as path_std_io, thread, time as path_std_time};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use tau_provider::private_attempt_trace as private_trace;
use tokio::sync::Notify;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use tokio::time as path_tokio_time;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{WebSocketStream, tungstenite};
use tungstenite::{
    error as path_tungstenite_error, handshake as path_tungstenite_handshake,
    http as path_tungstenite_http, protocol as path_tungstenite_protocol,
};

use super::compact_stream::CompactStreamShape;
use super::{
    CachedResponseAnchor, DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT, ProviderRawEventStream,
    ResponsesConfig, apply_parsed_json_event, build_ws_envelope,
    load_provider_stream_cassette_candidates, projected_retained_state_bytes,
    record_provider_raw_event_after, stream_idle_timeout_error,
};
use crate::common::{LlmError, PromptPayload, StreamState};
use crate::decoded_event::DecodedEvent;
use crate::responses::ws_runtime;
use crate::{TurnAbort, attempt_failure as path_crate_attempt_failure};

/// Parser language selected for one Responses envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponseMode {
    /// Ordinary inference compatibility parsing.
    Ordinary,
    /// Native standalone-compaction original-event validation.
    Compact,
}

/// Beta-feature header value the OpenAI WebSocket endpoint expects.
/// Dated by the server; will need a bump when OpenAI rolls a new
/// release. Pinned here as a single `const` so that bump is a
/// one-line change.
pub const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

/// How often the writer task sends an unsolicited WebSocket control `Ping`.
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
/// Periodic WebSocket control pings ensure pongs don't sit buffered for a full
/// turn boundary.
///
/// This is transport-only: it sends no Responses envelope, performs no
/// inference, and cannot refresh a prompt cache.
const WEBSOCKET_CONTROL_PING_INTERVAL: Duration = Duration::from_secs(25);

/// How long one WS turn may go without any provider event before Tau treats
/// the socket as wedged and returns a retryable WebSocket error to the caller.
const TURN_EVENT_TIMEOUT: Duration = DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT;
/// Maximum accepted WebSocket frame and complete message size.
const MAX_WS_EVENT_BYTES: usize = 1024 * 1024;
/// Maximum cumulative provider text accepted by one finite attempt.
const MAX_ATTEMPT_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum logical retained semantic state admitted by one finite attempt.
const MAX_RETAINED_STATE_BYTES: u64 = 64 * 1024 * 1024;
/// Fixed content-free error for either finite-attempt resource ceiling.
const RESPONSE_RESOURCE_LIMIT_ERROR: &str = "Codex WebSocket response resource limit exceeded";

fn response_resource_limit_error() -> LlmError {
    LlmError::InvalidResponse(RESPONSE_RESOURCE_LIMIT_ERROR.to_owned())
}

fn is_response_resource_limit(error: &LlmError) -> bool {
    matches!(
        error.root_error(),
        LlmError::InvalidResponse(message) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    )
}

fn checked_attempt_response_bytes(current: u64, event: usize) -> Option<u64> {
    current
        .checked_add(u64::try_from(event).ok()?)
        .filter(|total| *total <= MAX_ATTEMPT_RESPONSE_BYTES)
}

fn malformed_text_error(response_bytes: usize) -> LlmError {
    let frame = path_crate_attempt_failure::FrameFailure::new(
        path_crate_attempt_failure::FrameFailureKind::MalformedText,
        response_bytes,
    );
    LlmError::HttpStatus(0, "stream error: websocket transport failed".to_owned()).observed(
        path_crate_attempt_failure::AttemptFailureEvidence::transport(
            path_crate_attempt_failure::TransportPhase::ResponseStream,
            true,
            path_crate_attempt_failure::TransportFailureKind::Frame(frame),
        ),
    )
}

fn writer_failure_error(kind: path_crate_attempt_failure::TransportFailureKind) -> LlmError {
    LlmError::HttpStatus(0, "stream error: websocket transport failed".to_owned()).observed(
        path_crate_attempt_failure::AttemptFailureEvidence::transport(
            path_crate_attempt_failure::TransportPhase::ResponseStream,
            true,
            kind,
        ),
    )
}
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

/// Optional recording, evidence, and deadline policy for one envelope run.
struct EnvelopeExecution<'a> {
    /// VCR stream receiving accepted provider frames.
    recording_stream: Option<&'a mut ProviderRawEventStream>,
    /// Parser work allowed for failure evidence.
    evidence_mode: crate::attempt_failure::ProviderEvidenceMode,
    /// Idle and absolute response deadlines.
    timeouts: EnvelopeTimeouts,
    /// Response event language for this envelope.
    response_mode: ResponseMode,
}

/// Applies the WebSocket-only rate-limit side channel before delegating
/// ordinary Responses events to the transport-neutral event parser.
fn apply_ws_json_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    raw_item_json: Option<&str>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    apply_ws_json_event_with_limit(
        state,
        event,
        raw_item_json,
        MAX_RETAINED_STATE_BYTES,
        on_update,
    )
}

fn apply_ws_json_event_with_limit(
    state: &mut StreamState,
    event: &serde_json::Value,
    raw_item_json: Option<&str>,
    retained_state_limit: u64,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    if let Some(observation) = crate::quota::parse_ws_event_value(event) {
        state.quota_observation = Some(observation);
        on_update(state);
        return Ok(false);
    }
    let retained_state_bytes = projected_retained_state_bytes(state, event, raw_item_json)?;
    if retained_state_limit < retained_state_bytes {
        return Err(response_resource_limit_error());
    }
    let terminal = apply_parsed_json_event(state, event, raw_item_json, on_update)?;
    state.commit_retained_state_bytes(retained_state_bytes);
    Ok(terminal)
}

fn websocket_config() -> path_tungstenite_protocol::WebSocketConfig {
    path_tungstenite_protocol::WebSocketConfig::default()
        .max_frame_size(Some(MAX_WS_EVENT_BYTES))
        .max_message_size(Some(MAX_WS_EVENT_BYTES))
}

fn apply_ws_replay_decoded_event(
    state: &mut StreamState,
    decoded: &crate::decoded_event::DecodedEvent<'_>,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<bool, LlmError> {
    if let Some(observation) = crate::quota::parse_ws_event_value(decoded.value()) {
        state.quota_observation = Some(observation);
        on_update(state);
        return Ok(false);
    }
    apply_parsed_json_event(state, decoded.value(), decoded.raw_item(), on_update)
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
/// The reader forwards bounded raw text. The synchronous turn owner performs
/// the sole full-event JSON decode and reports malformed text.
enum InboundEvent {
    /// One bounded raw upstream text event.
    Event { text: Utf8Bytes },
    /// Server sent a `Close` frame or the stream ended cleanly without one.
    /// Carries only the semantic termination fact used by bounded diagnostics.
    Closed {
        /// Semantic close/EOF fact.
        termination: crate::attempt_failure::WsTermination,
    },
    /// One rejected provider frame whose length drives both accounting and
    /// persistent diagnostics.
    FrameFailure(crate::attempt_failure::FrameFailure),
    /// Transport / protocol error mid-stream.
    Error {
        /// Closed locally generated failure kind.
        kind: crate::attempt_failure::TransportFailureKind,
    },
    /// Tungstenite rejected a frame or complete message at Tau's explicit cap.
    ResourceLimit,
}

const CONTROL_ABORT: u8 = 1;
const CONTROL_WRITER_SEND_FAILURE: u8 = 2;
const CONTROL_WRITER_PING_FAILURE: u8 = 4;

/// Coalesced constant-size local control path and sync-owner wake target.
struct InboundControl {
    /// Pending local control bits.
    pending: AtomicU8,
    /// Sync turn-owner thread to wake from async transport tasks.
    waiter: Mutex<Option<thread::Thread>>,
}

impl InboundControl {
    /// Constructs an empty control path.
    fn new() -> Self {
        Self {
            pending: AtomicU8::new(0),
            waiter: Mutex::new(None),
        }
    }

    /// Registers the current synchronous turn owner.
    fn register_waiter(&self) {
        *self.waiter.lock().expect("inbound waiter lock") = Some(thread::current());
    }

    /// Wakes the turn owner after provider data becomes available.
    fn notify_data(&self) {
        self.wake();
    }

    /// Coalesces one cancellation wake.
    fn notify_abort(&self) {
        self.pending.fetch_or(CONTROL_ABORT, Ordering::Release);
        self.wake();
    }

    /// Coalesces one local writer failure.
    fn notify_writer_failure(&self, kind: path_crate_attempt_failure::TransportFailureKind) {
        let pending = match kind {
            path_crate_attempt_failure::TransportFailureKind::WebSocketControlPing => {
                CONTROL_WRITER_PING_FAILURE
            }
            _ => CONTROL_WRITER_SEND_FAILURE,
        };
        self.pending.fetch_or(pending, Ordering::Release);
        self.wake();
    }

    /// Takes pending controls with cancellation priority.
    fn take(&self) -> Option<InboundControlEvent> {
        let pending = self.pending.load(Ordering::Acquire);
        if pending & CONTROL_ABORT != 0 {
            self.pending.fetch_and(!CONTROL_ABORT, Ordering::AcqRel);
            Some(InboundControlEvent::Abort)
        } else if pending & CONTROL_WRITER_SEND_FAILURE != 0 {
            self.pending
                .fetch_and(!CONTROL_WRITER_SEND_FAILURE, Ordering::AcqRel);
            Some(InboundControlEvent::WriterFailure(
                path_crate_attempt_failure::TransportFailureKind::Send,
            ))
        } else if pending & CONTROL_WRITER_PING_FAILURE != 0 {
            self.pending
                .fetch_and(!CONTROL_WRITER_PING_FAILURE, Ordering::AcqRel);
            Some(InboundControlEvent::WriterFailure(
                path_crate_attempt_failure::TransportFailureKind::WebSocketControlPing,
            ))
        } else {
            None
        }
    }

    /// Unparks the registered turn owner without accumulating wake objects.
    fn wake(&self) {
        if let Some(waiter) = self.waiter.lock().expect("inbound waiter lock").as_ref() {
            waiter.unpark();
        }
    }
}

/// Local controls that preempt queued provider data.
enum InboundControlEvent {
    /// Harness cancellation changed state.
    Abort,
    /// The socket writer failed locally.
    WriterFailure(path_crate_attempt_failure::TransportFailureKind),
}

/// One-slot ordered provider-data sender with async backpressure.
#[derive(Clone)]
struct InboundSender {
    /// Bounded provider-data lane.
    tx: Sender<InboundEvent>,
    /// Owner wake target shared with local controls.
    control: Arc<InboundControl>,
}

impl InboundSender {
    /// Sends one provider event, waiting asynchronously for the sole queue
    /// slot.
    async fn send(&self, event: InboundEvent) -> Result<(), ()> {
        self.tx.send(event).await.map_err(|_| ())?;
        self.control.notify_data();
        Ok(())
    }

    /// Sends one synthetic provider event from synchronous tests.
    #[cfg(test)]
    fn send_blocking(&self, event: InboundEvent) -> Result<(), ()> {
        self.tx.blocking_send(event).map_err(|_| ())?;
        self.control.notify_data();
        Ok(())
    }
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
    inbound_rx: Receiver<InboundEvent>,
    inbound_control: Arc<InboundControl>,
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
    /// Drains local controls before provider data, preserving coalesced
    /// failures.
    fn check_inbound_control(&self, abort: &mut impl TurnAbort) -> Result<(), LlmError> {
        while let Some(control) = self.inbound_control.take() {
            match control {
                InboundControlEvent::Abort if abort.is_aborted() => {
                    return Err(LlmError::Canceled);
                }
                InboundControlEvent::Abort => {}
                InboundControlEvent::WriterFailure(kind) => {
                    return Err(writer_failure_error(kind));
                }
            }
        }
        Ok(())
    }

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
        let (inbound_tx, inbound_rx) = mpsc::channel(1);
        let inbound_control = Arc::new(InboundControl::new());
        let inbound_sender = InboundSender {
            tx: inbound_tx,
            control: Arc::clone(&inbound_control),
        };
        let reader_abort = runtime
            .spawn(read_loop(stream, inbound_sender))
            .abort_handle();
        let writer_abort = runtime
            .spawn(write_loop(
                sink,
                outbound_rx,
                Arc::clone(&inbound_control),
                WEBSOCKET_CONTROL_PING_INTERVAL,
            ))
            .abort_handle();

        Ok(Self {
            outbound_tx,
            inbound_rx,
            inbound_control,
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
        correlation: Option<crate::attempt_failure::DispatchCorrelation>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<WsTurnResult, LlmError> {
        self.run_response(
            config,
            agent_prompt_id,
            request,
            correlation,
            recording_stream,
            ResponseMode::Ordinary,
            abort,
            on_dispatched,
            on_update,
            &mut None,
        )
    }

    /// Sends one native standalone-compaction envelope through the compact-only
    /// original-event parser.
    #[expect(
        clippy::too_many_arguments,
        reason = "transport lifecycle callbacks remain separate typed boundaries"
    )]
    pub(super) fn run_compact(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        correlation: Option<crate::attempt_failure::DispatchCorrelation>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
    ) -> Result<WsTurnResult, LlmError> {
        self.run_response(
            config,
            agent_prompt_id,
            request,
            correlation,
            recording_stream,
            ResponseMode::Compact,
            abort,
            on_dispatched,
            on_update,
            &mut None,
        )
    }

    /// Runs one envelope with an explicitly selected response parser.
    #[expect(
        clippy::too_many_arguments,
        reason = "transport lifecycle callbacks remain separate typed boundaries"
    )]
    pub(super) fn run_response(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        correlation: Option<crate::attempt_failure::DispatchCorrelation>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        response_mode: ResponseMode,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
        private_trace: &mut Option<private_trace::AttemptTrace>,
    ) -> Result<WsTurnResult, LlmError> {
        self.run_response_with_capture_submit_observed(
            config,
            agent_prompt_id,
            request,
            correlation,
            recording_stream,
            response_mode,
            abort,
            on_dispatched,
            on_update,
            private_trace,
            tau_provider::debug_capture_writer::submit_provider_debug_capture,
        )
    }

    /// Runs one response with an injected capture sink and no private stage
    /// observation, preserving the focused test seam.
    #[cfg(test)]
    #[expect(
        clippy::too_many_arguments,
        reason = "the injected sink observes the existing transport lifecycle"
    )]
    fn run_response_with_capture_submit(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        correlation: Option<crate::attempt_failure::DispatchCorrelation>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        response_mode: ResponseMode,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
        capture_submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
    ) -> Result<WsTurnResult, LlmError> {
        self.run_response_with_capture_submit_observed(
            config,
            agent_prompt_id,
            request,
            correlation,
            recording_stream,
            response_mode,
            abort,
            on_dispatched,
            on_update,
            &mut None,
            capture_submit,
        )
    }

    /// Runs one response with an injected debug-capture sink for deterministic
    /// transport-boundary verification.
    #[expect(
        clippy::too_many_arguments,
        reason = "the injected sink observes the existing transport lifecycle"
    )]
    fn run_response_with_capture_submit_observed(
        &mut self,
        config: &ResponsesConfig,
        agent_prompt_id: &str,
        request: &PromptPayload<'_>,
        correlation: Option<crate::attempt_failure::DispatchCorrelation>,
        recording_stream: Option<&mut ProviderRawEventStream>,
        response_mode: ResponseMode,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
        private_trace: &mut Option<private_trace::AttemptTrace>,
        capture_submit: impl FnOnce(tau_provider::debug_capture_writer::ProviderDebugCapture),
    ) -> Result<WsTurnResult, LlmError> {
        let lowering_started = private_trace::started(private_trace);
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
        if let (Some(trace), Some(started)) = (private_trace.as_mut(), lowering_started) {
            trace.lowering_finished_from(started);
        }
        let request_body = recorded_request_body(&envelope, recording_stream.is_some())?;
        let capture_started = private_trace::started(private_trace);
        super::maybe_debug_submit_provider_request_with(
            agent_prompt_id,
            config,
            request,
            tau_proto::ProviderBackendTransport::Websocket,
            correlation,
            &envelope,
            capture_submit,
        );
        if let (Some(trace), Some(started)) = (private_trace.as_mut(), capture_started) {
            trace.capture_finished(started);
        }
        let mut state = self.run_envelope_with_timeouts(
            agent_prompt_id,
            envelope,
            EnvelopeExecution {
                recording_stream,
                evidence_mode: if request.debug_provider_requests {
                    path_crate_attempt_failure::ProviderEvidenceMode::Persistent
                } else {
                    path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly
                },
                timeouts: EnvelopeTimeouts {
                    idle: TURN_EVENT_TIMEOUT,
                    absolute: None,
                },
                response_mode,
            },
            abort,
            on_dispatched,
            on_update,
            private_trace,
        )?;
        if supports_cache_read_ceiling(config, state.compaction_update().is_some()) {
            state.prompt_cache_read_ceiling_tokens =
                eligible_previous_input_tokens.map(Self::cache_read_ceiling);
        }
        let response_input_tokens = state.input_tokens;
        self.cached_response_anchor = state.response_id.clone().and_then(|response_id| {
            if response_mode == ResponseMode::Compact {
                let represented_prefix = request.context.flatten();
                state
                    .with_single_compaction_context_item(|compaction| {
                        CachedResponseAnchor::new_with_input_tokens_and_suffix(
                            response_id,
                            &represented_prefix,
                            compaction,
                            response_input_tokens,
                        )
                    })
                    .flatten()
            } else {
                let mut represented_prefix = request.context.flatten();
                represented_prefix.extend(state.output_items_snapshot());
                CachedResponseAnchor::new_with_input_tokens(
                    response_id,
                    &represented_prefix,
                    response_input_tokens,
                )
            }
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
            EnvelopeExecution {
                recording_stream: None,
                evidence_mode: path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
                timeouts: EnvelopeTimeouts {
                    idle: response_timeout,
                    absolute: Some(response_timeout),
                },
                response_mode: ResponseMode::Ordinary,
            },
            abort,
            &mut |_| {},
            &mut |_| {},
            &mut None,
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

    #[expect(
        clippy::too_many_arguments,
        reason = "private observation follows envelope ownership"
    )]
    fn run_envelope_with_timeouts(
        &mut self,
        agent_prompt_id: &str,
        envelope: super::WsResponseCreate,
        mut execution: EnvelopeExecution<'_>,
        abort: &mut impl TurnAbort,
        on_dispatched: &mut impl FnMut(Instant),
        on_update: &mut impl FnMut(&StreamState),
        private_trace: &mut Option<private_trace::AttemptTrace>,
    ) -> Result<StreamState, LlmError> {
        serialize_and_enqueue_envelope_observed(
            &envelope,
            abort,
            on_dispatched,
            private_trace,
            |text| {
                self.outbound_tx
                    .send(WsCommand::SendText(text))
                    .map_err(|_| {
                        LlmError::HttpStatus(0, "stream error: ws writer task gone".to_owned())
                            .observed(
                                path_crate_attempt_failure::AttemptFailureEvidence::transport(
                                    path_crate_attempt_failure::TransportPhase::Send,
                                    true,
                                    path_crate_attempt_failure::TransportFailureKind::Send,
                                ),
                            )
                    })
            },
        )?;

        let mut state = StreamState::new();
        let mut compact_shape =
            (execution.response_mode == ResponseMode::Compact).then(CompactStreamShape::default);
        state.provider_evidence_mode = execution.evidence_mode;
        state.carry_transport_response_bytes(std::mem::take(&mut self.carried_response_bytes));
        let turn_started_at = Instant::now();
        let mut last_event_at = Instant::now();
        self.inbound_control.register_waiter();
        let _abort_waker = {
            let inbound_control = Arc::clone(&self.inbound_control);
            abort.register_waker(Arc::new(move || {
                inbound_control.notify_abort();
            }))
        };
        loop {
            if abort.is_aborted() {
                return Err(LlmError::Canceled);
            }
            if execution
                .timeouts
                .absolute
                .is_some_and(|timeout| timeout <= turn_started_at.elapsed())
            {
                return Err(LlmError::HttpStatus(
                    0,
                    "websocket prewarm response timeout".to_owned(),
                ));
            }
            let remaining = execution
                .timeouts
                .idle
                .saturating_sub(last_event_at.elapsed())
                .min(
                    execution
                        .timeouts
                        .absolute
                        .map(|timeout| timeout.saturating_sub(turn_started_at.elapsed()))
                        .unwrap_or(Duration::MAX),
                );
            let wait = remaining.min(Duration::from_secs(1));
            self.check_inbound_control(abort)?;
            let event = match self.inbound_rx.try_recv() {
                Ok(event) => {
                    // A control arriving concurrently with this dequeue wins.
                    if abort.is_aborted() {
                        return Err(LlmError::Canceled);
                    }
                    self.check_inbound_control(abort)?;
                    event
                }
                Err(TryRecvError::Empty) if last_event_at.elapsed() < execution.timeouts.idle => {
                    thread::park_timeout(wait);
                    // Provider-owned response liveness is deadline-driven, not
                    // upstream-event-driven. Wake the outer sampled emitter
                    // during quiet WebSocket waits; it enforces the 1Hz cadence.
                    on_update(&state);
                    continue;
                }
                Err(TryRecvError::Empty) => {
                    return Err(stream_idle_timeout_error(
                        tau_proto::ProviderBackendTransport::Websocket,
                        agent_prompt_id,
                        turn_started_at,
                        last_event_at,
                        execution.timeouts.idle,
                        &state,
                        None,
                    )
                    .observed(
                        path_crate_attempt_failure::AttemptFailureEvidence::transport(
                            path_crate_attempt_failure::TransportPhase::ResponseStream,
                            true,
                            path_crate_attempt_failure::TransportFailureKind::IdleTimeout,
                        ),
                    ));
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(LlmError::HttpStatus(
                        0,
                        "stream error: ws reader task gone".to_owned(),
                    )
                    .observed(
                        path_crate_attempt_failure::AttemptFailureEvidence::transport(
                            path_crate_attempt_failure::TransportPhase::ResponseStream,
                            true,
                            path_crate_attempt_failure::TransportFailureKind::Read,
                        ),
                    ));
                }
            };
            match event {
                InboundEvent::Event { text } => {
                    if let Some(trace) = private_trace.as_mut() {
                        trace.first_input(text.len());
                    }
                    let now = Instant::now();
                    let delta = now.saturating_duration_since(last_event_at);
                    last_event_at = now;
                    let Some(_) = checked_attempt_response_bytes(
                        state.transport_response_bytes(),
                        text.len(),
                    ) else {
                        self.reader_abort.abort();
                        self.writer_abort.abort();
                        return Err(response_resource_limit_error());
                    };
                    state.record_transport_response_bytes(text.len());
                    on_update(&state);
                    if let Some(stream) = execution.recording_stream.as_deref_mut() {
                        record_provider_raw_event_after(stream, delta, text.to_string())?;
                    }
                    let decode_started = private_trace::started(private_trace);
                    let decoded = match DecodedEvent::decode(text.as_ref()) {
                        Ok(decoded) => decoded,
                        Err(_) => {
                            if let (Some(trace), Some(started)) =
                                (private_trace.as_mut(), decode_started)
                            {
                                trace.decoded(started, false);
                            }
                            return Err(malformed_text_error(text.len()));
                        }
                    };
                    if let (Some(trace), Some(started)) = (private_trace.as_mut(), decode_started) {
                        trace.decoded(started, false);
                    }
                    if let Some(shape) = compact_shape.as_mut() {
                        shape.validate(decoded.value())?;
                    }
                    let mut observed_update = |state: &StreamState| {
                        if state.has_timed_semantic_output()
                            && let Some(trace) = private_trace.as_mut()
                        {
                            trace.semantic_qualified();
                        }
                        on_update(state);
                    };
                    let terminal = apply_ws_json_event(
                        &mut state,
                        decoded.value(),
                        decoded.raw_item(),
                        &mut observed_update,
                    );
                    if terminal.as_ref().is_err_and(is_response_resource_limit) {
                        self.reader_abort.abort();
                        self.writer_abort.abort();
                    }
                    if terminal? {
                        return Ok(state);
                    }
                }
                InboundEvent::Closed { termination } => {
                    let evidence = path_crate_attempt_failure::AttemptFailureEvidence::transport(
                        path_crate_attempt_failure::TransportPhase::ResponseStream,
                        true,
                        path_crate_attempt_failure::TransportFailureKind::WebSocketTermination(
                            termination.clone(),
                        ),
                    );
                    return Err(LlmError::WsClosed(termination).observed(evidence));
                }
                InboundEvent::FrameFailure(frame) => {
                    state.record_transport_response_bytes(frame.response_bytes());
                    on_update(&state);
                    let evidence = path_crate_attempt_failure::AttemptFailureEvidence::transport(
                        path_crate_attempt_failure::TransportPhase::ResponseStream,
                        true,
                        path_crate_attempt_failure::TransportFailureKind::Frame(frame),
                    );
                    return Err(LlmError::HttpStatus(
                        0,
                        "stream error: websocket transport failed".to_owned(),
                    )
                    .observed(evidence));
                }
                InboundEvent::Error { kind } => {
                    let evidence = path_crate_attempt_failure::AttemptFailureEvidence::transport(
                        path_crate_attempt_failure::TransportPhase::ResponseStream,
                        true,
                        kind,
                    );
                    return Err(LlmError::HttpStatus(
                        0,
                        "stream error: websocket transport failed".to_owned(),
                    )
                    .observed(evidence));
                }
                InboundEvent::ResourceLimit => {
                    self.reader_abort.abort();
                    self.writer_abort.abort();
                    return Err(response_resource_limit_error());
                }
            }
        }
    }

    /// Carries transport bytes into the immediately following repair attempt.
    pub(super) fn carry_response_bytes(&mut self, bytes: u64) {
        self.carried_response_bytes = bytes;
    }
}

/// ChatGPT models with the observed WebSocket cache-read geometry.
const CHATGPT_CACHE_READ_GEOMETRY_MODELS: &[&str] =
    &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

/// Matches the exact route contract that can establish an exact cache-read
/// ceiling for a non-compaction response.
fn supports_cache_read_ceiling(config: &ResponsesConfig, compaction: bool) -> bool {
    !compaction
        && config.base_url == "https://chatgpt.com/backend-api"
        && config.mode == super::ResponsesMode::Standard
        && CHATGPT_CACHE_READ_GEOMETRY_MODELS.contains(&config.model_id.as_str())
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
) -> Result<(SharedStream, path_tungstenite_handshake::client::Response), tungstenite::Error> {
    let websocket_url = request.uri().to_string();
    let http_url = if let Some(rest) = websocket_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = websocket_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        return Err(tungstenite::Error::Url(
            path_tungstenite_error::UrlError::UnsupportedUrlScheme,
        ));
    };
    let key = request
        .headers()
        .get("sec-websocket-key")
        .cloned()
        .ok_or_else(|| {
            tungstenite::Error::Protocol(
                path_tungstenite_error::ProtocolError::MissingSecWebSocketKey,
            )
        })?;
    let client = network
        .client_for(&websocket_url)
        .map_err(|error| tungstenite::Error::Io(path_std_io::Error::other(error)))?;
    let mut outbound = client.get(&http_url).version(reqwest::Version::HTTP_11);
    for (name, value) in request.headers() {
        outbound = outbound.header(name, value);
    }
    let response = outbound.send().await.map_err(|error| {
        tungstenite::Error::Io(path_std_io::Error::other(network.reqwest_error(
            &websocket_url,
            tau_provider::OutboundPhase::Request,
            &error,
        )))
    })?;
    let status = response.status();
    if status != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        if let Some(error) = network.proxy_response_error(&websocket_url, status.as_u16()) {
            return Err(tungstenite::Error::Io(path_std_io::Error::other(error)));
        }
        return Err(tungstenite::Error::Http(Box::new(
            path_tungstenite_http::Response::builder()
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
        return Err(tungstenite::Error::Io(path_std_io::Error::other(
            network.protocol_error(&websocket_url, tau_provider::OutboundPhase::Request),
        )));
    }
    let response_headers = response.headers().clone();
    let upgraded = response.upgrade().await.map_err(|_| {
        tungstenite::Error::Io(path_std_io::Error::other(
            network.protocol_error(&websocket_url, tau_provider::OutboundPhase::Request),
        ))
    })?;
    let stream = WebSocketStream::from_raw_socket(
        upgraded,
        path_tungstenite_protocol::Role::Client,
        Some(websocket_config()),
    )
    .await;
    let mut handshake = path_tungstenite_http::Response::builder().status(status);
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
                .observed(
                    path_crate_attempt_failure::AttemptFailureEvidence::transport(
                        path_crate_attempt_failure::TransportPhase::PreUpgrade,
                        false,
                        path_crate_attempt_failure::TransportFailureKind::Outbound,
                    ),
                )
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

/// Serializes one Responses envelope and hands it to the writer only while the
/// turn still owns cancellation authority.
fn serialize_and_enqueue_envelope(
    envelope: &impl serde::Serialize,
    abort: &mut impl TurnAbort,
    on_dispatched: &mut impl FnMut(Instant),
    enqueue: impl FnOnce(String) -> Result<(), LlmError>,
) -> Result<(), LlmError> {
    serialize_and_enqueue_envelope_observed(envelope, abort, on_dispatched, &mut None, enqueue)
}

/// Observed serializer/enqueue path used only by the private stage trace.
fn serialize_and_enqueue_envelope_observed(
    envelope: &impl serde::Serialize,
    abort: &mut impl TurnAbort,
    on_dispatched: &mut impl FnMut(Instant),
    private_trace: &mut Option<private_trace::AttemptTrace>,
    enqueue: impl FnOnce(String) -> Result<(), LlmError>,
) -> Result<(), LlmError> {
    if abort.is_aborted() {
        return Err(LlmError::Canceled);
    }
    let serialization_started = private_trace::started(private_trace);
    let text = serde_json::to_string(envelope).map_err(LlmError::Json)?;
    if let (Some(trace), Some(started)) = (private_trace.as_mut(), serialization_started) {
        trace.serialization_finished(started, text.len());
    }
    if abort.is_aborted() {
        return Err(LlmError::Canceled);
    }
    on_dispatched(Instant::now());
    if abort.is_aborted() {
        return Err(LlmError::Canceled);
    }
    if let Some(trace) = private_trace.as_mut() {
        trace.record_dispatch();
    }
    let enqueue_started = private_trace::started(private_trace);
    let result = enqueue(text);
    if let (Some(trace), Some(started)) = (private_trace.as_mut(), enqueue_started) {
        trace.enqueue_finished(started);
    }
    result
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
    response_mode: ResponseMode,
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
    run_replay(&cassette.stream, response_mode, on_update).map(Some)
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
    response_mode: ResponseMode,
    on_update: &mut impl FnMut(&StreamState),
) -> Result<StreamState, LlmError> {
    let mut state = StreamState::new();
    let mut compact_shape =
        (response_mode == ResponseMode::Compact).then(CompactStreamShape::default);
    for (index, event) in stream.raw_events.iter().enumerate() {
        let decoded =
            DecodedEvent::decode(&event.raw).map_err(|_| malformed_text_error(event.raw.len()))?;
        if let Some(shape) = compact_shape.as_mut() {
            shape.validate(decoded.value())?;
        }
        let terminal = apply_ws_replay_decoded_event(&mut state, &decoded, on_update)?;
        if terminal {
            if index + 1 != stream.raw_events.len() {
                return Err(super::replay_unconsumed_frames_error(
                    tau_proto::ProviderBackendTransport::Websocket,
                    stream.raw_events.len() - index - 1,
                ));
            }
            return Ok(state);
        }
    }
    let now = path_std_time::Instant::now();
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
async fn read_loop(mut stream: Stream, tx: InboundSender) {
    while let Some(item) = stream.next().await {
        let (event, terminal) = match item {
            Ok(Message::Text(text)) => (InboundEvent::Event { text }, false),
            Ok(Message::Close(frame)) => {
                tracing::info!(
                    target: crate::LOG_TARGET,
                    "ws server closed connection; it will be reopened on the next turn",
                );
                (
                    InboundEvent::Closed {
                        termination: path_crate_attempt_failure::WsTermination::CloseFrame {
                            code: frame.as_ref().map(|frame| u16::from(frame.code)),
                            reason: frame
                                .as_ref()
                                .map(|frame| frame.reason.to_string())
                                .filter(|reason| !reason.is_empty()),
                        },
                    },
                    true,
                )
            }
            Ok(Message::Binary(bytes)) => (
                InboundEvent::FrameFailure(path_crate_attempt_failure::FrameFailure::new(
                    path_crate_attempt_failure::FrameFailureKind::Binary,
                    bytes.len(),
                )),
                true,
            ),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                // Ping/Pong are protocol control frames — tungstenite surfaces them after
                // auto-handling, no caller action needed.
                continue;
            }
            Err(tungstenite::Error::Capacity(_)) => (InboundEvent::ResourceLimit, true),
            Err(e) => {
                tracing::warn!(
                    target: crate::LOG_TARGET,
                    "ws read failed — connection will be reopened on next turn",
                );
                let _ = e;
                (
                    InboundEvent::Error {
                        kind: path_crate_attempt_failure::TransportFailureKind::Read,
                    },
                    true,
                )
            }
        };
        if tx.send(event).await.is_err() {
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
    // error rather than parking until the idle deadline.
    let _ = tx
        .send(InboundEvent::Closed {
            termination: path_crate_attempt_failure::WsTermination::CleanEof,
        })
        .await;
}

/// Writer task. Drains outbound commands and emits periodic client
/// WebSocket control pings to keep the upstream's keepalive timer happy. These
/// frames are transport-only and never carry a Responses request. Exits when
/// the command channel is closed (WsConn was dropped) or when the
/// sink errors (server hung up mid-write); on the latter, signals
/// the failure through the independent control state so a sync `run_turn`
/// wakes immediately rather than waiting on the
/// reader to independently notice the close (which it might miss
/// entirely on a half-open socket).
async fn write_loop(
    mut sink: Sink,
    mut rx: UnboundedReceiver<WsCommand>,
    inbound_control: Arc<InboundControl>,
    ping_interval: Duration,
) {
    let mut ticker = tokio::time::interval(ping_interval);
    // First tick fires immediately by default — skip it. Pinging
    // right after a freshly-completed upgrade burns RTT for no
    // benefit; the upstream's timer just reset.
    ticker.set_missed_tick_behavior(path_tokio_time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(WsCommand::SendText(text)) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        inbound_control.notify_writer_failure(
                            path_crate_attempt_failure::TransportFailureKind::Send,
                        );
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
                        // WebSocket control pings are 25 s apart — info isn't spammy at
                        // that cadence, and a runtime log that suddenly
                        // *stops* showing them is the clearest signal
                        // that the writer task is stuck (and that the
                        // upstream's reap timer is therefore counting
                        // down toward a 1011 close). When we're confident
                        // the WebSocket control-ping path is solid, demote to debug.
                        tracing::info!(
                            target: crate::LOG_TARGET,
                            "websocket_control_ping sent",
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            target: crate::LOG_TARGET,
                            "websocket_control_ping failed — writer task exiting, next turn will reopen",
                        );
                        inbound_control.notify_writer_failure(
                            path_crate_attempt_failure::TransportFailureKind::WebSocketControlPing,
                        );
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
                tau_provider::retry_policy::parse_retry_after(
                    value,
                    path_std_time::SystemTime::now(),
                )
            });
        let request_id = ["x-request-id", "request-id", "openai-request-id"]
            .into_iter()
            .find_map(|name| response.headers().get(name))
            .and_then(|value| value.to_str().ok());
        return match retry_after {
            Some(delay) => LlmError::HttpStatusRetryAfter(code, body, delay),
            None => LlmError::HttpStatus(code, body),
        }
        .observed(path_crate_attempt_failure::AttemptFailureEvidence::upgrade(
            request_id,
            path_crate_attempt_failure::TransportFailureKind::Upgrade,
        ));
    }
    if let tungstenite::Error::Io(error) = &e
        && let Some(outbound) = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<tau_provider::OutboundError>())
    {
        return LlmError::Outbound(outbound.clone()).observed(
            path_crate_attempt_failure::AttemptFailureEvidence::transport(
                path_crate_attempt_failure::TransportPhase::PreUpgrade,
                false,
                path_crate_attempt_failure::TransportFailureKind::Outbound,
            ),
        );
    }
    // Network / TLS / protocol — treat as retryable transport.
    let _ = e;
    LlmError::HttpStatus(0, "stream error: websocket connection failed".to_owned()).observed(
        path_crate_attempt_failure::AttemptFailureEvidence::transport(
            path_crate_attempt_failure::TransportPhase::PreUpgrade,
            false,
            path_crate_attempt_failure::TransportFailureKind::Upgrade,
        ),
    )
}

#[cfg(test)]
mod tests;
