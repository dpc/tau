use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use crate::writer_thread::WriterCommand;
use crate::{ClientError, ClientResult};

/// Cloneable outbound handle for sending peer-to-harness protocol frames.
#[derive(Clone)]
pub struct ClientHandle {
    /// Channel to the serialized writer thread.
    sender: Arc<Mutex<Option<mpsc::Sender<WriterCommand>>>>,
    /// Immutable tool-name scope shared by every clone.
    tool_name_scope: Arc<OnceLock<crate::ToolNameScope>>,
    /// Public/background output remains gated until the runner writes `Ready`.
    startup_complete: Arc<AtomicBool>,
    /// Linearizes every pre-Ready ConfigError against the terminal Ready frame.
    startup_gate: Arc<Mutex<StartupGate>>,
    /// Detached output created by a state factory is released after `Ready`.
    pending_detached: Arc<Mutex<Vec<tau_proto::HarnessInputMessage>>>,
    /// Initial Configure callbacks may emit immediate diagnostics before Ready.
    configuring: Arc<AtomicBool>,
    /// Configuration-derived declarations replayed after static declarations.
    pending_configure_outputs: Arc<Mutex<Vec<tau_proto::HarnessInputMessage>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupGate {
    PreReady { rejected: bool },
    Ready,
}

impl ClientHandle {
    /// Creates a handle around a writer command channel.
    #[must_use]
    pub(crate) fn new(sender: mpsc::Sender<WriterCommand>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
            tool_name_scope: Arc::new(OnceLock::new()),
            startup_complete: Arc::new(AtomicBool::new(false)),
            startup_gate: Arc::new(Mutex::new(StartupGate::PreReady { rejected: false })),
            pending_detached: Arc::new(Mutex::new(Vec::new())),
            configuring: Arc::new(AtomicBool::new(false)),
            pending_configure_outputs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Install the immutable scope established by the initial `Configure`.
    pub(crate) fn install_tool_name_scope(&self, scope: crate::ToolNameScope) -> ClientResult<()> {
        if let Some(current) = self.tool_name_scope.get() {
            if current == &scope {
                return Ok(());
            }
            return Err(ClientError::handler(
                "tool_prefix changed after initial Configure; restart the extension to change tool identity",
            ));
        }
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        self.tool_name_scope
            .set(scope)
            .map_err(|_| ClientError::handler("failed to establish tool-name scope"))
    }

    /// Return the immutable tool-name scope after initial configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when called before the first `Configure`.
    pub fn tool_name_scope(&self) -> ClientResult<&crate::ToolNameScope> {
        self.tool_name_scope.get().ok_or_else(|| {
            ClientError::handler("tool-name scope is unavailable before initial Configure")
        })
    }

    /// Publish a transient registration declaration for one logical/local tool.
    ///
    /// Success means only that the declaration was buffered or flushed locally;
    /// it does not acknowledge harness commit or acceptance. Interception may
    /// drop or replace it, and accepted state appears later as a
    /// harness-authored `tool.register` event (or a rejection diagnostic).
    ///
    /// # Errors
    ///
    /// Returns an error when scope installation, composition, or output fails.
    pub fn register_local_tool(
        &self,
        registration: tau_proto::ToolRegistrationDeclared,
    ) -> ClientResult<()> {
        let registration = self.tool_name_scope()?.scope_registration(registration)?;
        self.emit_transient(tau_proto::Event::ToolRegistrationDeclared(registration))
    }

    /// Publishes a transient logical/local tool declaration without waiting for
    /// protocol flush.
    ///
    /// Queue admission is not a harness acceptance acknowledgement; accepted
    /// state appears later as canonical `tool.register` or a rejection
    /// diagnostic.
    ///
    /// # Errors
    ///
    /// Returns an error when scope composition fails or the writer has stopped.
    pub fn register_local_tool_detached(
        &self,
        registration: tau_proto::ToolRegistrationDeclared,
    ) -> ClientResult<()> {
        let registration = self.tool_name_scope()?.scope_registration(registration)?;
        self.emit_transient_detached(tau_proto::Event::ToolRegistrationDeclared(registration))
    }

    /// Publish a transient unregistration declaration for one logical/local
    /// tool.
    ///
    /// Success means only that the declaration was buffered or flushed locally;
    /// accepted withdrawal appears later as harness-authored `tool.unregister`,
    /// while an unknown or non-owned tool produces a rejection diagnostic.
    ///
    /// # Errors
    ///
    /// Returns an error when scope installation, composition, or output fails.
    pub fn unregister_local_tool(&self, local: tau_proto::ToolName) -> ClientResult<()> {
        let tool_name = self.tool_name_scope()?.wire_tool_name(&local)?;
        self.emit_transient(tau_proto::Event::ToolUnregistrationDeclared(
            tau_proto::ToolUnregistrationDeclared { tool_name },
        ))
    }

    /// Submit a transient tool progress observation for downstream routed-call
    /// validation.
    ///
    /// Success acknowledges only local protocol output. The harness commits the
    /// report through ordinary interception before it may publish a separate
    /// canonical `tool.progress` fact. The helper does not apply logical
    /// tool-name scoping; callers must preserve the routed wire name from
    /// [`tau_proto::ToolStarted::tool_name`].
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn report_tool_progress(&self, progress: tau_proto::ToolProgress) -> ClientResult<()> {
        self.emit_transient(tau_proto::Event::ToolProgressReported(progress))
    }

    /// Enqueue a transient tool progress observation without waiting for flush.
    ///
    /// Queue admission does not acknowledge report commit or canonical
    /// progress. The helper does not apply logical tool-name scoping; callers
    /// must preserve the routed wire name from
    /// [`tau_proto::ToolStarted::tool_name`].
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn report_tool_progress_detached(
        &self,
        progress: tau_proto::ToolProgress,
    ) -> ClientResult<()> {
        self.emit_transient_detached(tau_proto::Event::ToolProgressReported(progress))
    }

    /// Submit a transient successful tool completion for downstream validation.
    ///
    /// Success acknowledges only local protocol output. The harness commits the
    /// mutable report through ordinary interception, validates the captured
    /// routed-call owner, and may then publish protected canonical
    /// `tool.result` and `provider.tool_result` facts.
    ///
    /// This wire-level helper performs no routed-call correlation or logical
    /// tool-name scoping. Callers must preserve the exact call id, final wire
    /// name, and originator from the routed [`tau_proto::ToolStarted`].
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn report_tool_result(&self, result: tau_proto::ToolResult) -> ClientResult<()> {
        self.report_tool_terminal(result.into())
    }

    /// Enqueue a transient successful tool completion without waiting for
    /// flush.
    ///
    /// Queue admission does not acknowledge report commit or canonical
    /// completion. This wire-level helper performs no routed-call correlation
    /// or logical tool-name scoping; callers must preserve the exact routed
    /// [`tau_proto::ToolStarted`] fields.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn report_tool_result_detached(&self, result: tau_proto::ToolResult) -> ClientResult<()> {
        self.report_tool_terminal_detached(result.into())
    }

    /// Submit a transient failed tool completion for downstream validation.
    ///
    /// Success acknowledges only local protocol output. The harness may publish
    /// protected canonical `tool.error` and `provider.tool_error` facts after
    /// the report commits and its routed-call ownership validates.
    ///
    /// This wire-level helper performs no routed-call correlation or logical
    /// tool-name scoping. Callers must preserve the exact call id, final wire
    /// name, and originator from the routed [`tau_proto::ToolStarted`].
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn report_tool_error(&self, error: tau_proto::ToolError) -> ClientResult<()> {
        self.report_tool_terminal(error.into())
    }

    /// Enqueue a transient failed tool completion without waiting for flush.
    ///
    /// Queue admission does not acknowledge report commit or canonical
    /// completion. This wire-level helper performs no routed-call correlation
    /// or logical tool-name scoping; callers must preserve the exact routed
    /// [`tau_proto::ToolStarted`] fields.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn report_tool_error_detached(&self, error: tau_proto::ToolError) -> ClientResult<()> {
        self.report_tool_terminal_detached(error.into())
    }

    /// Submit a transient tool cancellation for downstream validation.
    ///
    /// The harness publishes canonical `tool.cancelled` for an accepted
    /// foreground cancellation, or preserves the existing background completion
    /// behavior when the call already runs in the background.
    ///
    /// This wire-level helper performs no routed-call correlation or logical
    /// tool-name scoping. Callers must preserve the exact call id and final
    /// wire name from the routed [`tau_proto::ToolStarted`].
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn report_tool_cancelled(&self, cancelled: tau_proto::ToolCancelled) -> ClientResult<()> {
        self.report_tool_terminal(cancelled.into())
    }

    /// Enqueue a transient tool cancellation without waiting for flush.
    ///
    /// Queue admission does not acknowledge report commit or canonical
    /// completion. This wire-level helper performs no routed-call correlation
    /// or logical tool-name scoping; callers must preserve the exact routed
    /// [`tau_proto::ToolStarted`] fields.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn report_tool_cancelled_detached(
        &self,
        cancelled: tau_proto::ToolCancelled,
    ) -> ClientResult<()> {
        self.report_tool_terminal_detached(cancelled.into())
    }

    /// Submit one typed terminal outcome as a transient peer report.
    ///
    /// This wire-level helper performs no routed-call correlation or logical
    /// tool-name scoping. Callers must preserve the exact routed
    /// [`tau_proto::ToolStarted`] fields.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn report_tool_terminal(&self, outcome: crate::ToolTerminalOutcome) -> ClientResult<()> {
        self.emit_transient(outcome.into_reported_event())
    }

    /// Enqueue one typed terminal outcome as a transient peer report.
    ///
    /// Queue admission does not acknowledge report commit or canonical
    /// completion. This wire-level helper performs no routed-call correlation
    /// or logical tool-name scoping; callers must preserve the exact routed
    /// [`tau_proto::ToolStarted`] fields.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn report_tool_terminal_detached(
        &self,
        outcome: crate::ToolTerminalOutcome,
    ) -> ClientResult<()> {
        self.emit_transient_detached(outcome.into_reported_event())
    }

    /// Sends one raw peer-to-harness message and normally waits until it is
    /// flushed.
    ///
    /// This wire-level API performs no logical-to-final tool-name mapping.
    /// During the initial Configure callback, capability declarations are
    /// buffered so static declarations can be written first; success then means
    /// accepted into that startup buffer, and a later startup call reports any
    /// encode/flush failure. `Ready` is always runner-owned and is rejected by
    /// this raw API.
    ///
    /// # Errors
    ///
    /// Returns an error when raw `Ready` is attempted, ordinary output is sent
    /// before startup Ready outside the initial Configure callback, the writer
    /// thread has stopped, the frame cannot be encoded or flushed, or the
    /// writer reports an I/O failure.
    pub fn send(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        if matches!(&message, tau_proto::HarnessInputMessage::Ready(_)) {
            return Err(ClientError::handler(
                "startup Ready is runner-owned and cannot be sent through the raw handle",
            ));
        }
        if matches!(&message, tau_proto::HarnessInputMessage::ConfigError(_)) {
            return self.send_config_error(message);
        }
        let configure_derived_declaration = matches!(
            &message,
            tau_proto::HarnessInputMessage::Subscribe(_)
                | tau_proto::HarnessInputMessage::Intercept(_)
        ) || matches!(
            &message,
            tau_proto::HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    tau_proto::Event::ActionSchemaPublished(_)
                        | tau_proto::Event::ToolRegistrationDeclared(_)
                        | tau_proto::Event::ToolUnregistrationDeclared(_)
                        | tau_proto::Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                        | tau_proto::Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                        | tau_proto::Event::ProviderModelsDeclared(_)
                        | tau_proto::Event::ExtensionContextProviderRegister(_)
                        | tau_proto::Event::ExtensionSessionContextProviderRegister(_)
                        | tau_proto::Event::ExtAgentContextPublish(_)
                        | tau_proto::Event::ExtPromptFragmentPublish(_)
                )
        );
        if self.configuring.load(Ordering::Acquire) && configure_derived_declaration {
            self.pending_configure_outputs
                .lock()
                .expect("lock pending Configure output")
                .push(message);
            return Ok(());
        }
        if !self.startup_complete.load(Ordering::Acquire)
            && !self.configuring.load(Ordering::Acquire)
        {
            return Err(ClientError::handler(
                "client output is unavailable before startup Ready",
            ));
        }
        self.send_immediate(message)
    }

    /// Sends one runner-owned startup frame before the public handle is
    /// released.
    pub(crate) fn send_startup(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        if matches!(&message, tau_proto::HarnessInputMessage::Ready(_)) {
            return Err(ClientError::handler(
                "startup Ready must use the synchronized startup gate",
            ));
        }
        if matches!(&message, tau_proto::HarnessInputMessage::ConfigError(_)) {
            return self.send_config_error(message);
        }
        self.send_immediate(message)
    }

    fn send_config_error(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        let mut gate = self.startup_gate.lock().expect("lock startup gate");
        if let StartupGate::PreReady { rejected } = &mut *gate {
            *rejected = true;
        }
        let ack = self.enqueue_immediate(message)?;
        drop(gate);
        Self::wait_for_ack(ack)
    }

    fn send_immediate(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        let ack = self.enqueue_immediate(message)?;
        Self::wait_for_ack(ack)
    }

    fn enqueue_immediate(
        &self,
        message: tau_proto::HarnessInputMessage,
    ) -> ClientResult<mpsc::Receiver<ClientResult<()>>> {
        let (ack_sender, ack_receiver) = mpsc::channel();
        self.enqueue(WriterCommand::Send(message, ack_sender))?;
        Ok(ack_receiver)
    }

    fn wait_for_ack(ack: mpsc::Receiver<ClientResult<()>>) -> ClientResult<()> {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        ack.recv().map_err(|_| ClientError::WriterClosed)?
    }

    /// Enqueues one peer-to-harness message without waiting for it to flush.
    ///
    /// This is intended for detached background workers whose result should not
    /// block the protocol reader. Use [`Self::send`] when the caller must know
    /// whether the frame was encoded and flushed before it continues. Detached
    /// `Ready` is rejected; a detached `ConfigError` immediately rejects
    /// pending startup rather than waiting behind the Ready boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when raw `Ready` is attempted or the writer thread has
    /// already stopped before the frame can be queued.
    pub fn send_detached(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        if matches!(&message, tau_proto::HarnessInputMessage::Ready(_)) {
            return Err(ClientError::handler(
                "startup Ready is runner-owned and cannot be sent through the raw handle",
            ));
        }
        if matches!(&message, tau_proto::HarnessInputMessage::ConfigError(_)) {
            let mut gate = self.startup_gate.lock().expect("lock startup gate");
            if let StartupGate::PreReady { rejected } = &mut *gate {
                *rejected = true;
            }
            let result = self.enqueue(WriterCommand::SendDetached(message));
            drop(gate);
            return result;
        }
        if !self.startup_complete.load(Ordering::Acquire) {
            let mut pending = self
                .pending_detached
                .lock()
                .expect("lock pending detached startup output");
            if !self.startup_complete.load(Ordering::Acquire) {
                pending.push(message);
                return Ok(());
            }
        }
        self.enqueue(WriterCommand::SendDetached(message))
    }

    /// Releases factory-created detached output after the terminal `Ready`.
    pub(crate) fn finish_startup(&self) -> ClientResult<()> {
        let pending = {
            let mut pending = self
                .pending_detached
                .lock()
                .expect("lock pending detached startup output");
            self.startup_complete.store(true, Ordering::Release);
            std::mem::take(&mut *pending)
        };
        for message in pending {
            self.enqueue(WriterCommand::SendDetached(message))?;
        }
        Ok(())
    }

    /// Emits a durable event through the harness.
    ///
    /// This wire-level API performs no logical-to-final tool-name mapping. Use
    /// [`Self::register_local_tool`] for a logical registration.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::emit(event))
    }

    /// Enqueues a durable event through the harness without waiting for flush.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn emit_detached(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send_detached(tau_proto::HarnessInputMessage::emit(event))
    }

    /// Emits an event with transient delivery metadata through the harness.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit_transient(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::emit_with_persist(
            event, false,
        ))
    }

    /// Enqueues a transient event through the harness without waiting for
    /// flush.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn emit_transient_detached(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send_detached(tau_proto::HarnessInputMessage::emit_with_persist(
            event, false,
        ))
    }

    /// Requests one routine user-visible notice from the harness.
    ///
    /// The harness owns the resulting notice kind, visibility, publication
    /// source, and live-only delivery policy. It caps
    /// [`tau_proto::NoticeLevel::Critical`] to
    /// [`tau_proto::NoticeLevel::Warning`]. The resulting event is live-only
    /// and is never replayed.
    ///
    /// Success confirms only that the request reached the local protocol writer
    /// and was flushed. It does not acknowledge harness commit or UI delivery;
    /// an interceptor may rewrite the message or drop the resulting notice.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn request_notice(
        &self,
        message: impl Into<String>,
        level: tau_proto::NoticeLevel,
    ) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: message.into(),
                level,
            },
        ))
    }

    /// Enqueues a routine notice request without waiting for writer flush.
    ///
    /// This has the same notice semantics as [`Self::request_notice`], but
    /// success confirms only local writer-queue admission.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn request_notice_detached(
        &self,
        message: impl Into<String>,
        level: tau_proto::NoticeLevel,
    ) -> ClientResult<()> {
        self.send_detached(tau_proto::HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: message.into(),
                level,
            },
        ))
    }

    /// Emits `extension.context_ready` for one agent after extension-owned
    /// per-agent context work is complete.
    ///
    /// This is only a protocol convenience paired with
    /// [`crate::ExtensionBuilder::register_context_provider`]. Callers still
    /// own any state folding, context publication, and readiness policy. The
    /// acknowledgement uses `persist=false` wire metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit_context_ready(
        &self,
        session_id: tau_proto::SessionId,
        agent_id: tau_proto::AgentId,
        agent_initialization_id: tau_proto::AgentInitializationId,
    ) -> ClientResult<()> {
        self.emit_transient(tau_proto::Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id,
                agent_id,
                agent_initialization_id,
            },
        ))
    }

    /// Emits `extension.session_context_ready` after extension-owned
    /// session-wide context work is complete.
    ///
    /// This is only a protocol convenience paired with
    /// [`crate::ExtensionBuilder::register_session_context_provider`]. Callers
    /// still own session lifecycle handling, context publication, and readiness
    /// policy. The acknowledgement uses `persist=false` wire metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit_session_context_ready(&self, session_id: tau_proto::SessionId) -> ClientResult<()> {
        self.emit_transient(tau_proto::Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady { session_id },
        ))
    }

    /// Publishes one complete transient session discovery source snapshot.
    ///
    /// Empty lists represent an explicit empty source. Success confirms only
    /// local protocol writer admission and flush; interception and harness
    /// validation determine whether the snapshot becomes canonical state.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn declare_session_discovery_snapshot(
        &self,
        snapshot: tau_proto::ExtensionSessionDiscoverySnapshotDeclared,
    ) -> ClientResult<()> {
        self.emit_transient(tau_proto::Event::ExtensionSessionDiscoverySnapshotDeclared(
            snapshot,
        ))
    }

    /// Publishes one complete transient source snapshot for an agent
    /// initialization.
    ///
    /// Empty lists represent an explicit empty source. Success confirms only
    /// local protocol writer admission and flush; interception, correlation,
    /// and harness validation determine whether the snapshot becomes
    /// canonical state.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn declare_agent_discovery_snapshot(
        &self,
        snapshot: tau_proto::ExtensionAgentDiscoverySnapshotDeclared,
    ) -> ClientResult<()> {
        self.emit_transient(tau_proto::Event::ExtensionAgentDiscoverySnapshotDeclared(
            snapshot,
        ))
    }

    /// Reports an extension configuration failure to the harness.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn config_error(&self, message: impl Into<String>) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::ConfigError(
            tau_proto::ConfigError {
                message: message.into(),
            },
        ))
    }

    /// Return whether startup has emitted a configuration rejection.
    pub(crate) fn startup_rejected(&self) -> bool {
        matches!(
            *self.startup_gate.lock().expect("lock startup gate"),
            StartupGate::PreReady { rejected: true }
        )
    }

    pub(crate) fn set_configuring(&self, configuring: bool) {
        self.configuring.store(configuring, Ordering::Release);
    }

    /// Atomically reject or publish the one terminal startup Ready frame.
    pub(crate) fn send_ready(&self, message: Option<String>) -> ClientResult<()> {
        let mut gate = self.startup_gate.lock().expect("lock startup gate");
        match *gate {
            StartupGate::PreReady { rejected: true } => {
                return Err(ClientError::handler(
                    "startup cannot send Ready after ConfigError",
                ));
            }
            StartupGate::PreReady { rejected: false } => {}
            StartupGate::Ready => {
                return Err(ClientError::handler("startup Ready has already been sent"));
            }
        }
        let ack =
            self.enqueue_immediate(tau_proto::HarnessInputMessage::Ready(tau_proto::Ready {
                message,
            }))?;
        *gate = StartupGate::Ready;
        drop(gate);
        Self::wait_for_ack(ack)?;
        self.finish_startup()
    }

    /// Replays accepted configuration-derived output after static declarations.
    pub(crate) fn flush_configure_outputs(&self) -> ClientResult<()> {
        let pending = std::mem::take(
            &mut *self
                .pending_configure_outputs
                .lock()
                .expect("lock pending Configure output"),
        );
        for message in pending {
            self.send_startup(message)?;
        }
        Ok(())
    }

    /// Drops output derived from a rejected initial configuration.
    pub(crate) fn discard_configure_outputs(&self) {
        self.pending_configure_outputs
            .lock()
            .expect("lock pending Configure output")
            .clear();
    }

    /// Requests a clean peer disconnect with a reason string.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn disconnect(&self, reason: impl Into<String>) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::Disconnect(
            tau_proto::Disconnect {
                reason: Some(reason.into()),
            },
        ))
    }

    /// Sends one prompt-interception reply to the harness.
    ///
    /// This is a protocol convenience for custom-loop extensions that own
    /// dynamic interception policy. The caller is still responsible for sending
    /// exactly one reply for each received `InterceptRequest`.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn intercept_reply(&self, action: tau_proto::InterceptAction) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::InterceptReply(
            tau_proto::InterceptReply { action },
        ))
    }

    /// Stops the writer thread after flushing any pending state.
    pub(crate) fn shutdown(&self) -> ClientResult<()> {
        let (ack_sender, ack_receiver) = mpsc::channel();
        let sender = self
            .sender
            .lock()
            .expect("lock client handle sender")
            .take()
            .ok_or(ClientError::WriterClosed)?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        sender
            .send(WriterCommand::Shutdown(ack_sender))
            .map_err(|_| ClientError::WriterClosed)?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        ack_receiver.recv().map_err(|_| ClientError::WriterClosed)?
    }

    fn enqueue(&self, command: WriterCommand) -> ClientResult<()> {
        let sender = self.sender.lock().expect("lock client handle sender");
        let sender = sender.as_ref().ok_or(ClientError::WriterClosed)?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        sender.send(command).map_err(|_| ClientError::WriterClosed)
    }
}
