//! Test-only Tau extension used by harness integration tests.
//!
//! The extension registers the [`RESTART_TEST_DUMMY_TOOL_NAME`] fixture tool
//! and an `agent.prompt_submitted` interceptor. It deliberately has no user
//! facing production role; its behavior exists to exercise extension
//! supervision, tool dispatch, replay suppression, and prompt interception.
//! See `ARCH-tau-ext-test-dummy` for the fixture boundary and invariants.

mod release_hold;

use std::error::Error;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::PathBuf;
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
    AgentPromptSubmitted, Event, EventSelector, InterceptionPriority, NoticeLevel, ToolError,
    ToolResult, ToolResultKind, ToolSpec,
};

/// Tool name registered by this fixture extension for restart-supervision
/// tests.
pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

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
}

/// Closed configuration accepted by the test-only dummy extension.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Test-only deterministic behavior for `restart_test_dummy`.
    restart_mode: Option<RestartMode>,
    /// Fixture-private Unix socket used by `hold_until_success_release`.
    release_socket_path: Option<PathBuf>,
    /// Fixture-generated nonce required by `hold_until_success_release`.
    release_nonce: Option<String>,
}

/// Runtime state for the dummy extension.
struct DummyState<T> {
    /// Random source used by the historical restart fixture mode.
    rng: T,
    /// Active deterministic restart behavior selected by config.
    restart_mode: RestartMode,
    /// Sole bounded invocation owned by either deterministic hold mode.
    pending_hold: Option<PendingHold>,
    /// Terminal deadline for the no-side-effect hold worker.
    hold_timeout: Duration,
    /// Validated fixture-private release configuration.
    release_config: Option<ReleaseConfig>,
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
    /// Reaps any naturally completed hold worker before another invocation.
    fn reap_finished_hold(&mut self) -> ClientResult<()> {
        if self.pending_hold.as_ref().is_some_and(|hold| match hold {
            PendingHold::NoSideEffect { join, .. } => join.is_finished(),
            PendingHold::Release(hold) => hold.is_finished(),
        }) {
            let hold = self
                .pending_hold
                .take()
                .expect("finished hold remains present");
            match hold {
                PendingHold::NoSideEffect { join, .. } => join.join().map_err(|_| {
                    tau_client::ClientError::handler("deterministic hold worker panicked")
                })?,
                PendingHold::Release(hold) => hold.join()?,
            }
        }
        Ok(())
    }

    /// Cancels and joins only the exactly correlated active hold.
    fn cancel_pending_hold(&mut self, target_call_id: &tau_proto::ToolCallId) -> ClientResult<()> {
        if self.pending_hold.as_ref().is_none_or(|hold| match hold {
            PendingHold::NoSideEffect { call_id, .. } => {
                call_id.as_str() != target_call_id.as_str()
            }
            PendingHold::Release(hold) => hold.call_id() != target_call_id,
        }) {
            return Ok(());
        }
        let hold = self
            .pending_hold
            .take()
            .expect("correlated hold remains present");
        match hold {
            PendingHold::NoSideEffect { signal, join, .. } => {
                let _ = signal.send(HoldSignal::Cancel);
                join.join().map_err(|_| {
                    tau_client::ClientError::handler("deterministic hold worker panicked")
                })
            }
            PendingHold::Release(hold) => hold.cancel(),
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
            .configure::<ExtConfig>(|cx| {
                cx.state.restart_mode = cx.config().restart_mode.unwrap_or_default();
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
                    cx.state.cancel_pending_hold(&cancel.target_call_id)
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
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the dummy extension over the supplied harness protocol streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send,
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
    R: Read,
    W: Write + Send,
    T: Rng,
{
    let state = DummyState {
        rng,
        restart_mode: RestartMode::Random,
        pending_hold: None,
        hold_timeout,
        release_config: None,
    };
    TauExtensionRunner::new(DummyExtension::<&mut T>::default()).run(reader, writer, state)?;
    Ok(())
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
    cx.state.reap_finished_hold()?;
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
        cx.handle(),
    )?;
    cx.state.pending_hold = Some(PendingHold::Release(hold));
    if let Err(error) = cx.handle().report_tool_progress(tau_proto::ToolProgress {
        call_id,
        tool_name,
        message: Some("hold_until_success_release ready".to_owned()),
        progress: None,
        display: None,
    }) {
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
    cx.state.reap_finished_hold()?;
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
    let handle = cx.handle();
    let join = thread::spawn(move || {
        if ready.send(()).is_err() {
            return;
        }
        match signals.recv_timeout(hold_timeout) {
            Ok(HoldSignal::Cancel) => {
                let _ = handle.report_tool_cancelled_detached(tau_proto::ToolCancelled {
                    presentation: Default::default(),
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                });
            }
            Ok(HoldSignal::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = handle.report_tool_error_detached(ToolError {
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
                });
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
    if let Err(error) = cx.handle().report_tool_progress(tau_proto::ToolProgress {
        call_id,
        tool_name,
        message: Some("hold_no_side_effect ready".to_owned()),
        progress: None,
        display: None,
    }) {
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
