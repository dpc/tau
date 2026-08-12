//! Checked mandatory and detached optional harness output.

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use tau_client::{ClientError, ClientHandle, ClientResult, ManualRuntimeWaker};
use tau_proto::{
    Event, HarnessInputMessage, NoticeLevel, ToolProgress, ToolStarted, ToolUseState, ToolUseStatus,
};

#[cfg(test)]
pub(crate) static SATURATION_HOOK: Mutex<Option<(String, mpsc::Sender<()>)>> = Mutex::new(None);
#[cfg(test)]
pub(crate) static MUTATION_PUBLICATION_HOOK: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
    Mutex::new(None);

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
                "zulip mandatory output publication failed",
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
    /// Test output channel.
    Channel {
        /// Test output sender.
        tx: mpsc::Sender<HarnessInputMessage>,
        /// Test-visible mandatory failure state.
        failed: Arc<AtomicBool>,
        /// Remaining successful mandatory reports before test failure.
        #[cfg(test)]
        reports_before_failure: Option<Arc<AtomicUsize>>,
        /// Number of mandatory report attempts in deterministic tests.
        #[cfg(test)]
        report_attempts: Option<Arc<AtomicUsize>>,
    },
    /// Live tau-client output handle.
    Client {
        /// Ordered checked protocol output.
        handle: ClientHandle,
        /// Failure signal observed by the manual loop.
        failure: Arc<OutputFailure>,
    },
}

impl From<ClientHandle> for Output {
    fn from(value: ClientHandle) -> Self {
        Self {
            kind: OutputKind::Client {
                handle: value,
                failure: Arc::new(OutputFailure::new()),
            },
        }
    }
}

impl From<mpsc::Sender<HarnessInputMessage>> for Output {
    fn from(value: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            kind: OutputKind::Channel {
                tx: value,
                failed: Arc::new(AtomicBool::new(false)),
                #[cfg(test)]
                reports_before_failure: None,
                #[cfg(test)]
                report_attempts: None,
            },
        }
    }
}

impl Output {
    /// Build a test channel that fails after `reports` successful reports.
    #[cfg(test)]
    pub(crate) fn channel_failing_after(
        tx: mpsc::Sender<HarnessInputMessage>,
        reports: usize,
    ) -> Self {
        Self {
            kind: OutputKind::Channel {
                tx,
                failed: Arc::new(AtomicBool::new(false)),
                reports_before_failure: Some(Arc::new(AtomicUsize::new(reports))),
                report_attempts: Some(Arc::new(AtomicUsize::new(0))),
            },
        }
    }

    /// Return the deterministic mandatory-report attempt count.
    #[cfg(test)]
    pub(crate) fn report_attempts(&self) -> usize {
        match &self.kind {
            OutputKind::Channel {
                report_attempts: Some(attempts),
                ..
            } => attempts.load(Ordering::SeqCst),
            _ => 0,
        }
    }

    /// Return the live client handle when this output belongs to a protocol
    /// runtime.
    pub(crate) fn client_handle(&self) -> Option<&ClientHandle> {
        match &self.kind {
            OutputKind::Client { handle, .. } => Some(handle),
            OutputKind::Channel { .. } => None,
        }
    }

    /// Exhaust the real detached FIFO at a correlated mandatory-output
    /// boundary.
    #[cfg(test)]
    fn saturate_for_test(&self, correlation: &str) {
        let hook = SATURATION_HOOK
            .lock()
            .expect("zulip saturation hook")
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

    /// Submit optional output without changing mandatory ownership state.
    fn send(&self, message: HarnessInputMessage) -> bool {
        match &self.kind {
            OutputKind::Channel { tx, .. } => tx.send(message).is_ok(),
            OutputKind::Client { handle, .. } => handle.send_detached(message).is_ok(),
        }
    }

    /// Publish a mandatory message report and record writer failure.
    pub(crate) fn emit_message_report(&self, event: Event) -> bool {
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(event.is_message_report());
        #[cfg(test)]
        if matches!(
            event,
            Event::MessageEditedReported(_)
                | Event::MessageDeletedReported(_)
                | Event::MessageReactionAddedReported(_)
                | Event::MessageReactionRemovedReported(_)
        ) && let Some((entered, release)) = MUTATION_PUBLICATION_HOOK
            .lock()
            .expect("mutation publication hook")
            .take()
        {
            let _ = entered.send(());
            let _ = release.recv();
        }
        #[cfg(test)]
        match &event {
            Event::MessageDeliveredReported(_) => {
                self.saturate_for_test("message.delivered_reported");
            }
            Event::MessageSentReported(_) => self.saturate_for_test("message.sent_reported"),
            Event::MessageEditedReported(_) => self.saturate_for_test("message.edited_reported"),
            Event::MessageDeletedReported(_) => self.saturate_for_test("message.deleted_reported"),
            Event::MessageReactionAddedReported(_) => {
                self.saturate_for_test("message.reaction_added_reported");
            }
            Event::MessageReactionRemovedReported(_) => {
                self.saturate_for_test("message.reaction_removed_reported");
            }
            _ => {}
        }
        let message = HarnessInputMessage::emit_with_persist(event, false);
        let result = match &self.kind {
            OutputKind::Channel {
                tx,
                #[cfg(test)]
                reports_before_failure,
                #[cfg(test)]
                report_attempts,
                ..
            } => {
                #[cfg(test)]
                if let Some(attempts) = report_attempts {
                    attempts.fetch_add(1, Ordering::SeqCst);
                }
                #[cfg(test)]
                if reports_before_failure.as_ref().is_some_and(|remaining| {
                    remaining
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                            if value == 0 { None } else { Some(value - 1) }
                        })
                        .is_err()
                }) {
                    return self.record_mandatory_result(Err(ClientError::WriterClosed));
                }
                tx.send(message).map_err(|_| ClientError::WriterClosed)
            }
            OutputKind::Client { handle, .. } => handle.send(message),
        };
        self.record_mandatory_result(result)
    }

    /// Submit an optional warning notice.
    pub(crate) fn notice(&self, message: &str) {
        let _ = self.send(HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: message.to_owned(),
                level: NoticeLevel::Warning,
            },
        ));
    }

    /// Submit optional tool progress.
    pub(crate) fn progress(&self, invoke: &ToolStarted) {
        let _ = self.send(HarnessInputMessage::emit_with_persist(
            Event::ToolProgressReported(ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: Some("zulip tool started".to_owned()),
                progress: None,
                display: Some(ToolUseState {
                    status: ToolUseStatus::InProgress,
                    status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                    ..Default::default()
                }),
            }),
            false,
        ));
    }

    /// Publish the sole mandatory terminal for a tool call.
    pub(crate) fn terminal(&self, event: Event) -> ClientResult<()> {
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
            Ok(value) => value,
            Err(event) => {
                return Err(ClientError::handler(format!(
                    "zulip tool returned non-terminal event {}",
                    event.name()
                )));
            }
        };
        let result = match &self.kind {
            OutputKind::Channel { tx, .. } => tx
                .send(HarnessInputMessage::emit_with_persist(
                    outcome.into_reported_event(),
                    false,
                ))
                .map_err(|_| ClientError::WriterClosed),
            OutputKind::Client { handle, .. } => handle.report_tool_terminal(outcome),
        };
        if result.is_err() {
            match &self.kind {
                OutputKind::Client { failure, .. } => failure.report(),
                OutputKind::Channel { failed, .. } => failed.store(true, Ordering::Release),
            }
        }
        result
    }

    /// Convert mandatory publication to success while signaling loop failure.
    fn record_mandatory_result(&self, result: ClientResult<()>) -> bool {
        if result.is_err() {
            match &self.kind {
                OutputKind::Client { failure, .. } => failure.report(),
                OutputKind::Channel { failed, .. } => failed.store(true, Ordering::Release),
            }
        }
        result.is_ok()
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
            OutputKind::Channel { .. } => Ok(()),
            OutputKind::Client { failure, .. } => failure.check(),
        }
    }

    /// Return whether a checked mandatory publication has failed.
    pub(crate) fn mandatory_output_failed(&self) -> bool {
        match &self.kind {
            OutputKind::Channel { failed, .. } => failed.load(Ordering::Acquire),
            OutputKind::Client { failure, .. } => failure.failed.load(Ordering::Acquire),
        }
    }
}
