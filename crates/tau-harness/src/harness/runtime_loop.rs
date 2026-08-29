//! Owns runtime waits, deadlines, event dispatch, and client writer draining.
//!
//! Extension lifecycle and cache owners supply deadlines; this loop only
//! invokes them in order.

use super::*;

/// Result of waiting for the next runtime-loop event.
pub(super) enum RuntimeEventWait {
    /// A harness event is ready to be logged and dispatched.
    Event(HarnessEvent),
    /// A runtime deadline elapsed and was processed; the loop should poll
    /// again.
    DeadlineElapsed,
    /// All senders for the harness event channel have disconnected.
    Disconnected,
}

impl Harness {
    /// Drives the event loop until the in-flight session initialization
    /// completes (turn state returns to `Idle`).
    ///
    /// Exact readiness or disconnect from every outstanding provider completes
    /// the wait before its non-renewable thirty-second cap. Final readiness
    /// completes synchronous harness finalization before deadline
    /// classification, so success cannot become
    /// [`HarnessError::SessionInitTimeout`] retroactively. See
    /// `SPEC-session-discovery-declarations-and-readiness`.
    pub(super) fn wait_for_session_init(&mut self) -> Result<(), HarnessError> {
        if self.session_runtime.turn_state.is_idle() {
            return Ok(());
        }
        let deadline = SessionInitDeadline::new(Instant::now());
        self.wait_for_session_init_with_deadline(deadline)
    }

    /// Drives the production session-init waiter with an explicit deadline.
    ///
    /// Tests inject already-expired boundaries without sleeping; production
    /// uses [`SessionInitDeadline::new`] through
    /// [`Self::wait_for_session_init`].
    pub(super) fn wait_for_session_init_with_deadline(
        &mut self,
        deadline: SessionInitDeadline,
    ) -> Result<(), HarnessError> {
        while !self.session_runtime.turn_state.is_idle() {
            let harness_evt = self
                .recv_session_init_event_until(deadline.next_deadline())
                .map_err(|_| HarnessError::SessionInitTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                    frame_bytes,
                } => {
                    let _ = self.handle_startup_from_connection_with_frame_bytes(
                        &connection_id,
                        *message,
                        frame_bytes,
                    )?;
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
                HarnessEvent::NewClient(_) => {}
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
            let providers_outstanding = !self.session_runtime.turn_state.is_idle();
            if !providers_outstanding {
                return Ok(());
            }
            if deadline.expired(Instant::now()) {
                return Err(HarnessError::SessionInitTimeout);
            }
        }
        Ok(())
    }

    /// Receives queued provider readiness before classifying a reached
    /// deadline.
    ///
    /// Runtime deadlines run first. The subsequent non-blocking cut gives a
    /// final waiter already admitted to the central FIFO precedence at the
    /// exact session deadline. When the FIFO is empty, ordinary
    /// deadline-aware receive behavior remains unchanged.
    pub(super) fn recv_session_init_event_until(
        &mut self,
        deadline: Instant,
    ) -> Result<HarnessEvent, mpsc::RecvTimeoutError> {
        self.process_runtime_deadlines();
        match self.runtime_io.rx.try_recv() {
            Ok(event) => Ok(self.expand_component_ingress_wake(event)),
            Err(mpsc::TryRecvError::Empty) => self.recv_event_until(deadline),
            Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvTimeoutError::Disconnected),
        }
    }

    pub(super) fn ensure_selected_role_available_after_required_skill_validation(
        &self,
    ) -> Result<(), HarnessError> {
        if self
            .config
            .available_roles
            .contains_key(&self.config.selected_role)
        {
            return Ok(());
        }
        if let Some(reason) = self
            .config
            .disabled_role_reasons
            .get(&self.config.selected_role)
        {
            return Err(HarnessError::Participant(format!(
                "{}; selected/default role is unavailable",
                reason.message
            )));
        }
        Err(HarnessError::Participant(format!(
            "selected/default role `{}` is unavailable",
            self.config.selected_role
        )))
    }

    /// Drives the event loop until every configured extension reaches
    /// `ExtensionState::Ready`. Replaces the old `wait_for_startup(n)`:
    /// state transitions are tracked per-extension so the same predicate
    /// can also gate runtime dispatch in `dispatch_blocked_for`.
    pub(super) fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        self.wait_for_extensions_ready_at(Instant::now())
    }

    /// Drive initial extension activation from one deadline anchor.
    ///
    /// Production records this anchor immediately before it begins waiting.
    /// Tests supply an exact instant to exercise ordering at a deadline without
    /// sleeping.
    pub(super) fn wait_for_extensions_ready_at(
        &mut self,
        wait_started_at: Instant,
    ) -> Result<(), HarnessError> {
        self.maybe_finish_extension_activation(None)?;
        if self.extensions.pending_connects == 0 && self.extensions_all_ready() {
            return Ok(());
        }
        self.extensions.startup_wait_deadline = Some(wait_started_at + STARTUP_TIMEOUT);
        self.ensure_extension_startup_deadlines(wait_started_at);
        while self.extensions.pending_connects != 0 || !self.extensions_all_ready() {
            self.handle_expired_extension_startup_deadlines(Instant::now())?;
            let deadline = self
                .next_extension_startup_deadline()
                .unwrap_or_else(|| Instant::now() + STARTUP_TIMEOUT);
            let harness_evt = if deadline <= Instant::now() {
                match self.runtime_io.rx.try_recv() {
                    Ok(event) => self.expand_component_ingress_wake(event),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
                        return self.handle_extensions_startup_timeout();
                    }
                }
            } else {
                match self.recv_event_until(deadline) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.handle_expired_extension_startup_deadlines(Instant::now())?;
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return self.handle_extensions_startup_timeout();
                    }
                }
            };
            self.log_event(&harness_evt);
            // Do not allow a frame received just before the deadline to activate
            // an extension after its own startup deadline.
            self.handle_expired_extension_startup_deadlines(Instant::now())?;
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                    frame_bytes,
                } => {
                    let _ = self.handle_startup_from_connection_with_frame_bytes(
                        &connection_id,
                        *message,
                        frame_bytes,
                    )?;
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
                HarnessEvent::NewClient(_) => {}
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
            self.ensure_extension_startup_deadlines(wait_started_at);
        }
        self.extensions.startup_wait_deadline = None;
        Ok(())
    }

    /// Assign the general startup deadline only to entries that an external
    /// caller installed without a successful supervised spawn record.
    pub(super) fn ensure_extension_startup_deadlines(&mut self, now: Instant) {
        let wait_deadline = self
            .extensions
            .startup_wait_deadline
            .unwrap_or(now + STARTUP_TIMEOUT);
        for connection_id in &self.extensions.order {
            let Some(entry) = self.extensions.entries.get(connection_id) else {
                continue;
            };
            if matches!(
                entry.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            ) || self.extensions.ready_received.contains(connection_id)
            {
                continue;
            }
            self.extensions
                .startup_deadlines
                .entry(connection_id.clone())
                .or_insert(StartupDeadline {
                    deadline: wait_deadline,
                    name: entry.name.clone(),
                    require: entry.require,
                });
        }
    }

    /// Return the next configured initial-readiness deadline.
    pub(super) fn next_extension_startup_deadline(&self) -> Option<Instant> {
        self.extensions
            .startup_deadlines
            .values()
            .map(|deadline| deadline.deadline)
            .chain(
                (self.extensions.pending_connects != 0)
                    .then_some(self.extensions.startup_wait_deadline)
                    .flatten(),
            )
            .min()
    }

    /// Apply every expired per-extension startup deadline.
    pub(super) fn handle_expired_extension_startup_deadlines(
        &mut self,
        now: Instant,
    ) -> Result<(), HarnessError> {
        let mut expired = self
            .extensions
            .startup_deadlines
            .iter()
            .filter_map(|(connection_id, deadline)| {
                (deadline.deadline <= now).then_some((connection_id.clone(), deadline.clone()))
            })
            .collect::<Vec<_>>();
        expired.sort_by(|(_, left), (_, right)| left.name.cmp(&right.name));
        if expired.is_empty() {
            return Ok(());
        }
        let required = expired
            .iter()
            .filter_map(|(_, deadline)| deadline.require.then_some(deadline.name.as_str()))
            .collect::<Vec<_>>();
        if !required.is_empty() {
            self.emit_info_important(&format!(
                "startup timed out waiting for required extension(s): {}",
                required.join(", ")
            ));
            return Err(HarnessError::StartupTimeout);
        }
        for (connection_id, deadline) in expired {
            self.extensions.startup_deadlines.remove(&connection_id);
            if self.extensions.entries.contains_key(&connection_id) {
                tracing::warn!(
                    target: "tau_harness::startup",
                    extension = %deadline.name,
                    "optional extension did not initialize: timed out before becoming ready"
                );
                self.disable_optional_extension(
                    &connection_id,
                    &format!("optional extension {} did not initialize", deadline.name),
                );
            } else {
                self.extensions
                    .expired_startup_connects
                    .insert(connection_id, deadline);
            }
        }
        self.maybe_finish_extension_activation(None)?;
        Ok(())
    }

    pub(super) fn handle_extensions_startup_timeout(&mut self) -> Result<(), HarnessError> {
        let blockers = self
            .extensions
            .entries
            .iter()
            .filter(|(connection_id, entry)| {
                !matches!(
                    entry.state,
                    ExtensionState::Ready | ExtensionState::Disconnected
                ) && !self.extensions.ready_received.contains(*connection_id)
            })
            .map(|(connection_id, _)| connection_id.clone())
            .collect();
        self.handle_extensions_startup_timeout_for(blockers)
    }

    /// Apply startup timeout policy to the supplied non-ready connections.
    pub(super) fn handle_extensions_startup_timeout_for(
        &mut self,
        timed_out: Vec<tau_proto::ConnectionId>,
    ) -> Result<(), HarnessError> {
        let blockers: Vec<_> = timed_out
            .into_iter()
            .filter_map(|connection_id| {
                self.extensions
                    .entries
                    .get(&connection_id)
                    .and_then(|entry| {
                        (!matches!(
                            entry.state,
                            ExtensionState::Ready | ExtensionState::Disconnected
                        ) && !self.extensions.ready_received.contains(&connection_id))
                        .then_some((connection_id, entry.name.clone(), entry.require))
                    })
            })
            .collect();
        let required_blockers: Vec<_> = blockers
            .iter()
            .filter_map(|(_, name, require)| require.then_some(name.as_str()))
            .collect();
        if !required_blockers.is_empty() {
            self.emit_info_important(&format!(
                "startup timed out waiting for required extension(s): {}",
                required_blockers.join(", ")
            ));
            return Err(HarnessError::StartupTimeout);
        }

        for (connection_id, name, require) in blockers {
            if require {
                continue;
            }
            tracing::warn!(
                target: "tau_harness::startup",
                extension = %name,
                "optional extension did not initialize: timed out before becoming ready"
            );
            self.disable_optional_extension(
                &connection_id,
                &format!("optional extension {name} did not initialize"),
            );
        }
        self.maybe_finish_extension_activation(None)?;

        if self.extensions.pending_connects == 0 && self.extensions_all_ready() {
            Ok(())
        } else {
            Err(HarnessError::StartupTimeout)
        }
    }

    // -----------------------------------------------------------------------
    // Main event loop (daemon mode)
    // -----------------------------------------------------------------------

    pub(crate) fn run_event_loop(
        &mut self,
        max_clients: Option<usize>,
        mut exit_on_disconnect: bool,
    ) -> Result<(), HarnessError> {
        let mut served_clients = 0_usize;
        if self.ui_runtime.startup_detach_requested {
            exit_on_disconnect = false;
        }
        let mut ever_attached = !self.ui_runtime.client_writers.is_empty();
        loop {
            if Self::should_stop_run_loop(
                max_clients,
                served_clients,
                exit_on_disconnect,
                ever_attached,
                self.ui_runtime.client_writers.is_empty(),
            ) {
                break;
            }
            let harness_evt = match self.next_runtime_event() {
                RuntimeEventWait::Event(event) => event,
                RuntimeEventWait::DeadlineElapsed => continue,
                RuntimeEventWait::Disconnected => break,
            };
            self.log_event(&harness_evt);
            self.handle_runtime_event(
                harness_evt,
                &mut served_clients,
                &mut exit_on_disconnect,
                &mut ever_attached,
            )?;
        }
        Ok(())
    }

    pub(super) fn should_stop_run_loop(
        max_clients: Option<usize>,
        served_clients: usize,
        exit_on_disconnect: bool,
        ever_attached: bool,
        no_clients_attached: bool,
    ) -> bool {
        if max_clients.is_some_and(|max| max <= served_clients) {
            return true;
        }
        // `exit_on_disconnect`: once at least one UI has been attached, exiting
        // the moment the last one leaves lets `tau` behave like a normal
        // foreground command. Before any UI attaches we wait — otherwise a
        // slightly late first connect would race us into immediate exit.
        exit_on_disconnect && ever_attached && no_clients_attached
    }

    pub(super) fn next_runtime_event(&mut self) -> RuntimeEventWait {
        self.process_runtime_deadlines();
        if self
            .next_runtime_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return RuntimeEventWait::DeadlineElapsed;
        }
        if let Some(event) = self.runtime_io.pending_runtime_event.take() {
            return RuntimeEventWait::Event(event);
        }
        if self.has_pending_long_wait_notifications() {
            return match self.runtime_io.rx.try_recv() {
                Ok(event) => RuntimeEventWait::Event(self.expand_component_ingress_wake(event)),
                Err(mpsc::TryRecvError::Empty) => RuntimeEventWait::DeadlineElapsed,
                Err(mpsc::TryRecvError::Disconnected) => RuntimeEventWait::Disconnected,
            };
        }
        if let Some(deadline) = self.next_runtime_deadline() {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match self.runtime_io.rx.recv_timeout(timeout) {
                Ok(event) => {
                    let event = self.expand_component_ingress_wake(event);
                    self.runtime_io.pending_runtime_event = Some(event);
                    #[cfg(test)]
                    let now = self
                        .runtime_io
                        .runtime_event_receive_cut
                        .take()
                        .unwrap_or_else(Instant::now);
                    #[cfg(not(test))]
                    let now = Instant::now();
                    self.process_runtime_deadlines_at(now);
                    if self
                        .next_runtime_deadline()
                        .is_some_and(|deadline| deadline <= now)
                    {
                        RuntimeEventWait::DeadlineElapsed
                    } else {
                        RuntimeEventWait::Event(
                            self.runtime_io
                                .pending_runtime_event
                                .take()
                                .expect("received event remains held"),
                        )
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.process_runtime_deadlines();
                    RuntimeEventWait::DeadlineElapsed
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => RuntimeEventWait::Disconnected,
            }
        } else {
            self.runtime_io
                .rx
                .recv()
                .map(|event| RuntimeEventWait::Event(self.expand_component_ingress_wake(event)))
                .unwrap_or(RuntimeEventWait::Disconnected)
        }
    }

    pub(super) fn process_runtime_deadlines(&mut self) {
        self.process_runtime_deadlines_at(Instant::now());
    }

    pub(super) fn process_runtime_deadlines_at(&mut self, now: Instant) {
        let max_live_lag = self.runtime_io.event_log.max_consumer_lag();
        if LIVE_EGRESS_LAG_WARNING_POSITIONS <= max_live_lag
            && self
                .runtime_io
                .last_live_egress_lag_warning
                .is_none_or(|last| {
                    LIVE_EGRESS_LAG_WARNING_INTERVAL <= now.saturating_duration_since(last)
                })
        {
            self.runtime_io.last_live_egress_lag_warning = Some(now);
            self.emit_info_important(&format!(
                "a connected component is pathologically behind the live event stream ({max_live_lag} pending positions); delivery remains active and retention may grow"
            ));
        }
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(
            self.agent_runtime
                .agent_watch
                .long_wait_materialization_budget
                .is_none()
        );
        self.agent_runtime
            .agent_watch
            .long_wait_materialization_budget =
            Some(subagents_tool::MAX_WORK_WAIT_THRESHOLDS_PER_RUNTIME_CYCLE);
        self.drain_pending_long_wait_notifications_for_scheduler();
        loop {
            // Drain one earliest deadline cohort at a time. Supplying that
            // cohort's deadline, rather than `now`, preserves deterministic
            // ordering when the event loop wakes after several classes are due.
            let background = self
                .tool_routing
                .tool_runtime
                .tool_turn
                .next_background_deadline();
            let input = self.next_input_wait_deadline();
            let work_wait = self.next_work_wait_threshold_deadline();
            let extension = self.next_extension_deadline();
            let cache = self.next_cache_refresh_deadline();
            let preview = self
                .prompt_coordination
                .context_discovery
                .pending_rendered_prompts
                .values()
                .map(|pending| pending.deadline)
                .min();
            let next = [work_wait, input, background, extension, cache, preview]
                .into_iter()
                .flatten()
                .min();
            let Some(deadline) = next.filter(|deadline| *deadline <= now) else {
                break;
            };
            if work_wait == Some(deadline) {
                let budget = self
                    .agent_runtime
                    .agent_watch
                    .long_wait_materialization_budget
                    .unwrap_or_default();
                let next_non_work = [input, background, extension, cache]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(now);
                self.process_work_wait_threshold_deadlines(now.min(next_non_work), budget);
            } else if input == Some(deadline) {
                self.process_input_wait_deadlines(deadline);
            } else if background == Some(deadline) {
                self.process_background_deadlines_at(deadline);
            } else if cache == Some(deadline) {
                self.process_cache_refresh_deadline();
            } else if preview == Some(deadline) {
                self.process_rendered_preview_deadlines(deadline);
            } else {
                self.process_extension_deadlines_at(deadline, now);
            }
        }
        self.agent_runtime
            .agent_watch
            .long_wait_materialization_budget = None;
    }

    pub(super) fn next_runtime_deadline(&self) -> Option<Instant> {
        [
            self.tool_routing
                .tool_runtime
                .tool_turn
                .next_background_deadline(),
            self.next_input_wait_deadline(),
            self.next_work_wait_threshold_deadline(),
            self.next_extension_deadline(),
            self.next_cache_refresh_deadline(),
            self.prompt_coordination
                .context_discovery
                .pending_rendered_prompts
                .values()
                .map(|pending| pending.deadline)
                .min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(super) fn next_cache_refresh_deadline(&self) -> Option<Instant> {
        self.provider_runtime.cache_residency.next_deadline()
    }

    pub(super) fn process_cache_refresh_deadline(&mut self) {
        let cancellations = self.provider_runtime.cache_residency.expire_deadlines();
        self.send_cache_refresh_cancellations(cancellations);
        for refresh in self.provider_runtime.cache_residency.admit() {
            tracing::debug!(
                target: "tau_harness",
                provider = %refresh.provider,
                agent_id = %refresh.request.prompt.agent_id,
                "dispatching bounded Provider cache refresh",
            );
            let refresh_id = refresh.request.refresh_id.clone();
            let delivered = self.runtime_io.bus.send_to(
                &refresh.connection_id,
                Some(crate::harness::harness_connection_id()),
                HarnessOutputMessage::deliver(Event::AgentCacheRefreshRequested(refresh.request)),
            );
            if !delivered.is_ok_and(|report| !report.delivered_to.is_empty()) {
                self.provider_runtime
                    .cache_residency
                    .finish(&refresh.connection_id, &refresh_id);
            }
        }
    }

    pub(super) fn send_cache_refresh_cancellations(
        &mut self,
        cancellations: Vec<crate::provider_cache_residency::CacheRefreshCancel>,
    ) {
        for cancellation in cancellations {
            let _ = self.runtime_io.bus.send_to(
                &cancellation.connection_id,
                Some(crate::harness::harness_connection_id()),
                HarnessOutputMessage::deliver(Event::AgentCacheRefreshCancelRequested(
                    cancellation.request,
                )),
            );
        }
    }

    pub(super) fn preempt_cache_refresh_for_prompt(&mut self, prompt: &AgentPromptCreated) {
        let cancellations = self
            .provider_runtime
            .cache_residency
            .cancel_real(&prompt.model.provider, &prompt.agent_id);
        self.send_cache_refresh_cancellations(cancellations);
    }

    pub(super) fn clear_cache_refreshes(
        &mut self,
        reason: tau_proto::ProviderCacheRefreshCancelReason,
    ) {
        self.provider_runtime
            .cache_refresh_tool_window_calls
            .clear();
        let cancellations = self.provider_runtime.cache_residency.clear(reason);
        self.send_cache_refresh_cancellations(cancellations);
    }

    pub(super) fn next_extension_deadline(&self) -> Option<Instant> {
        self.extensions
            .cleanup_deadlines
            .values()
            .chain(self.extensions.restart_deadlines.values())
            .copied()
            .min()
    }

    pub(super) fn process_extension_deadlines_at(&mut self, deadline: Instant, now: Instant) {
        let cleanup = self
            .extensions
            .order
            .iter()
            .filter(|connection_id| {
                self.extensions.cleanup_deadlines.get(*connection_id) == Some(&deadline)
            })
            .cloned()
            .collect::<Vec<_>>();
        for connection_id in cleanup {
            self.extensions.cleanup_deadlines.remove(&connection_id);
            if let Some(writer) = self.extensions.supervised_writers.get(&connection_id) {
                writer.fire_watchdog();
            }
        }

        let restarts = self
            .extensions
            .order
            .iter()
            .filter(|connection_id| {
                self.extensions.restart_deadlines.get(*connection_id) == Some(&deadline)
            })
            .cloned()
            .collect::<Vec<_>>();
        for connection_id in restarts {
            self.extensions.restart_deadlines.remove(&connection_id);
            if let Err(error) = self.try_respawn_supervised_extension(&connection_id) {
                tracing::warn!(
                    target: "tau_harness::startup",
                    connection_id = %connection_id,
                    error = %error,
                    "automatic extension restart failed"
                );
                self.schedule_extension_restart_at(&connection_id, now.max(Instant::now()));
            }
        }
    }

    pub(super) fn handle_runtime_event(
        &mut self,
        harness_evt: HarnessEvent,
        served_clients: &mut usize,
        exit_on_disconnect: &mut bool,
        ever_attached: &mut bool,
    ) -> Result<(), HarnessError> {
        match harness_evt {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                frame_bytes,
            } => self.handle_runtime_connection_message(
                connection_id,
                message,
                frame_bytes,
                served_clients,
                exit_on_disconnect,
            )?,
            HarnessEvent::Disconnected { connection_id } => {
                self.handle_runtime_disconnect(connection_id, served_clients)?;
            }
            HarnessEvent::ReadFailed {
                connection_id,
                error,
            } => {
                if self.extensions.entries.contains_key(&connection_id) {
                    self.handle_extension_protocol_failure(
                        &connection_id,
                        format!("extension protocol decode failed: {error}"),
                    )?;
                } else {
                    self.handle_runtime_disconnect(connection_id, served_clients)?;
                }
            }
            HarnessEvent::NewClient(stream) => {
                self.accept_client(stream)?;
                *ever_attached = true;
            }
            HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => {
                self.handle_supervised_writer_cleanup_complete_at(&connection_id, Instant::now())?;
            }
            HarnessEvent::ComponentIngressReady => {
                unreachable!("component ingress wakes expand before dispatch")
            }
            HarnessEvent::Command(command) => self.handle_harness_command(command)?,
        }
        self.take_pending_publish_error()
    }

    pub(super) fn handle_runtime_connection_message(
        &mut self,
        connection_id: ConnectionId,
        message: Box<HarnessInputMessage>,
        frame_bytes: tau_proto::ProtocolMessageBytes,
        served_clients: &mut usize,
        exit_on_disconnect: &mut bool,
    ) -> Result<(), HarnessError> {
        if matches!(
            message.as_ref(),
            HarnessInputMessage::UiDebugEventStatsRequest(_)
        ) && !self.extensions.entries.contains_key(&connection_id)
        {
            let _disposition = self.handle_client_message_disposition(&connection_id, *message)?;
            return Ok(());
        }
        let origin = self
            .runtime_io
            .bus
            .connection(&connection_id)
            .map(|m| m.origin.clone());
        match origin {
            Some(ConnectionOrigin::Socket) => {
                // `:detach` → stay alive even after this UI leaves; a later
                // `tau attach SESSION` can pick up right here.
                if self.is_authorized_ui_detach_request(&connection_id, &message) {
                    *exit_on_disconnect = false;
                }
                let disposition =
                    self.handle_client_message_disposition(&connection_id, *message)?;
                let close = match disposition {
                    ClientMessageDisposition::Continue => false,
                    ClientMessageDisposition::Close => true,
                    ClientMessageDisposition::CloseAfterReply => {
                        self.drain_client_writer(&connection_id);
                        true
                    }
                };
                if close {
                    self.handle_disconnect(&connection_id);
                    *served_clients += 1;
                }
            }
            Some(_) => {
                self.handle_extension_message_with_frame_bytes(
                    &connection_id,
                    *message,
                    frame_bytes,
                )?;
            }
            None => {}
        }
        Ok(())
    }

    pub(super) fn is_authorized_ui_detach_request(
        &self,
        connection_id: &tau_proto::ConnectionId,
        message: &HarnessInputMessage,
    ) -> bool {
        matches!(message, HarnessInputMessage::UiDetachRequest(_))
            && self.is_attached_socket_ui(connection_id)
    }

    pub(super) fn handle_runtime_disconnect(
        &mut self,
        connection_id: ConnectionId,
        served_clients: &mut usize,
    ) -> Result<(), HarnessError> {
        let was_provider = self.is_provider_extension(&connection_id);
        let was_socket = self
            .runtime_io
            .bus
            .connection(&connection_id)
            .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
        self.handle_disconnect(&connection_id);
        if was_socket {
            *served_clients += 1;
        }
        if was_provider {
            return Err(provider_disconnected_error());
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Client acceptance
    // -----------------------------------------------------------------------

    pub(crate) fn accept_client(
        &mut self,
        stream: UnixStream,
    ) -> Result<ConnectionId, HarnessError> {
        let write_stream = stream.try_clone()?;
        let shutdown_stream = stream.try_clone()?;
        self.accept_client_io(
            stream,
            write_stream,
            Some(shutdown_stream),
            ConnectionOrigin::Socket,
            ClientWriterFailure::Report,
        )
    }

    pub(crate) fn accept_stdio_client(&mut self) -> Result<ConnectionId, HarnessError> {
        self.accept_client_io(
            io::stdin(),
            io::stdout(),
            None,
            ConnectionOrigin::Socket,
            ClientWriterFailure::AwaitIngress,
        )
    }

    pub(super) fn accept_client_io<R, W>(
        &mut self,
        read: R,
        write: W,
        socket_shutdown: Option<UnixStream>,
        origin: ConnectionOrigin,
        writer_failure: ClientWriterFailure,
    ) -> Result<ConnectionId, HarnessError>
    where
        R: io::Read + Send + 'static,
        W: io::Write + Send + 'static,
    {
        let writer_tx = match writer_failure {
            ClientWriterFailure::Report => crate::event::spawn_writer_thread(write, None),
            ClientWriterFailure::AwaitIngress => {
                crate::event::spawn_initial_stdio_writer_thread(write, None)
            }
        };
        let conn_id = self.runtime_io.bus.reserve_connection_id();
        let sink = ChannelSink::new(
            &writer_tx,
            path_std_sync::Arc::clone(&self.runtime_io.event_log),
            self.runtime_io.tx.clone(),
            conn_id.clone(),
        )
        .map_err(|error| HarnessError::Io(io::Error::other(error)))?;
        let consumer = sink.handle();
        let connected_id = self.runtime_io.bus.connect(Connection::new(
            PendingConnectionMetadata {
                id: Some(conn_id.clone()),
                name: tau_proto::ExtensionName::parse("socket-ui")
                    .expect("socket UI name must satisfy the extension identifier grammar"),
                kind: ClientKind::Ui,
                origin,
            },
            Box::new(sink),
        ));
        debug_assert_eq!(connected_id, conn_id);
        let lifecycle = match socket_shutdown {
            Some(stream) => ClientWriterLifecycle::socket(consumer, stream),
            None => ClientWriterLifecycle::generic(consumer),
        };
        self.ui_runtime
            .client_writers
            .insert(conn_id.clone(), lifecycle);
        spawn_reader_thread(
            conn_id.clone(),
            read,
            self.runtime_io.component_ingress_tx.clone(),
        );
        Ok(conn_id)
    }

    pub(crate) fn send_startup_disconnect_to_initial_client(
        &mut self,
        client_id: Option<&ConnectionId>,
        error: &dyn std::fmt::Display,
    ) {
        let Some(client_id) = client_id else {
            return;
        };
        let _ = self.runtime_io.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some(format!("harness startup failed: {error}")),
            }),
        );
        let Some(writer) = self.ui_runtime.client_writers.remove(client_id) else {
            return;
        };
        writer.close_after_current_for_startup(STARTUP_DISCONNECT_GRACE);
    }

    /// Waits until the connection writer processes every previously queued
    /// frame or exits after an I/O failure.
    ///
    /// An absent or already-closed writer has no remaining queue to drain.
    pub(super) fn drain_client_writer(&self, client_id: &ConnectionId) {
        let Some(writer) = self.ui_runtime.client_writers.get(client_id) else {
            return;
        };
        writer.flush();
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------
}
