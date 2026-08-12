//! Checked mandatory and detached optional harness output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use tau_client::{ClientError, ClientHandle, ClientResult, ManualRuntimeWaker};
use tau_proto::{Event, HarnessInputMessage, ToolProgress};

#[cfg(test)]
pub(crate) static SATURATION_HOOK: Mutex<Option<(String, mpsc::Sender<()>)>> = Mutex::new(None);

/// Sticky mandatory-output failure that wakes the owning manual protocol loop.
struct OutputFailure {
    /// Whether checked mandatory publication failed.
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

    /// Install the protocol-loop wake handle.
    fn install_waker(&self, waker: ManualRuntimeWaker) {
        *self.waker.lock().unwrap_or_else(|error| error.into_inner()) = Some(waker);
    }

    /// Record mandatory output failure and wake the protocol loop.
    fn report(&self) {
        self.failed.store(true, Ordering::Release);
        if let Some(waker) = &self
            .waker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            waker.wake();
        }
    }

    /// Return an error once mandatory output has failed.
    fn check(&self) -> ClientResult<()> {
        if self.failed.load(Ordering::Acquire) {
            Err(ClientError::handler(
                "xmpp mandatory output publication failed",
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
        /// Failure signal observed by the manual loop.
        failure: Arc<OutputFailure>,
    },
}

impl From<mpsc::Sender<HarnessInputMessage>> for Output {
    fn from(tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            kind: OutputKind::Channel(tx),
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
        }
    }
}

impl Output {
    /// Exhaust the real detached FIFO at a correlated mandatory-output
    /// boundary.
    #[cfg(test)]
    fn saturate_for_test(&self, correlation: &str) {
        let hook = SATURATION_HOOK
            .lock()
            .expect("xmpp saturation hook")
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
    /// XMPP worker and tool output is best-effort once the harness has
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

    /// Submit one transient tool progress observation.
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
        match &event {
            Event::ToolResult(result) => self.saturate_for_test(result.call_id.as_str()),
            Event::ToolError(error) => self.saturate_for_test(error.call_id.as_str()),
            Event::ToolCancelled(cancelled) => {
                self.saturate_for_test(cancelled.call_id.as_str());
            }
            _ => {}
        }
        let outcome = match tau_client::ToolTerminalOutcome::try_from(event) {
            Ok(outcome) => outcome,
            Err(event) => {
                return Err(ClientError::handler(format!(
                    "XMPP tool returned non-terminal event {}",
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

    /// Emit one transient external-message report for downstream
    /// canonicalization.
    pub(crate) fn emit_message_report(&self, event: Event) -> ClientResult<()> {
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(event.is_message_report());
        #[cfg(test)]
        match &event {
            Event::MessageSentReported(report) => {
                self.saturate_for_test(&report.text);
            }
            Event::MessageDeliveredReported(_) => {
                self.saturate_for_test("message.delivered_reported");
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

    /// Signal worker failure while preserving the checked publication result.
    fn record_mandatory_result(&self, result: ClientResult<()>) -> ClientResult<()> {
        if result.is_err()
            && let OutputKind::Client { failure, .. } = &self.kind
        {
            failure.report();
        }
        result
    }

    /// Install the wake handle used for worker-side output failure.
    pub(crate) fn install_waker(&self, waker: ManualRuntimeWaker) {
        if let OutputKind::Client { failure, .. } = &self.kind {
            failure.install_waker(waker);
        }
    }

    /// Fail the protocol loop after checked worker output fails.
    pub(crate) fn check_mandatory_output(&self) -> ClientResult<()> {
        match &self.kind {
            OutputKind::Channel(_) => Ok(()),
            OutputKind::Client { failure, .. } => failure.check(),
        }
    }
}
