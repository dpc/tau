//! Owns startup connection acceptance, configured extension spawning, and
//! startup commands.
//!
//! This boundary preserves the configured same-user extension trust model and
//! startup protocol.

use super::*;

impl Harness {
    /// Record the runtime harness metadata path stem owned by the daemon.
    pub(crate) fn set_runtime_harness_path(&mut self, path: PathBuf) {
        self.session_runtime.runtime_harness_path = Some(path);
    }

    /// Return whether this harness accepts bare inter-session messages.
    pub(crate) fn has_peer_entrypoint(&self) -> bool {
        !self.config.inter_session_receivers.is_empty()
    }

    pub(super) fn accept_initial_client(
        &mut self,
        initial_client: Option<InitialClient>,
        initial_client_error_stream: &mut Option<InitialClientStartupErrorOutput>,
    ) -> Result<Option<ConnectionId>, HarnessError> {
        let Some(initial_client) = initial_client else {
            return Ok(None);
        };
        let client_id = match initial_client {
            InitialClient::Stdio => self.accept_stdio_client()?,
        };
        *initial_client_error_stream = None;
        if let Err(error) = self.wait_for_initial_ui_subscribe() {
            self.send_startup_disconnect_to_initial_client(Some(&client_id), &error);
            return Err(error);
        }
        Ok(Some(client_id))
    }

    pub(super) fn spawn_configured_extensions(
        &mut self,
        config: &Config,
        sessions_dir: &Path,
        eager_session_id: &str,
        extension_secrets: &BTreeMap<String, BTreeMap<String, SecretValue>>,
        skipped_extensions: &BTreeSet<String>,
        startup_started_at: Instant,
    ) -> Result<(), HarnessError> {
        let mut extension_connects = Vec::new();
        let mut next_iid = instance_id_factory();
        for ext_config in config.extensions.values() {
            if skipped_extensions.contains(&ext_config.name) {
                continue;
            }
            let kind = match ext_config.role.as_deref() {
                Some("provider") => ClientKind::Provider,
                _ => ClientKind::Tool,
            };

            let log_path = if self.session_runtime.storage_mode.is_ephemeral() {
                None
            } else {
                Some(
                    extension_stderr_log_path(sessions_dir, eager_session_id, &ext_config.name)
                        .map_err(|error| HarnessError::Participant(error.to_string()))?,
                )
            };
            let spawned = match spawn_supervised(
                ext_config,
                kind.clone(),
                log_path,
                &self.runtime_io.tx,
                &self.runtime_io.component_ingress_tx,
                &self.session_runtime.state_dir,
                self.session_runtime.storage_mode.is_memory_only(),
                self.config
                    .provider_settings_snapshots
                    .get(ext_config.name.as_str())
                    .unwrap_or(&BTreeMap::new()),
            ) {
                Ok(spawned) => spawned,
                Err(error) if !ext_config.require => {
                    tracing::warn!(
                        target: "tau_harness::startup",
                        error = %error,
                        "optional extension did not initialize during spawn"
                    );
                    self.emit_optional_extension_skipped(&error.to_string());
                    continue;
                }
                Err(error) => return Err(error),
            };
            let conn_id = spawned.connection_id.clone();
            self.extensions.startup_deadlines.insert(
                conn_id.clone(),
                StartupDeadline {
                    deadline: Instant::now() + ext_config.startup_timeout,
                    name: tau_proto::ExtensionName::parse(ext_config.name.clone())
                        .expect("validated extension config name must remain canonical"),
                    require: ext_config.require,
                },
            );
            tracing::info!(
                target: "tau_harness::startup",
                extension = %ext_config.name,
                pid = spawned.child_pid,
                elapsed_ms = startup_started_at.elapsed().as_millis(),
                "extension spawned",
            );

            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: tau_proto::ExtensionName::parse(ext_config.name.clone())
                        .expect("validated extension config name must remain canonical"),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: kind.clone(),
                    peer_capabilities: Default::default(),
                    tool_prefix: ext_config.tool_prefix.clone(),
                    require: ext_config.require,
                    respawn_allowed: true,
                    pid: Some(spawned.child_pid),
                    in_process_thread: None,
                    supervised_config: Some(ext_config.clone()),
                    secrets: extension_secrets
                        .get(&ext_config.name)
                        .cloned()
                        .unwrap_or_default(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                    protocol_io: spawned.protocol_io,
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: spawned.writer_tx,
                initialized_ack: spawned.initialized_ack,
                supervised_writer: Some(spawned.writer),
                replaces: None,
            });
        }
        for command in extension_connects {
            self.queue_extension_connect(command)?;
        }
        Ok(())
    }

    pub(super) fn wait_for_initial_ui_subscribe(&mut self) -> Result<(), HarnessError> {
        let started_at = Instant::now();
        loop {
            let harness_evt = self
                .recv_startup_event(started_at)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                    frame_bytes,
                } => {
                    if self.handle_startup_from_connection_with_frame_bytes(
                        &connection_id,
                        *message,
                        frame_bytes,
                    )? {
                        if STARTUP_TIMEOUT <= started_at.elapsed() {
                            return Err(HarnessError::StartupTimeout);
                        }
                        return Ok(());
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    self.handle_startup_disconnect(&connection_id)?;
                }
                HarnessEvent::ReadFailed {
                    connection_id,
                    error,
                } => {
                    self.handle_startup_read_failure(&connection_id, error)?;
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                }
                HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => {
                    self.handle_supervised_writer_cleanup_complete_at(
                        &connection_id,
                        Instant::now(),
                    )?;
                }
                HarnessEvent::ComponentIngressReady => {
                    unreachable!("component ingress wakes expand before dispatch")
                }
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
    }

    pub(super) fn recv_startup_event(
        &mut self,
        started_at: Instant,
    ) -> Result<HarnessEvent, mpsc::RecvTimeoutError> {
        self.recv_event_until(started_at + STARTUP_TIMEOUT)
    }

    /// Replaces a non-payload ingress wake with its bounded component payload.
    pub(super) fn expand_component_ingress_wake(&self, event: HarnessEvent) -> HarnessEvent {
        if matches!(event, HarnessEvent::ComponentIngressReady) {
            let started = Instant::now();
            let taken = self
                .runtime_io
                .component_ingress
                .take_ready_with_diagnostic()
                .expect("component ingress wake must own one payload");
            let handoff_us = started.elapsed().as_micros();
            ComponentIngress::trace_take_diagnostic(taken.diagnostic);
            let event = taken.event;
            if let Some(traffic_class) = event.prompt_traffic_class() {
                tracing::trace!(
                    target: "tau_harness::prompt_ingress",
                    stage = "take_ready_to_runtime_handoff",
                    traffic_class,
                    take_ready_to_runtime_handoff_us = handoff_us,
                    "content-free component ingress stage"
                );
            }
            event
        } else {
            event
        }
    }

    /// Receive one harness event without crossing `deadline`, while continuing
    /// to service ordinary runtime deadlines.
    pub(super) fn recv_event_until(
        &mut self,
        deadline: Instant,
    ) -> Result<HarnessEvent, mpsc::RecvTimeoutError> {
        loop {
            self.process_runtime_deadlines();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let wait = self
                .next_runtime_deadline()
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(remaining)
                })
                .unwrap_or(remaining);
            match self.runtime_io.rx.recv_timeout(wait) {
                Ok(event) => return Ok(self.expand_component_ingress_wake(event)),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if Instant::now() < deadline && self.next_runtime_deadline().is_some() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    pub(super) fn handle_startup_from_connection(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        message: HarnessInputMessage,
    ) -> Result<bool, HarnessError> {
        let frame_bytes = tau_proto::ProtocolMessageBytes::new(
            tau_proto::encode_message_to_vec(&message)
                .expect("synthetic startup message must encode")
                .len() as u64,
        )
        .expect("an encoded startup message is nonempty");
        self.handle_startup_from_connection_with_frame_bytes(connection_id, message, frame_bytes)
    }

    pub(super) fn handle_startup_from_connection_with_frame_bytes(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        message: HarnessInputMessage,
        frame_bytes: tau_proto::ProtocolMessageBytes,
    ) -> Result<bool, HarnessError> {
        if matches!(&message, HarnessInputMessage::UiDebugEventStatsRequest(_))
            && !self.extensions.entries.contains_key(connection_id)
        {
            let _disposition = self.handle_client_message_disposition(connection_id, message)?;
            self.take_pending_publish_error()?;
            return Ok(false);
        }
        let origin = self
            .runtime_io
            .bus
            .connection(connection_id)
            .map(|m| m.origin.clone());
        let subscribed = match origin {
            Some(ConnectionOrigin::Socket) => {
                let detach_requested =
                    self.is_authorized_ui_detach_request(connection_id, &message);
                let subscribed = matches!(&message, HarnessInputMessage::Subscribe(_));
                if detach_requested {
                    self.ui_runtime.startup_detach_requested = true;
                }
                let disposition = self.handle_client_message_disposition(connection_id, message)?;
                let close = match disposition {
                    ClientMessageDisposition::Continue => false,
                    ClientMessageDisposition::Close => true,
                    ClientMessageDisposition::CloseAfterReply => {
                        self.drain_client_writer(connection_id);
                        true
                    }
                };
                if close {
                    self.handle_disconnect(connection_id);
                    if self.ui_runtime.startup_detach_requested {
                        false
                    } else {
                        return Err(HarnessError::Participant(
                            "initial UI disconnected during startup handshake".to_owned(),
                        ));
                    }
                } else {
                    subscribed
                }
            }
            Some(_) => {
                self.handle_extension_message_with_frame_bytes(
                    connection_id,
                    message,
                    frame_bytes,
                )?;
                false
            }
            None => false,
        };
        self.take_pending_publish_error()?;
        Ok(subscribed)
    }

    pub(super) fn handle_startup_disconnect(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) -> Result<(), HarnessError> {
        let name = self
            .runtime_io
            .bus
            .connection(connection_id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| {
                tau_proto::ExtensionName::parse(connection_id.to_string())
                    .expect("allocated connection id must satisfy the extension-name grammar")
            });
        let was_socket = self
            .runtime_io
            .bus
            .connection(connection_id)
            .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
        let was_provider = self.is_provider_extension(connection_id);
        let optional_pre_ready_extension = self
            .extensions
            .entries
            .get(connection_id)
            .is_some_and(|entry| !entry.require && entry.state != ExtensionState::Ready);
        if optional_pre_ready_extension {
            tracing::warn!(
                target: "tau_harness::startup",
                extension = %name,
                "optional extension did not initialize: disconnected before becoming ready"
            );
            self.disable_optional_extension(
                connection_id,
                &format!("optional extension {name} did not initialize"),
            );
            self.maybe_finish_extension_activation(Some(connection_id))?;
            return Ok(());
        }
        self.handle_disconnect(connection_id);
        if was_socket {
            if self.ui_runtime.startup_detach_requested {
                return Ok(());
            }
            return Err(HarnessError::Participant(format!(
                "{name} disconnected during startup"
            )));
        }
        if was_provider {
            return Err(provider_disconnected_error());
        }
        self.maybe_finish_extension_activation(Some(connection_id))?;
        Ok(())
    }

    pub(super) fn handle_startup_read_failure(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        error: String,
    ) -> Result<(), HarnessError> {
        if self.extensions.entries.contains_key(connection_id) {
            return self.handle_extension_protocol_failure(
                connection_id,
                format!("extension protocol decode failed: {error}"),
            );
        }
        self.handle_startup_disconnect(connection_id)
    }

    pub(super) fn log_event(&mut self, harness_event: &HarnessEvent) {
        if self.debug_harness_event_targets_ephemeral_agent(harness_event) {
            return;
        }
        if let Some(log) = &mut self.runtime_io.debug_log {
            let result = log.log_harness_event(harness_event);
            self.observe_debug_log_result(result);
        }
    }

    /// Reports a debug-log failure without changing semantic-event commit
    /// behavior.
    pub(super) fn observe_debug_log_result(
        &mut self,
        result: Result<(), crate::debug_log::DebugLogError>,
    ) {
        if let Err(error) = result {
            if error.disables_logging() {
                self.runtime_io.debug_log_poisoned = true;
                self.runtime_io.debug_log = None;
            }
            if error.should_report() {
                tracing::warn!(
                    target: "tau_harness",
                    error = %error.bounded_diagnostic(),
                    "debug event log append failed"
                );
            }
        }
    }

    pub(super) fn debug_harness_event_targets_ephemeral_agent(
        &self,
        harness_event: &HarnessEvent,
    ) -> bool {
        let HarnessEvent::FromConnection {
            connection_id,
            message,
            frame_bytes: _,
        } = harness_event
        else {
            return false;
        };
        match message.as_ref() {
            tau_proto::HarnessInputMessage::Emit(emit) => {
                self.event_targets_ephemeral_agent(&emit.event, None)
            }
            tau_proto::HarnessInputMessage::InterceptReply(reply) => {
                let Some(pending) = self
                    .runtime_io
                    .publication
                    .pending_intercept
                    .as_ref()
                    .filter(|pending| pending.conn_id == *connection_id)
                else {
                    return false;
                };
                let replacement_targets_ephemeral = match &reply.action {
                    tau_proto::InterceptAction::Pass(Some(replacement)) => {
                        self.debug_intercept_event_targets_ephemeral(replacement)
                    }
                    _ => false,
                };
                pending.original_shell_report_targets_ephemeral()
                    || self.debug_intercept_event_targets_ephemeral(&pending.event)
                    || replacement_targets_ephemeral
            }
            _ => false,
        }
    }

    pub(super) fn queue_extension_connect(
        &mut self,
        command: ExtensionConnectCommand,
    ) -> Result<(), HarnessError> {
        self.extensions.pending_connects += 1;
        if self
            .runtime_io
            .tx
            .send(HarnessEvent::Command(HarnessCommand::ConnectExtension(
                Box::new(command),
            )))
            .is_ok()
        {
            return Ok(());
        }
        self.extensions.pending_connects -= 1;
        Err(HarnessError::Participant(
            "harness command channel closed".to_owned(),
        ))
    }

    pub(super) fn handle_harness_command(
        &mut self,
        command: HarnessCommand,
    ) -> Result<(), HarnessError> {
        match command {
            HarnessCommand::SemanticPersistenceProgress => {
                self.observe_semantic_persistence_progress();
            }
            HarnessCommand::SemanticPersistenceActivationRetry => {
                self.retry_capacity_rejected_activations();
            }
            HarnessCommand::ConnectExtension(command) => self.connect_extension(*command)?,
            HarnessCommand::ExternalMessageToolCompleted(command) => {
                self.peer_messaging
                    .pending_external_message_auth
                    .remove(&command.auth_message_id);
                if command.session_generation != self.session_runtime.current_session_generation
                    || self
                        .tool_routing
                        .tool_runtime
                        .tool_agents
                        .get(&command.call_id)
                        != Some(&command.conversation_id)
                    || !self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .contains_key(&command.conversation_id)
                {
                    tracing::debug!(
                        target: "tau_harness::external_agent_message",
                        conversation_id = %command.conversation_id,
                        call_id = %command.call_id,
                        completion_generation = %command.session_generation,
                        current_generation = %self.session_runtime.current_session_generation,
                        "dropping stale external message tool completion"
                    );
                    return Ok(());
                }
                match command.result {
                    Ok((recipient_id, _started)) => {
                        if command.publish_sent {
                            self.publish_for_agent_from(
                                &command.conversation_id,
                                Some(crate::harness::harness_connection_id()),
                                Event::AgentMessageSent(tau_proto::AgentMessageSent {
                                    message_id: command.auth_message_id.clone(),
                                    sender_id: command.sender_id,
                                    recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
                                        session_id: command.recipient_session_id.clone(),
                                        agent_id: recipient_id.clone(),
                                    },
                                    kind: command.kind,
                                    message: command.message,
                                }),
                            );
                        }
                        self.finish_harness_owned_tool_with_cbor_result(
                            &command.conversation_id,
                            command.call_id,
                            command.tool_name,
                            command.tool_type,
                            tau_proto::CborValue::Map(vec![
                                (
                                    tau_proto::CborValue::Text("status".to_owned()),
                                    tau_proto::CborValue::Text(format!(
                                        "Message committed: {}; recipient was live; response not guaranteed",
                                        command.auth_message_id
                                    )),
                                ),
                                (
                                    tau_proto::CborValue::Text("message_id".to_owned()),
                                    tau_proto::CborValue::Text(
                                        command.auth_message_id.to_string(),
                                    ),
                                ),
                                (
                                    tau_proto::CborValue::Text("recipient".to_owned()),
                                    tau_proto::CborValue::Text(format!(
                                        "{}/{}",
                                        command.recipient_session_id, recipient_id
                                    )),
                                ),
                            ]),
                            None,
                        );
                    }
                    Err(error) => self.finish_harness_owned_tool_with_error(
                        &command.conversation_id,
                        command.call_id,
                        command.tool_name,
                        command.tool_type,
                        error.tool_message(),
                        Some(command.details),
                    ),
                }
            }
            HarnessCommand::ExternalMessageAuthCompleted(command) => {
                let client_id = command.client_id.clone();
                if command.session_generation != self.session_runtime.current_session_generation
                    || !self
                        .peer_messaging
                        .external_message_peers
                        .contains(&client_id)
                {
                    tracing::debug!(
                        target: "tau_harness::external_agent_message",
                        "dropping stale external message authentication completion"
                    );
                    return Ok(());
                }
                if let Some(result) = self.complete_external_agent_message_auth(
                    client_id.clone(),
                    command.session_generation,
                    command.request,
                    command.result,
                ) {
                    let _ = self.runtime_io.bus.send_to(
                        &client_id,
                        None,
                        HarnessOutputMessage::ExternalAgentMessageResult(result),
                    );
                }
            }
            HarnessCommand::SessionDiscoveryCompleted(command) => {
                if command.session_generation != self.session_runtime.current_session_generation
                    || self
                        .tool_routing
                        .tool_runtime
                        .tool_agents
                        .get(&command.call_id)
                        != Some(&command.conversation_id)
                    || !self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .contains_key(&command.conversation_id)
                {
                    tracing::debug!(
                        target: "tau_harness::session_discovery",
                        call_id = %command.call_id,
                        "dropping stale session discovery completion"
                    );
                    return Ok(());
                }
                self.finish_harness_owned_tool_with_cbor_result(
                    &command.conversation_id,
                    command.call_id,
                    command.tool_name,
                    command.tool_type,
                    command.result,
                    None,
                );
            }
        }
        Ok(())
    }

    pub(super) fn connect_extension(
        &mut self,
        command: ExtensionConnectCommand,
    ) -> Result<(), HarnessError> {
        let ExtensionConnectCommand {
            entry,
            origin,
            writer_tx,
            initialized_ack,
            supervised_writer,
            replaces,
        } = command;
        let connection_id = entry.connection_id.clone();
        let name = entry.name.clone();
        let kind = entry.kind.clone();

        let sink = ChannelSink::new(
            &writer_tx,
            path_std_sync::Arc::clone(&self.runtime_io.event_log),
            self.runtime_io.tx.clone(),
            connection_id.clone(),
        )
        .map_err(|error| HarnessError::Io(io::Error::other(error)))?;
        let connected_id = self.runtime_io.bus.connect(Connection::new(
            PendingConnectionMetadata {
                id: Some(connection_id.clone()),
                name: name.clone(),
                kind,
                origin,
            },
            Box::new(sink),
        ));
        debug_assert_eq!(connected_id, connection_id);

        if let Some(replaced) = replaces {
            assert!(
                !self.extensions.supervised_writers.contains_key(&replaced)
                    && !self.extensions.cleanup_deadlines.contains_key(&replaced)
                    && !self.extensions.restart_deadlines.contains_key(&replaced)
                    && !self.extensions.restart_budget_disabled.contains(&replaced),
                "replacement must follow complete cleanup and consume its deadline"
            );
            self.extensions.entries.remove(&replaced);
            self.extensions.activation_staging.remove(&replaced);
            self.extensions.ready_received.remove(&replaced);
            if let Some(slot) = self.extensions.order.iter_mut().find(|id| **id == replaced) {
                *slot = connection_id.clone();
            } else if !self.extensions.order.iter().any(|id| id == &connection_id) {
                self.extensions.order.push(connection_id.clone());
            }
        } else if !self.extensions.order.iter().any(|id| id == &connection_id) {
            self.extensions.order.push(connection_id.clone());
        }
        self.extensions
            .activation_staging
            .insert(connection_id.clone(), ExtensionActivationStage::default());
        if let Some(supervised_writer) = supervised_writer {
            self.extensions
                .supervised_writers
                .insert(connection_id.clone(), supervised_writer);
        }
        self.extensions.entries.insert(connection_id.clone(), entry);
        if 0 < self.extensions.pending_connects {
            self.extensions.pending_connects -= 1;
        }
        self.emit_extension_starting(&name);
        let _ = initialized_ack.send(());
        if let Some(expired) = self
            .extensions
            .expired_startup_connects
            .remove(&connection_id)
        {
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(!expired.require, "required startup timeout returned");
            tracing::warn!(
                target: "tau_harness::startup",
                extension = %expired.name,
                "optional extension did not initialize: timed out before connecting"
            );
            self.disable_optional_extension(
                &connection_id,
                &format!("optional extension {} did not initialize", expired.name),
            );
        }
        Ok(())
    }
}
