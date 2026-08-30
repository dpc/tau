//! Checked mandatory and detached optional harness output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use tau_client::{ClientError, ClientHandle, ClientResult, ManualRuntimeWaker};
use tau_proto::{Event, HarnessInputMessage, NoticeLevel, ToolProgress};

#[cfg(test)]
pub(crate) static SATURATION_HOOK: Mutex<Option<(String, mpsc::Sender<()>)>> = Mutex::new(None);

#[cfg(test)]
/// Outcome observed when the selected terminal reaches its test gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolTerminalGateOutcome {
    /// The selected call completed successfully.
    Success,
    /// The selected call completed with an error.
    Error,
    /// The selected call was cancelled.
    Cancelled,
}

#[cfg(test)]
/// Lifecycle event distinguishing gate arrival from an early dispatch exit.
pub(crate) enum ToolTerminalGateEvent {
    /// The selected terminal reached the gate.
    Reached(ToolTerminalGateOutcome),
    /// Dispatch exited before reaching the selected terminal gate.
    DispatchFinished,
}

#[cfg(test)]
/// Per-output gate that forces an asynchronous publication ahead of one tool
/// terminal.
struct ToolTerminalGate {
    /// Tool-call correlation selected by the test.
    call_id: String,
    /// One-shot notification that terminal publication is paused.
    lifecycle: mpsc::Sender<ToolTerminalGateEvent>,
    /// One-shot release for the paused terminal publication.
    release: Mutex<mpsc::Receiver<()>>,
}

/// Sticky mandatory-output failure that wakes the owning manual protocol loop.
struct OutputFailure {
    /// Whether a mandatory publication has failed.
    failed: AtomicBool,
    /// Manual-loop wake handle installed after startup.
    waker: Mutex<Option<ManualRuntimeWaker>>,
}

impl OutputFailure {
    /// Create an unset output-failure signal.
    fn new() -> Self {
        Self {
            failed: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    /// Install the manual runtime wake handle.
    fn install_waker(&self, waker: ManualRuntimeWaker) {
        *self.waker.lock().expect("output failure waker lock") = Some(waker);
    }

    /// Record mandatory output failure and wake the protocol loop.
    fn report(&self) {
        self.failed.store(true, Ordering::Release);
        if let Some(waker) = &self
            .waker
            .lock()
            .expect("output failure waker lock")
            .as_ref()
        {
            waker.wake();
        }
    }

    /// Return an error after any mandatory publication failure.
    fn check(&self) -> ClientResult<()> {
        if self.failed.load(Ordering::Acquire) {
            Err(ClientError::handler(
                "telegram mandatory output publication failed",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
/// Harness output facade that checks ownership-defining publications.
pub(crate) struct Output {
    /// Private transport and sticky-failure representation.
    kind: OutputKind,
    #[cfg(test)]
    /// Optional deterministic gate for output-ordering regressions.
    tool_terminal_gate: Arc<Mutex<Option<Arc<ToolTerminalGate>>>>,
}

/// Private representation keeps failure signaling inside this subsystem.
#[derive(Clone)]
enum OutputKind {
    /// Test-side output channel preserving existing direct unit-test helpers.
    Channel(mpsc::Sender<HarnessInputMessage>),
    /// Tau-client output handle used by the protocol runtime.
    Client {
        /// Ordered checked protocol output.
        handle: ClientHandle,
        /// Failure signal observed by the manual protocol loop.
        failure: Arc<OutputFailure>,
    },
}

impl From<mpsc::Sender<HarnessInputMessage>> for Output {
    fn from(tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            kind: OutputKind::Channel(tx),
            #[cfg(test)]
            tool_terminal_gate: Arc::new(Mutex::new(None)),
        }
    }
}

impl From<ClientHandle> for Output {
    fn from(handle: ClientHandle) -> Self {
        Self {
            kind: OutputKind::Client {
                handle,
                failure: Arc::new(OutputFailure::new()),
            },
            #[cfg(test)]
            tool_terminal_gate: Arc::new(Mutex::new(None)),
        }
    }
}

impl Output {
    /// Pause the selected tool terminal until a test explicitly releases it.
    #[cfg(test)]
    pub(crate) fn gate_tool_terminal_for_test(
        &self,
        call_id: &str,
    ) -> (
        mpsc::Receiver<ToolTerminalGateEvent>,
        mpsc::Sender<ToolTerminalGateEvent>,
        mpsc::Sender<()>,
    ) {
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let gate = Arc::new(ToolTerminalGate {
            call_id: call_id.to_owned(),
            lifecycle: lifecycle_tx.clone(),
            release: Mutex::new(release_rx),
        });
        *self
            .tool_terminal_gate
            .lock()
            .expect("tool terminal gate lock") = Some(Arc::clone(&gate));
        (lifecycle_rx, lifecycle_tx, release_tx)
    }

    /// Exhaust the real detached FIFO at a correlated mandatory-output
    /// boundary.
    #[cfg(test)]
    fn saturate_for_test(&self, correlation: &str) {
        let hook = SATURATION_HOOK
            .lock()
            .expect("telegram saturation hook")
            .clone();
        let Some((expected, saturated)) = hook else {
            return;
        };
        if expected != correlation {
            return;
        }
        let OutputKind::Client { handle, .. } = &self.kind else {
            return;
        };
        for _ in 0..96 {
            match handle.emit_transient_detached(Event::TermBell(tau_proto::TermBell {})) {
                Err(ClientError::Overloaded) => {
                    let _ = saturated.send(());
                    return;
                }
                Ok(()) => {}
                Err(_) => return,
            }
        }
    }

    /// Sends one protocol frame, intentionally ignoring closed-writer failures.
    ///
    /// Telegram poller and tool output is best-effort once the harness has
    /// disconnected or the tau-client writer has shut down.
    fn send(&self, message: HarnessInputMessage) {
        match &self.kind {
            OutputKind::Channel(tx) => {
                let _ = tx.send(message);
            }
            OutputKind::Client { handle, .. } => {
                let _ = handle.send_detached(message);
            }
        }
    }

    /// Submit an optional extension notice.
    pub(crate) fn request_notice(&self, message: impl Into<String>, level: NoticeLevel) {
        self.send(HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: message.into(),
                level,
            },
        ));
    }

    /// Submit optional tool progress.
    pub(crate) fn report_tool_progress(&self, progress: ToolProgress) {
        self.send(HarnessInputMessage::emit_with_persist(
            Event::ToolProgressReported(progress),
            false,
        ));
    }

    /// Submit one terminal tool report through the typed client helper or the
    /// equivalent explicit transient channel frame.
    pub(crate) fn report_tool_terminal(&self, event: Event) -> ClientResult<()> {
        #[cfg(test)]
        {
            let (call_id, outcome) = match &event {
                Event::ToolResult(result) => (
                    Some(result.call_id.as_str()),
                    ToolTerminalGateOutcome::Success,
                ),
                Event::ToolError(error) => {
                    (Some(error.call_id.as_str()), ToolTerminalGateOutcome::Error)
                }
                Event::ToolCancelled(cancelled) => (
                    Some(cancelled.call_id.as_str()),
                    ToolTerminalGateOutcome::Cancelled,
                ),
                _ => (None, ToolTerminalGateOutcome::Error),
            };
            let gate = self
                .tool_terminal_gate
                .lock()
                .expect("tool terminal gate lock")
                .take_if(|gate| Some(gate.call_id.as_str()) == call_id);
            if let Some(gate) = gate {
                let _ = gate.lifecycle.send(ToolTerminalGateEvent::Reached(outcome));
                let _ = gate
                    .release
                    .lock()
                    .expect("tool terminal release lock")
                    .recv();
            }
        }
        #[cfg(test)]
        if let Event::ToolResult(result) = &event {
            self.saturate_for_test(result.call_id.as_str());
        } else if let Event::ToolError(error) = &event {
            self.saturate_for_test(error.call_id.as_str());
        } else if let Event::ToolCancelled(cancelled) = &event {
            self.saturate_for_test(cancelled.call_id.as_str());
        }
        let outcome = match tau_client::ToolTerminalOutcome::try_from(event) {
            Ok(outcome) => outcome,
            Err(event) => {
                return Err(ClientError::handler(format!(
                    "telegram tool returned non-terminal event {}",
                    event.name()
                )));
            }
        };
        let result = match &self.kind {
            OutputKind::Client { handle, .. } => handle.report_tool_terminal(outcome),
            OutputKind::Channel(tx) => tx
                .send(HarnessInputMessage::emit_with_persist(
                    outcome.into_reported_event(),
                    false,
                ))
                .map_err(|_| ClientError::WriterClosed),
        };
        self.record_mandatory_result(result)
    }

    /// Record failure for the manual runtime while preserving the original
    /// result.
    fn record_mandatory_result(&self, result: ClientResult<()>) -> ClientResult<()> {
        if result.is_err()
            && let OutputKind::Client { failure, .. } = &self.kind
        {
            failure.report();
        }
        result
    }

    /// Publish a checked configuration error and latch writer failure.
    pub(crate) fn config_error(&self, message: String) -> ClientResult<()> {
        let result = match &self.kind {
            OutputKind::Client { handle, .. } => handle.config_error(message),
            OutputKind::Channel(_) => Ok(()),
        };
        self.record_mandatory_result(result)
    }

    /// Latch writer-side failures produced by tau-client before caller
    /// dispatch.
    pub(crate) fn observe_client_error(&self, error: &ClientError) {
        let writer_failure = matches!(
            error,
            ClientError::WriterClosed | ClientError::WriterPanicked | ClientError::Encode(_)
        );
        if writer_failure && let OutputKind::Client { failure, .. } = &self.kind {
            failure.report();
        }
    }

    /// Latch a checked writer failure from startup declarations, whose caller
    /// knows the failed operation was mandatory.
    pub(crate) fn report_known_mandatory_failure(&self) {
        if let OutputKind::Client { failure, .. } = &self.kind {
            failure.report();
        }
    }

    /// Classify errors returned by `ManualExtensionRuntime::try_recv`.
    ///
    /// In this call path, `Handler` can only come from tau-client's immutable
    /// Configure rejection after its checked ConfigError write failed.
    pub(crate) fn observe_pre_dispatch_error(&self, error: &ClientError) {
        if matches!(error, ClientError::Handler(_)) {
            self.report_known_mandatory_failure();
        } else {
            self.observe_client_error(error);
        }
    }

    /// Emit one transient external-message report for downstream
    /// canonicalization.
    pub(crate) fn emit_message_report(&self, event: Event) -> ClientResult<()> {
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(event.is_message_report());
        #[cfg(test)]
        match &event {
            Event::MessageDeliveredReported(report) => {
                self.saturate_for_test(report.message_id.as_str());
            }
            Event::MessageSentReported(report) => {
                self.saturate_for_test(report.message_id.as_str());
            }
            _ => {}
        }
        let message = HarnessInputMessage::emit_with_persist(event, false);
        let result = match &self.kind {
            OutputKind::Channel(tx) => tx.send(message).map_err(|_| ClientError::WriterClosed),
            OutputKind::Client { handle, .. } => handle.send(message),
        };
        self.record_mandatory_result(result)
    }

    /// Install the wake handle used to retire the protocol loop on worker
    /// failure.
    pub(crate) fn install_waker(&self, waker: ManualRuntimeWaker) {
        if let OutputKind::Client { failure, .. } = &self.kind {
            failure.install_waker(waker);
        }
    }

    /// Fail the protocol loop after a worker-side mandatory publication error.
    pub(crate) fn check_mandatory_output(&self) -> ClientResult<()> {
        match &self.kind {
            OutputKind::Channel(_) => Ok(()),
            OutputKind::Client { failure, .. } => failure.check(),
        }
    }
}
