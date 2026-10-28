//! Optional session-owned identity and prompt-free lazy materialization.

use super::*;

impl Harness {
    /// Approves exactly one immutable retained peer review. `true` means its
    /// worker receipt already exists; `false` means commit owns tool
    /// completion.
    pub(crate) fn release_peer_review(
        &mut self,
        conversation_id: &AgentId,
        source: tau_proto::PeerMessageReviewReference,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
    ) -> Result<bool, String> {
        let caller = self
            .ensure_agent_id_for_agent(conversation_id)
            .ok_or_else(|| "release caller is unavailable".to_owned())?;
        if self.main_agent_id() != Some(&caller)
            || source.main_agent_id != caller
            || !self.main_agent_is_eligible(&caller)
        {
            return Err(
                "only this session's available canonical main may release its reviews".to_owned(),
            );
        }
        let received = self
            .session_runtime
            .agent_store
            .agent(caller.as_str())
            .and_then(|tree| tree.peer_review_source(&source))
            .ok_or_else(|| "reference does not identify a retained peer review".to_owned())?;
        let recipient = &received.recipient_id;
        let worker = self
            .session_runtime
            .agent_store
            .agent(recipient.as_str())
            .ok_or_else(|| {
                "reviewed worker history is unavailable; load the original worker before retrying"
                    .to_owned()
            })?;
        if worker.has_peer_review_receipt(&source) {
            return Ok(true);
        }
        if self.agent_message_recipient_status(recipient.as_str())
            != AgentMessageRecipientStatus::Live
        {
            return Err(
                "reviewed worker is unavailable; the original target cannot be replaced".to_owned(),
            );
        }
        if self.peer_messaging.pending_external_receive_acks.len()
            >= super::subagents_tool::MAX_INBOUND_PEER_AUTH_JOBS
            || self
                .peer_messaging
                .pending_external_receive_acks
                .contains_key(&received.message_id)
        {
            return Err("peer receive or release is pending; retry later".to_owned());
        }
        let rate_admitted_at = self.admit_peer_input(
            recipient,
            super::subagents_tool::external_agent_message_admission_bytes(&received),
        )?;
        self.peer_messaging.pending_external_receive_acks.insert(
            received.message_id.clone(),
            PendingExternalReceiveAck {
                session_generation: self.session_runtime.current_session_generation,
                recipient_id: recipient.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::Exact(recipient.clone()),
                expected_receive: received.clone(),
                canceled: false,
                started: false,
                rate_admitted_at,
                completion: PendingPeerReceiveCompletion::Release {
                    conversation_id: conversation_id.clone(),
                    call_id,
                    tool_name,
                    tool_type,
                },
            },
        );
        self.publish_event(
            Some(harness_connection_id()),
            Event::AgentMessageReceived(received),
        );
        Ok(false)
    }

    /// Revalidates main authority and every original fact immediately before
    /// publishing a linked worker receipt.
    pub(super) fn peer_release_is_valid(
        &self,
        conversation_id: &AgentId,
        received: &tau_proto::AgentMessageReceived,
    ) -> bool {
        let Some(tau_proto::PeerMessageReview::Released { source }) = &received.peer_review else {
            return false;
        };
        self.main_agent_id() == Some(&source.main_agent_id)
            && self.main_agent_is_eligible(&source.main_agent_id)
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(conversation_id)
                .and_then(|agent| agent.identity.agent_id.as_ref())
                == Some(&source.main_agent_id)
            && self
                .session_runtime
                .agent_store
                .agent(source.main_agent_id.as_str())
                .and_then(|tree| tree.peer_review_source(source))
                .as_ref()
                == Some(received)
            && self
                .session_runtime
                .agent_store
                .agent(received.recipient_id.as_str())
                .is_some_and(|tree| !tree.has_peer_review_receipt(source))
    }

    /// Drops failed runtime discovery without changing durable main membership.
    pub(super) fn retire_unavailable_main_context(&mut self, main: &AgentId) {
        self.prompt_coordination
            .context_discovery
            .pending_agents
            .remove(main);
        self.prompt_coordination
            .context_discovery
            .frozen_agents
            .remove(main);
        self.prompt_coordination
            .context_discovery
            .initialized_agent_context
            .remove(main);
        self.agent_runtime
            .agent_registry
            .navigation_modes
            .remove(main);
        self.fail_ui_main_demands(&format!("session main agent `{main}` is unavailable"));
    }

    /// Defers UI demand until role/provider discovery and session restoration
    /// have completed, without delaying session initialization itself.
    pub(super) fn fulfill_ui_main_demands(&mut self) {
        if self.ui_runtime.main_demand_clients.is_empty()
            || !self.session_initialized(&self.session_runtime.current_session_id)
        {
            return;
        }
        self.ui_runtime.main_demand_processing = true;
        let result = self.ensure_main_agent();
        match result {
            Ok(Some(_)) if self.main_agent_id().is_none() => {}
            Ok(_) => {
                self.ui_runtime.main_demand_clients.clear();
            }
            Err(error) => {
                self.fail_ui_main_demands(&error);
            }
        }
        self.ui_runtime.main_demand_processing = false;
    }

    /// Continues only the subscriptions whose initial main demand awaited this
    /// ordinary membership commit. Focus remains attachment-local.
    pub(super) fn complete_ui_main_demands(&mut self) {
        if self.ui_runtime.main_demand_processing {
            return;
        }
        for client in std::mem::take(&mut self.ui_runtime.main_demand_clients) {
            let Some(historical) = self.runtime_io.bus.historical_subscriptions(&client) else {
                continue;
            };
            let historical = historical.to_vec();
            let live = self
                .runtime_io
                .bus
                .live_subscriptions(&client)
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            let _ = self.complete_subscription(&client, historical, live);
        }
    }

    /// Releases failed initial selections explicitly instead of leaving an
    /// attached UI waiting forever for a rejected membership publication.
    pub(super) fn fail_ui_main_demands(&mut self, error: &str) {
        for client in std::mem::take(&mut self.ui_runtime.main_demand_clients) {
            self.send_direct_harness_notice(
                &client,
                tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                tau_proto::NoticeLevel::Warning,
                tau_proto::NoticePurpose::Alert,
                error.to_owned(),
            );
            if !self.ui_runtime.main_demand_processing
                && self
                    .runtime_io
                    .bus
                    .historical_subscriptions(&client)
                    .is_some()
            {
                self.emit_session_replay_complete(&client, Some(error.to_owned()));
            }
        }
    }

    /// Returns only the canonical retained binding, never a matching-role
    /// agent.
    pub(crate) fn main_agent_id(&self) -> Option<&AgentId> {
        self.session_runtime
            .store
            .session(self.session_runtime.current_session_id.as_str())
            .and_then(tau_core::SessionMembership::main_agent_id)
    }

    /// Recognizes the canonical main or the one serialized live load candidate.
    pub(super) fn is_main_agent_load(&self, agent_id: &AgentId) -> bool {
        self.main_agent_id() == Some(agent_id)
            || self
                .session_runtime
                .current_session_state
                .main_candidate
                .as_ref()
                == Some(agent_id)
    }

    /// Materializes the configured main without admitting a prompt or
    /// inference.
    ///
    /// Ordinary creation and membership are independent transitions. An unused
    /// candidate after a crash is harmless; retained membership alone binds
    /// main. A bound identity is never silently replaced when its runtime
    /// is absent.
    pub(crate) fn ensure_main_agent(&mut self) -> Result<Option<AgentId>, String> {
        if self.ui_runtime.shutdown_requested
            || self.session_runtime.shutdown_published
            || !self.agent_runtime.agent_registry.roster_valid
        {
            return Err(
                "session main is unavailable during shutdown or failed restoration".to_owned(),
            );
        }
        if let Some(main) = self.main_agent_id().cloned().or_else(|| {
            self.session_runtime
                .current_session_state
                .main_candidate
                .clone()
        }) {
            return if self.main_agent_is_eligible(&main) {
                Ok(Some(main))
            } else {
                Err(format!("session main agent `{main}` is unavailable"))
            };
        }
        let Some(role) = self.config.accepted_harness_settings.main_role() else {
            return Ok(None);
        };
        let query = tau_proto::StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            query_id: "session-main".to_owned(),
            instruction: String::new(),
            role: Some(role.to_owned()),
            input_stats: Default::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: None,
        };
        let pending = self
            .prepare_start_agent_request(harness_connection_id(), query)?
            .ok_or_else(|| "configured session main role is unavailable".to_owned())?;
        let agent_id = crate::parse_agent_id(&pending.agent_id);
        self.session_runtime.current_session_state.main_candidate = Some(agent_id.clone());
        // Reuse the ordinary receiving lifecycle: no initial prompt, normal
        // tool surface, and resumability without a completed startup
        // model dispatch. Its creation marker does not confer
        // session-main authority.
        if let Err(error) = self.start_peer_agent_request(pending) {
            if self.main_agent_id().is_none() {
                self.session_runtime.current_session_state.main_candidate = None;
            }
            return Err(error.to_string());
        }
        Ok(Some(agent_id))
    }

    /// Checks current runtime and actual role availability without consulting
    /// today's main-role designation or substituting a different instance.
    pub(crate) fn main_agent_is_eligible(&self, agent_id: &AgentId) -> bool {
        if self.agent_message_recipient_status(agent_id.as_str())
            != AgentMessageRecipientStatus::Live
        {
            return false;
        }
        let role = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id)
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .and_then(|agent| agent.identity.role.as_deref());
        role.is_some_and(|role| {
            self.config.available_roles.contains_key(role)
                && crate::model::model_for_role(
                    &self.provider_runtime.model_info,
                    &self.config.available_roles,
                    role,
                )
                .is_some()
                && self.validate_required_tools_for_role(role).is_ok()
        })
    }
}
