//! Test-only Tau extension used by harness integration tests.
//!
//! The extension registers the [`RESTART_TEST_DUMMY_TOOL_NAME`] fixture tool
//! and an `agent.prompt_submitted` interceptor. It deliberately has no user
//! facing production role; its behavior exists to exercise extension
//! supervision, tool dispatch, replay suppression, and prompt interception.
//! See `ARCH-tau-ext-test-dummy` for the fixture boundary and invariants.

mod release_hold;

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::marker::PhantomData;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use release_hold::ReleaseConfig;
use tau_client::{
    ClientResult, ExtensionBuilder, InterceptDecision, TauExtension, TauExtensionRunner,
};
use tau_proto::{
    AgentPromptSubmitted, CborValue, Event, EventSelector, ImageContent, ImageDetail,
    ImageMediaType, InterceptionPriority, MessageAgentTarget, MessageDelivered, MessageFactId,
    MessageParty, NoticeLevel, RawMessagePublisherId, ToolError, ToolResult, ToolResultContentPart,
    ToolResultKind, ToolSpec,
};
#[cfg(test)]
static SATURATION_HOOK: Mutex<Option<(tau_proto::ToolCallId, mpsc::Sender<bool>)>> =
    Mutex::new(None);

/// Tool name registered by this fixture extension for restart-supervision
/// tests.
pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

/// Tool name reserved for the one typed-image deterministic acceptance mode.
pub const TYPED_IMAGE_TEST_DUMMY_TOOL_NAME: &str = "typed_image_test_dummy";
/// Tool name reserved for provider-context placement acceptance.
pub const PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME: &str = "provider_context_raw_message";

/// Fixed 1×1 indexed-color PNG emitted only by the deterministic typed-image
/// fixture mode.
pub const TYPED_IMAGE_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 1, 3, 0,
    0, 0, 37, 219, 86, 202, 0, 0, 0, 32, 99, 72, 82, 77, 0, 0, 122, 38, 0, 0, 128, 132, 0, 0, 250,
    0, 0, 0, 128, 232, 0, 0, 117, 48, 0, 0, 234, 96, 0, 0, 58, 152, 0, 0, 23, 112, 156, 186, 81,
    60, 0, 0, 0, 6, 80, 76, 84, 69, 18, 52, 86, 255, 255, 255, 81, 0, 114, 117, 0, 0, 0, 1, 98, 75,
    71, 68, 1, 255, 2, 45, 222, 0, 0, 0, 10, 73, 68, 65, 84, 8, 215, 99, 96, 0, 0, 0, 2, 0, 1, 226,
    33, 188, 51, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Maximum time allowed for the closed hold worker to acknowledge that it
/// reached its wait point.
const HOLD_READY_TIMEOUT: Duration = Duration::from_secs(1);
/// Hard deadline for the closed hold mode before it returns a terminal error.
const HOLD_TERMINAL_TIMEOUT_SECS: u64 = 10;
/// Hard deadline duration derived from [`HOLD_TERMINAL_TIMEOUT_SECS`].
const HOLD_TERMINAL_TIMEOUT: Duration = Duration::from_secs(HOLD_TERMINAL_TIMEOUT_SECS);
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestartMode {
    /// Preserve the historical random exit-or-error behavior.
    #[default]
    Random,
    /// Emit a successful tool result without restarting the extension.
    Success,
    /// Emit the same tool error as the historical failure branch.
    Error,
    /// Exit without replying to the tool invocation.
    Exit,
    /// Acknowledge the invocation and wait without performing side effects.
    HoldNoSideEffect,
    /// Wait for an authenticated fixture-private socket release.
    HoldUntilSuccessRelease,
    /// Exit once after a fixture-private atomic marker claim, then succeed.
    ExitOnceThenSuccess,
}

/// Closed configuration accepted by the test-only dummy extension.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Test-only deterministic behavior for `restart_test_dummy`.
    restart_mode: Option<RestartMode>,
    /// Enables the separate fixed typed-image fixture tool.
    typed_image: bool,
    /// Enables the provider-context raw-message fixture tool.
    provider_context_raw_message: bool,
    /// Fixture-private Unix socket used by `hold_until_success_release`.
    release_socket_path: Option<PathBuf>,
    /// Fixture-generated nonce required by `hold_until_success_release`.
    release_nonce: Option<String>,
    /// Fixture-private atomic marker used by `exit_once_then_success`.
    exit_once_marker_path: Option<PathBuf>,
}

/// Runtime state for the dummy extension.
struct DummyState<T> {
    /// Random source used by the historical restart fixture mode.
    rng: T,
    /// Active deterministic restart behavior selected by config.
    restart_mode: RestartMode,
    /// Whether the separate typed-image fixture tool may return its fixed
    /// image.
    typed_image: bool,
    /// Whether the provider-context raw-message fixture tool is enabled.
    provider_context_raw_message: bool,
    /// Sole bounded invocation owned by either deterministic hold mode.
    pending_hold: Option<PendingHold>,
    /// Terminal deadline for the no-side-effect hold worker.
    hold_timeout: Duration,
    /// Validated fixture-private release configuration.
    release_config: Option<ReleaseConfig>,
    /// Validated fixture-private atomic exit-once marker.
    exit_once_marker_path: Option<PathBuf>,
    /// Worker-to-loop terminal outcomes.
    terminals: AsyncTerminals,
}

/// Cloneable worker-to-loop terminal publication adapter.
#[derive(Clone)]
struct TerminalSender {
    /// Unbounded outcome sender, independent of protocol FIFO capacity.
    sender: mpsc::Sender<PendingTerminal>,
    /// Manual runtime wake handle.
    waker: tau_client::ManualRuntimeWaker,
}

impl TerminalSender {
    /// Transfers one terminal outcome to the protocol loop.
    fn send(&self, terminal: tau_client::ToolTerminalOutcome) {
        let call_id = match &terminal {
            tau_client::ToolTerminalOutcome::Result(result) => result.call_id.clone(),
            tau_client::ToolTerminalOutcome::Failure(error) => error.call_id.clone(),
            tau_client::ToolTerminalOutcome::Cancelled(cancelled) => cancelled.call_id.clone(),
        };
        let _ = self.sender.send(PendingTerminal { call_id, terminal });
        self.waker.wake();
    }
}

/// Main-loop side of asynchronous terminal publication.
struct AsyncTerminals {
    /// Worker sender installed after manual-runtime startup.
    sender: Option<TerminalSender>,
    /// Sole receiver drained by the protocol loop.
    receiver: mpsc::Receiver<PendingTerminal>,
}

/// One worker terminal awaiting checked ordered publication.
struct PendingTerminal {
    /// Invocation retained in [`DummyState::pending_hold`].
    call_id: tau_proto::ToolCallId,
    /// Sole terminal selected by worker arbitration.
    terminal: tau_client::ToolTerminalOutcome,
}

/// One bounded deterministic hold worker.
enum PendingHold {
    /// Closed no-side-effect worker with its typed lifecycle channel.
    NoSideEffect {
        /// Correlation identity accepted by the worker.
        call_id: tau_proto::ToolCallId,
        /// Cancellation/shutdown signal owned by the reader loop.
        signal: mpsc::Sender<HoldSignal>,
        /// Worker joined on cancellation, disconnect, or state teardown.
        join: thread::JoinHandle<()>,
    },
    /// Authenticated fixture-private release worker.
    Release(release_hold::ReleaseHold),
}

/// Closed wake reasons accepted only by the no-side-effect worker.
enum HoldSignal {
    /// Emit one correlated cancellation terminal.
    Cancel,
    /// Exit without terminal output because the extension is shutting down.
    Shutdown,
}

impl<T> Drop for DummyState<T> {
    fn drop(&mut self) {
        self.shutdown_pending_hold();
    }
}

impl<T> DummyState<T> {
    /// Cancels and joins only the exactly correlated active hold.
    fn cancel_pending_hold(
        &mut self,
        target_call_id: &tau_proto::ToolCallId,
        handle: &tau_client::ClientHandle,
    ) -> ClientResult<()> {
        if self.pending_hold.as_ref().is_none_or(|hold| match hold {
            PendingHold::NoSideEffect { call_id, .. } => {
                call_id.as_str() != target_call_id.as_str()
            }
            PendingHold::Release(hold) => hold.call_id() != target_call_id,
        }) {
            return Ok(());
        }
        match self
            .pending_hold
            .as_ref()
            .expect("correlated hold remains present")
        {
            PendingHold::NoSideEffect { signal, .. } => {
                let _ = signal.send(HoldSignal::Cancel);
            }
            PendingHold::Release(hold) => {
                hold.cancel();
            }
        }
        let pending = self.terminals.receiver.recv().map_err(|_| {
            tau_client::ClientError::handler("cancelled hold produced no terminal outcome")
        })?;
        #[cfg(test)]
        saturate_detached_fifo_for_test(handle, &pending.call_id, self.pending_hold.is_some());
        handle.report_tool_terminal(pending.terminal)?;
        self.complete_pending_hold(&pending.call_id)
    }

    /// Removes and joins the hold only after its terminal flush succeeds.
    fn complete_pending_hold(&mut self, call_id: &tau_proto::ToolCallId) -> ClientResult<()> {
        if self.pending_hold.as_ref().is_none_or(|hold| match hold {
            PendingHold::NoSideEffect {
                call_id: active, ..
            } => active != call_id,
            PendingHold::Release(hold) => hold.call_id() != call_id,
        }) {
            return Err(tau_client::ClientError::handler(
                "terminal outcome did not own the active deterministic hold",
            ));
        }
        let hold = self.pending_hold.take().expect("matching hold remains");
        match hold {
            PendingHold::NoSideEffect { join, .. } => join.join().map_err(|_| {
                tau_client::ClientError::handler("deterministic hold worker panicked")
            }),
            PendingHold::Release(hold) => hold.join(),
        }
    }

    /// Stops and joins the active hold without emitting terminal tool output.
    fn shutdown_pending_hold(&mut self) {
        if let Some(hold) = self.pending_hold.take() {
            match hold {
                PendingHold::NoSideEffect { signal, join, .. } => {
                    let _ = signal.send(HoldSignal::Shutdown);
                    let _ = join.join();
                }
                PendingHold::Release(hold) => hold.shutdown(),
            }
        }
    }
}

/// Tau-client declaration for the dummy extension.
struct DummyExtension<T> {
    /// Marker carrying the state random-source type.
    _rng: PhantomData<fn() -> T>,
}

impl<T> Default for DummyExtension<T> {
    fn default() -> Self {
        Self { _rng: PhantomData }
    }
}

impl<T> TauExtension for DummyExtension<T>
where
    T: Rng,
{
    type State = DummyState<T>;

    fn name(&self) -> &'static str {
        "tau-ext-test-dummy"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .message_bridge()
            .configure::<ExtConfig>(|cx| {
                cx.state.restart_mode = cx.config().restart_mode.unwrap_or_default();
                let typed_image = cx.config().typed_image;
                if typed_image && !cx.state.typed_image {
                    cx.handle
                        .register_local_tool(tau_proto::ToolRegistrationDeclared {
                            tool: typed_image_tool_spec(),
                            tool_group: Some(tau_proto::ToolGroup {
                                name: tau_proto::ToolGroupName::new("test"),
                                prompt_fragment: None,
                            }),
                            prompt_fragment: None,
                        })?;
                } else if !typed_image && cx.state.typed_image {
                    cx.handle
                        .unregister_local_tool(tau_proto::ToolName::new(
                            TYPED_IMAGE_TEST_DUMMY_TOOL_NAME,
                        ))?;
                }
                cx.state.typed_image = typed_image;
                let provider_context_raw_message = cx.config().provider_context_raw_message;
                if provider_context_raw_message && !cx.state.provider_context_raw_message {
                    cx.handle.register_local_tool(
                        tau_proto::ToolRegistrationDeclared {
                            tool: provider_context_raw_message_tool_spec(),
                            tool_group: Some(tau_proto::ToolGroup {
                                name: tau_proto::ToolGroupName::new("test"),
                                prompt_fragment: None,
                            }),
                            prompt_fragment: None,
                        },
                    )?;
                } else if !provider_context_raw_message && cx.state.provider_context_raw_message {
                    cx.handle.unregister_local_tool(tau_proto::ToolName::new(
                        PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME,
                    ))?;
                }
                cx.state.provider_context_raw_message = provider_context_raw_message;
                if cx.state.restart_mode == RestartMode::HoldUntilSuccessRelease {
                    let Some(socket_path) = cx.config().release_socket_path.clone() else {
                        return Err(tau_client::ClientError::handler(
                            "hold_until_success_release requires release_socket_path and non-empty release_nonce",
                        ));
                    };
                    let Some(nonce) = cx
                        .config()
                        .release_nonce
                        .clone()
                        .filter(|nonce| !nonce.is_empty())
                    else {
                        return Err(tau_client::ClientError::handler(
                            "hold_until_success_release requires release_socket_path and non-empty release_nonce",
                        ));
                    };
                    cx.state.release_config = Some(ReleaseConfig::new(socket_path, nonce)?);
                } else if cx.config().release_socket_path.is_some() || cx.config().release_nonce.is_some()
                {
                    return Err(tau_client::ClientError::handler(
                        "release_socket_path and release_nonce are only valid for hold_until_success_release",
                    ));
                }
                if cx.state.restart_mode == RestartMode::ExitOnceThenSuccess {
                    let Some(marker_path) = cx.config().exit_once_marker_path.clone() else {
                        return Err(tau_client::ClientError::handler(
                            "exit_once_then_success requires exit_once_marker_path",
                        ));
                    };
                    if !marker_path.is_absolute() {
                        return Err(tau_client::ClientError::handler(
                            "exit_once_then_success requires an absolute exit_once_marker_path",
                        ));
                    }
                    if marker_path
                        .symlink_metadata()
                        .is_ok_and(|metadata| !metadata.file_type().is_file())
                    {
                        return Err(tau_client::ClientError::handler(
                            "exit_once_then_success marker must be absent or a regular file",
                        ));
                    }
                    cx.state.exit_once_marker_path = Some(marker_path);
                } else if cx.config().exit_once_marker_path.is_some() {
                    return Err(tau_client::ClientError::handler(
                        "exit_once_marker_path is only valid for exit_once_then_success",
                    ));
                }
                Ok(())
            })
            .on_output_message(|message, state, _| {
                if matches!(message, tau_proto::HarnessOutputMessage::Disconnect(_)) {
                    state.shutdown_pending_hold();
                }
                Ok(())
            })
            .on_raw_routed_live(
                EventSelector::Exact(tau_proto::EventName::TOOL_CANCEL_REQUEST),
                |cx| {
                    let Event::ToolCancelRequest(cancel) = cx.delivery.event() else {
                        return Ok(());
                    };
                    let handle = cx.handle();
                    cx.state
                        .cancel_pending_hold(&cancel.target_call_id, &handle)
                },
            )
            .intercept(
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                InterceptionPriority::new(0),
                |cx| {
                    let replacement = intercepted_prompt_replacement(cx.event());
                    match replacement {
                        Some(event) => {
                            cx.handle().request_notice(
                                "did you mean \"Tau\"? — corrected for you",
                                NoticeLevel::Info,
                            )?;
                            Ok(InterceptDecision::replace(event))
                        }
                        None => Ok(InterceptDecision::Pass),
                    }
                },
            )
            .tool_with_group_and_prompt_fragment(
                restart_tool_spec(),
                Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new("test"),
                    prompt_fragment: None,
                }),
                None,
                |mut cx| handle_restart_invocation(&mut cx),
            )
            .on::<tau_proto::ToolStarted>(|cx| {
                handle_provider_context_raw_message(cx.state, cx.event, &cx.handle)?;
                handle_typed_image_invocation(cx.state, cx.event, &cx.handle)
            })
            .ready_message("test dummy tools ready");
    }
}

/// Returns a copy of `text` with every case-insensitive "tao" word
/// rewritten to "tau", preserving the original casing letter-by-letter
/// (so `Tao` → `Tau`, `TAO` → `TAU`, `tAo` → `tAu`). Returns `None` if
/// no replacement happened so the caller can short-circuit and reply
/// with `Pass(None)` rather than re-publish an identical event.
///
/// Only ASCII letters form word boundaries for this test fixture. `"tao"` is
/// matched as a whole word, not as a free-floating substring — the `tao` inside
/// `taoism` is left alone.
fn correct_tao_to_tau(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let is_match = i + 3 <= bytes.len()
            && bytes[i].eq_ignore_ascii_case(&b't')
            && bytes[i + 1].eq_ignore_ascii_case(&b'a')
            && bytes[i + 2].eq_ignore_ascii_case(&b'o')
            && !preceded_by_letter(bytes, i)
            && !followed_by_letter(bytes, i + 3);
        if is_match {
            out.push(bytes[i] as char);
            out.push(bytes[i + 1] as char);
            // Replace 'o'/'O' → 'u'/'U' matching the original case.
            out.push(if bytes[i + 2].is_ascii_uppercase() {
                'U'
            } else {
                'u'
            });
            i += 3;
            changed = true;
        } else {
            // Cheap path for ASCII; fall back to a char step at the
            // current byte boundary to stay UTF-8-safe.
            let ch = text[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    changed.then_some(out)
}

fn preceded_by_letter(bytes: &[u8], i: usize) -> bool {
    0 < i && bytes[i - 1].is_ascii_alphabetic()
}

fn followed_by_letter(bytes: &[u8], i: usize) -> bool {
    bytes.get(i).is_some_and(|b| b.is_ascii_alphabetic())
}

/// Runs the dummy extension on standard input and standard output.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for("tau_ext_test_dummy");
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the dummy extension over the supplied harness protocol streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    T: Rng,
{
    run_with_rng_and_hold_timeout(reader, writer, rng, HOLD_TERMINAL_TIMEOUT)
}

fn run_with_rng_and_hold_timeout<R, W, T>(
    reader: R,
    writer: W,
    rng: &mut T,
    hold_timeout: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    T: Rng,
{
    let (terminal_tx, terminal_rx) = mpsc::channel();
    let state = DummyState {
        rng,
        restart_mode: RestartMode::Random,
        typed_image: false,
        provider_context_raw_message: false,
        pending_hold: None,
        hold_timeout,
        release_config: None,
        exit_once_marker_path: None,
        terminals: AsyncTerminals {
            sender: None,
            receiver: terminal_rx,
        },
    };
    let mut runtime = match TauExtensionRunner::new(DummyExtension::<&mut T>::default())
        .start_manual_loop(reader, writer, state)
    {
        Ok(runtime) => runtime,
        Err(tau_client::ClientError::InitialConfigureRejected) => return Ok(()),
        Err(error) => return Err(Box::new(error)),
    };
    runtime.state_mut().terminals.sender = Some(TerminalSender {
        sender: terminal_tx,
        waker: runtime.waker(),
    });
    tracing::info!(target: "tau_ext_test_dummy", "test dummy configured");
    let loop_result = run_dummy_loop(&mut runtime);
    runtime.state_mut().shutdown_pending_hold();
    match loop_result {
        Ok(DummyLoopExit::Disconnect) => {
            let _ = runtime.finish_detached();
            Ok(())
        }
        Ok(DummyLoopExit::Graceful) => runtime
            .finish()
            .map(|_| ())
            .map_err(|error| Box::new(error) as Box<dyn Error>),
        Err(error) => {
            let _ = runtime.finish();
            Err(Box::new(error))
        }
    }
}

/// Reason the manual runtime stopped.
enum DummyLoopExit {
    /// Harness sent a normal protocol disconnect.
    Disconnect,
    /// Input closed or a handler requested graceful stop.
    Graceful,
}

/// Dispatches harness input and serializes worker terminals through checked
/// output.
fn run_dummy_loop<T>(
    runtime: &mut tau_client::ManualExtensionRuntime<DummyState<T>>,
) -> ClientResult<DummyLoopExit> {
    loop {
        while let Ok(pending) = runtime.state().terminals.receiver.try_recv() {
            #[cfg(test)]
            let retained = runtime.state().pending_hold.is_some();
            #[cfg(test)]
            saturate_detached_fifo_for_test(&runtime.handle(), &pending.call_id, retained);
            runtime.handle().report_tool_terminal(pending.terminal)?;
            runtime
                .state_mut()
                .complete_pending_hold(&pending.call_id)?;
        }
        match runtime.try_recv()? {
            tau_client::ManualRuntimePoll::Message(message) => {
                match runtime.dispatch_one(message)? {
                    tau_client::DispatchOutcome::Continue => {}
                    tau_client::DispatchOutcome::StopRequested => {
                        return Ok(DummyLoopExit::Graceful);
                    }
                    tau_client::DispatchOutcome::Disconnect(_) => {
                        return Ok(DummyLoopExit::Disconnect);
                    }
                }
            }
            tau_client::ManualRuntimePoll::InputClosed => return Ok(DummyLoopExit::Graceful),
            tau_client::ManualRuntimePoll::Empty => runtime.wait_for_wake(),
        }
    }
}

/// Exhausts the real detached FIFO immediately before worker terminal output.
#[cfg(test)]
fn saturate_detached_fifo_for_test(
    handle: &tau_client::ClientHandle,
    call_id: &tau_proto::ToolCallId,
    ownership_retained: bool,
) {
    let hook = SATURATION_HOOK
        .lock()
        .expect("dummy saturation hook")
        .clone();
    let Some((hook_call_id, notify)) = hook else {
        return;
    };
    if hook_call_id != *call_id {
        return;
    };
    for _ in 0..96 {
        match handle.emit_transient_detached(Event::TermBell(tau_proto::TermBell {})) {
            Err(tau_client::ClientError::Overloaded) => {
                let _ = notify.send(ownership_retained);
                return;
            }
            Ok(()) => {}
            Err(_) => return,
        }
    }
}

fn restart_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Test-only tool that restarts the dummy extension, returns an error, or follows configured restart_mode"
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

/// Returns the image-capable tool declaration reserved for one E2E fixture.
fn typed_image_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(TYPED_IMAGE_TEST_DUMMY_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Test-only tool that returns one fixed typed image in typed_image mode".to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new("provider-content:image")],
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

fn provider_context_raw_message_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Test-only tool that publishes one raw message fact to an explicit agent".to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["agent_id", "text"],
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Never),
        examples: Vec::new(),
    }
}
fn intercepted_prompt_replacement(event: &Event) -> Option<Event> {
    match event {
        Event::AgentPromptSubmitted(prompt) => correct_tao_to_tau(&prompt.text).map(|fixed| {
            Event::AgentPromptSubmitted(AgentPromptSubmitted {
                inference_activation: false,
                text: fixed,
                trusted_internal_spans: Vec::new(),
                ..prompt.clone()
            })
        }),
        _ => None,
    }
}

fn handle_restart_invocation<T>(
    cx: &mut tau_client::ToolContext<'_, DummyState<T>>,
) -> ClientResult<()>
where
    T: Rng,
{
    let invoke = cx.invoke().clone();
    match cx.state.restart_mode {
        RestartMode::Random if cx.state.rng.gen_bool(0.5) => {
            cx.request_stop();
            Ok(())
        }
        RestartMode::Exit => {
            cx.request_stop();
            Ok(())
        }
        RestartMode::Random | RestartMode::Error => cx.report_error(restart_error(invoke)),
        RestartMode::Success => cx.report_result(restart_success(invoke)),
        RestartMode::HoldNoSideEffect => start_no_side_effect_hold(cx, invoke),
        RestartMode::HoldUntilSuccessRelease => start_success_release_hold(cx, invoke),
        RestartMode::ExitOnceThenSuccess => exit_once_then_success(cx, invoke),
    }
}

fn handle_provider_context_raw_message<T>(
    state: &DummyState<T>,
    invoke: &tau_proto::ToolStarted,
    handle: &tau_client::ClientHandle,
) -> ClientResult<()>
where
    T: Rng,
{
    let expected_name = handle
        .tool_name_scope()?
        .wire_tool_name(&tau_proto::ToolName::new(
            PROVIDER_CONTEXT_RAW_MESSAGE_TOOL_NAME,
        ))?;
    if !state.provider_context_raw_message || invoke.tool_name != expected_name {
        return Ok(());
    }
    let CborValue::Map(fields) = &invoke.arguments else {
        return Err(tau_client::ClientError::handler(
            "provider-context raw message arguments must be a map",
        ));
    };
    let text_field = |name: &str| {
        fields.iter().find_map(|(key, value)| {
            if matches!(key, CborValue::Text(key) if key == name)
                && let CborValue::Text(value) = value
            {
                Some(value.clone())
            } else {
                None
            }
        })
    };
    let agent_id = text_field("agent_id").ok_or_else(|| {
        tau_client::ClientError::handler("provider-context raw message omitted agent_id")
    })?;
    let text = text_field("text").ok_or_else(|| {
        tau_client::ClientError::handler("provider-context raw message omitted text")
    })?;
    handle.emit(Event::MessageDeliveredReported(MessageDelivered::new(
        RawMessagePublisherId::new("e2e-test-dummy"),
        MessageAgentTarget::new(agent_id),
        MessageFactId::new("provider-context-raw-message"),
        MessageParty {
            stable_id: "provider-context-raw-sender".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        text,
    )))?;
    handle.report_tool_result(ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("raw message emitted".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: invoke.originator.clone(),
    })
}

/// Emits correlated observation progress, atomically claims the configured
/// marker, and exits once or reports the ordinary deterministic success.
fn exit_once_then_success<T>(
    cx: &mut tau_client::ToolContext<'_, DummyState<T>>,
    invoke: tau_proto::ToolStarted,
) -> ClientResult<()>
where
    T: Rng,
{
    cx.handle().report_tool_progress(tau_proto::ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: Some("exit_once_then_success ready".to_owned()),
        progress: None,
        display: None,
    })?;
    let marker_path = cx
        .state
        .exit_once_marker_path
        .as_ref()
        .expect("exit-once marker configuration validated");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_path)
    {
        Ok(_) => {
            cx.request_stop();
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = marker_path.symlink_metadata().map_err(|metadata_error| {
                tau_client::ClientError::handler(format!(
                    "exit_once_then_success marker could not be checked after existing claim: {metadata_error}"
                ))
            })?;
            if !metadata.file_type().is_file() {
                return Err(tau_client::ClientError::handler(
                    "exit_once_then_success marker must be a regular file",
                ));
            }
            cx.report_result(restart_success(invoke))
        }
        Err(error) => Err(tau_client::ClientError::handler(format!(
            "exit_once_then_success marker claim failed: {error}"
        ))),
    }
}

/// Dispatches the separate closed fixed-image fixture tool.
fn handle_typed_image_invocation<T>(
    state: &DummyState<T>,
    invoke: &tau_proto::ToolStarted,
    handle: &tau_client::ClientHandle,
) -> ClientResult<()>
where
    T: Rng,
{
    let expected_tool_name = handle
        .tool_name_scope()?
        .wire_tool_name(&tau_proto::ToolName::new(TYPED_IMAGE_TEST_DUMMY_TOOL_NAME))?;
    if state.typed_image && invoke.tool_name == expected_tool_name {
        handle.report_tool_result(typed_image_success(invoke.clone()))
    } else {
        Ok(())
    }
}

/// Starts one authenticated Unix-socket release worker.
fn start_success_release_hold<T>(
    cx: &mut tau_client::ToolContext<'_, DummyState<T>>,
    invoke: tau_proto::ToolStarted,
) -> ClientResult<()>
where
    T: Rng,
{
    if cx.state.pending_hold.is_some() {
        return cx.report_error(ToolError {
            presentation: Default::default(),
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            message: "a deterministic hold already has an active invocation".to_owned(),
            details: None,
            originator: invoke.originator,
            display: None,
        });
    }
    let call_id = invoke.call_id.clone();
    let tool_name = invoke.tool_name.clone();
    let hold = release_hold::ReleaseHold::start(
        cx.state
            .release_config
            .clone()
            .expect("release mode configuration validated"),
        invoke,
        cx.state
            .terminals
            .sender
            .clone()
            .expect("manual runtime terminal sender installed"),
    )?;
    cx.state.pending_hold = Some(PendingHold::Release(hold));
    if let Err(error) = cx
        .handle()
        .report_tool_progress_detached(tau_proto::ToolProgress {
            call_id,
            tool_name,
            message: Some("hold_until_success_release ready".to_owned()),
            progress: None,
            display: None,
        })
    {
        cx.state.shutdown_pending_hold();
        return Err(error);
    }
    if let Some(PendingHold::Release(hold)) = &cx.state.pending_hold {
        hold.arm();
    }
    Ok(())
}

/// Starts the sole closed hold worker and publishes its correlated readiness
/// only after the worker reaches the bounded wait point.
fn start_no_side_effect_hold<T>(
    cx: &mut tau_client::ToolContext<'_, DummyState<T>>,
    invoke: tau_proto::ToolStarted,
) -> ClientResult<()>
where
    T: Rng,
{
    if cx.state.pending_hold.is_some() {
        return cx.report_error(ToolError {
            presentation: Default::default(),
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            message: "hold_no_side_effect already has an active invocation".to_owned(),
            details: None,
            originator: invoke.originator,
            display: None,
        });
    }

    let call_id = invoke.call_id.clone();
    let tool_name = invoke.tool_name.clone();
    let hold_timeout = cx.state.hold_timeout;
    let (signal, signals) = mpsc::channel();
    let (ready, readiness) = mpsc::channel();
    let terminals = cx
        .state
        .terminals
        .sender
        .clone()
        .expect("manual runtime terminal sender installed");
    let join = thread::spawn(move || {
        if ready.send(()).is_err() {
            return;
        }
        match signals.recv_timeout(hold_timeout) {
            Ok(HoldSignal::Cancel) => {
                terminals.send(
                    tau_proto::ToolCancelled {
                        presentation: Default::default(),
                        call_id: invoke.call_id,
                        tool_name: invoke.tool_name,
                        tool_type: tau_proto::ToolType::Function,
                        display: None,
                    }
                    .into(),
                );
            }
            Ok(HoldSignal::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                terminals.send(ToolError {
                    presentation: Default::default(),
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    message: format!(
                        "hold_no_side_effect reached its {HOLD_TERMINAL_TIMEOUT_SECS} second deadline"
                    ),
                    details: None,
                    originator: invoke.originator,
                    display: None,
                }.into());
            }
        }
    });
    if let Err(error) = readiness.recv_timeout(HOLD_READY_TIMEOUT) {
        let _ = signal.send(HoldSignal::Shutdown);
        let _ = join.join();
        return Err(tau_client::ClientError::handler(format!(
            "hold_no_side_effect worker did not become ready: {error}"
        )));
    }
    cx.state.pending_hold = Some(PendingHold::NoSideEffect {
        call_id: call_id.clone(),
        signal,
        join,
    });
    if let Err(error) = cx
        .handle()
        .report_tool_progress_detached(tau_proto::ToolProgress {
            call_id,
            tool_name,
            message: Some("hold_no_side_effect ready".to_owned()),
            progress: None,
            display: None,
        })
    {
        cx.state.shutdown_pending_hold();
        return Err(error);
    }
    Ok(())
}

fn restart_success(invoke: tau_proto::ToolStarted) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("restart succeeded".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: invoke.originator,
    }
}

/// Returns the fixed image-bearing terminal used only by deterministic E2E.
fn typed_image_success(invoke: tau_proto::ToolStarted) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("typed image succeeded".to_owned()),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: TYPED_IMAGE_PNG.to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
        kind: ToolResultKind::Final,
        display: None,
        originator: invoke.originator,
    }
}

fn restart_error(invoke: tau_proto::ToolStarted) -> ToolError {
    ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: "restarting failed".to_owned(),
        details: None,
        display: None,
        originator: invoke.originator,
    }
}

#[cfg(test)]
mod tests;
