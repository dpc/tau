//! Source-bound canonical transport message intake and send completion.

use std::sync::atomic::{AtomicU64, Ordering};

use tau_proto::{
    AgentMessageIncoming, AgentMessageOutgoing, CompleteTransportSendRequest,
    CompleteTransportSendResult, Event, ExtensionName, HarnessOutputMessage, MessageContentTrust,
    MessageEndpoint, MessageEnvelope, MessageId, MessageReplyPath, MessageTransportRef,
    MessageTrust, RegisterTransportCapabilityRequest, RegisterTransportCapabilityResult,
    ReplyPathLifetime, ReplySelector, SenderPolicyStatus, ToolName, TransportMessageDraft,
    TransportMessageIngressOutcome, TransportMessageIngressRequest, TransportMessageIngressResult,
};

use super::{
    AgentMessageRecipientStatus, CompletedTransportSend, Harness, PendingIngressAck,
    PendingTransportSendAck, TransportCapability, TransportDedupKey, TransportDedupRecord,
    TransportOrderingRouteKey, TransportReplyRoute,
};

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_TRANSPORT_NAME_BYTES: usize = 48;
const MAX_DRAFT_BYTES: usize = 128 * 1024;
const MAX_DEDUP_KEY_BYTES: usize = 512;
const MAX_DEDUP_RECORDS: usize = 4096;
const MAX_SEND_RESULTS: usize = 4096;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

impl Harness {
    pub(super) fn fail_transport_publish(&mut self, event: &Event) {
        match event {
            Event::AgentMessageIncoming(message) => {
                let waiters = self
                    .pending_ingress_acks
                    .remove(&message.envelope.message_id)
                    .unwrap_or_default();
                for waiter in waiters {
                    let _ = self.bus.send_to(
                        &waiter.connection_id,
                        None,
                        HarnessOutputMessage::TransportMessageIngressResult(
                            TransportMessageIngressResult {
                                request_id: waiter.request_id,
                                message_id: None,
                                outcome: None,
                                error: Some("durable_commit_failed".to_owned()),
                            },
                        ),
                    );
                }
                let failed_keys = self
                    .transport_dedup
                    .iter()
                    .filter(|(_, record)| {
                        !record.committed && record.message_id == message.envelope.message_id
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                for key in failed_keys {
                    if let Some(record) = self.transport_dedup.remove(&key) {
                        let route_key = ordering_route_key(&key.extension_name, &record.draft);
                        if let Some(pending) =
                            self.pending_transport_route_sequences.get_mut(&route_key)
                        {
                            pending.remove(&message.envelope.message_id);
                            if pending.is_empty() {
                                self.pending_transport_route_sequences.remove(&route_key);
                            }
                        }
                    }
                }
            }
            Event::AgentMessageOutgoing(message) => {
                let call_id =
                    self.pending_transport_send_acks
                        .iter()
                        .find_map(|(call_id, pending)| {
                            (pending.message_id == message.envelope.message_id)
                                .then(|| call_id.clone())
                        });
                let Some(call_id) = call_id else {
                    return;
                };
                let Some(pending) = self.pending_transport_send_acks.remove(&call_id) else {
                    return;
                };
                let _ = self.bus.send_to(
                    &pending.connection_id,
                    None,
                    HarnessOutputMessage::CompleteTransportSendResult(
                        CompleteTransportSendResult {
                            request_id: pending.result.request_id,
                            message_id: None,
                            accepted: false,
                            error: Some("durable_commit_failed".to_owned()),
                        },
                    ),
                );
                if let Some(tool_result) = pending.tool_result {
                    self.handle_extension_tool_error(
                        &pending.connection_id,
                        tau_proto::ToolError {
                            call_id: tool_result.call_id,
                            tool_name: tool_result.tool_name,
                            tool_type: tool_result.tool_type,
                            message: "transport send succeeded remotely but local durable completion failed"
                                .to_owned(),
                            details: None,
                            display: None,
                            originator: tool_result.originator,
                        },
                    );
                }
            }
            Event::ProviderToolResult(result) => {
                let Some(pending) = self.pending_transport_send_acks.remove(&result.call_id) else {
                    return;
                };
                let _ = self.bus.send_to(
                    &pending.connection_id,
                    None,
                    HarnessOutputMessage::CompleteTransportSendResult(
                        CompleteTransportSendResult {
                            request_id: pending.result.request_id,
                            message_id: None,
                            accepted: false,
                            error: Some("terminal_commit_failed".to_owned()),
                        },
                    ),
                );
                let error = tau_proto::ToolError {
                    call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    tool_type: result.tool_type,
                    message: "remote send was accepted but terminal result could not be persisted"
                        .to_owned(),
                    details: None,
                    display: None,
                    originator: result.originator.clone(),
                };
                if let Some(cid) = self
                    .agent_routes
                    .get(pending.request.agent_id.as_str())
                    .cloned()
                {
                    self.publish_for_agent(&cid, Event::ProviderToolError(error));
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_register_transport_capability(
        &mut self,
        source_id: &str,
        request: RegisterTransportCapabilityRequest,
    ) {
        let result = self.register_transport_capability(source_id, request);
        let _ = self.bus.send_to(
            source_id,
            None,
            HarnessOutputMessage::RegisterTransportCapabilityResult(result),
        );
    }

    fn register_transport_capability(
        &mut self,
        source_id: &str,
        request: RegisterTransportCapabilityRequest,
    ) -> RegisterTransportCapabilityResult {
        let fail = |message: &str| RegisterTransportCapabilityResult {
            request_id: request.request_id.clone(),
            accepted: false,
            error: Some(message.to_owned()),
        };
        if !valid_request_id(&request.request_id) {
            return fail("invalid_request_id");
        }
        if !valid_transport_name(&request.transport_name) || request.transport_name == "tau" {
            return fail("invalid_transport_name");
        }
        if self.transport_source_name(source_id).is_none() {
            return fail("extension_source_required");
        }
        if let Some(tool) = &request.reply_tool
            && !self
                .registry
                .providers_for(tool.as_str())
                .iter()
                .any(|provider| provider.connection_id.as_str() == source_id)
        {
            return fail("reply_tool_not_owned");
        }
        let capability = TransportCapability {
            transport_name: request.transport_name.clone(),
            reply_tool: request.reply_tool,
            session_generation: self.current_session_generation,
        };
        let capabilities = self
            .transport_capabilities
            .entry(source_id.to_owned())
            .or_default();
        capabilities.retain(|existing| existing.transport_name != capability.transport_name);
        capabilities.push(capability);
        self.transport_reply_routes.retain(|_, route| {
            route.connection_id != source_id || route.transport_name != request.transport_name
        });
        RegisterTransportCapabilityResult {
            request_id: request.request_id,
            accepted: true,
            error: None,
        }
    }

    pub(super) fn revoke_transport_reply_tool(&mut self, source_id: &str, tool_name: &ToolName) {
        if let Some(capabilities) = self.transport_capabilities.get_mut(source_id) {
            capabilities.retain(|capability| capability.reply_tool.as_ref() != Some(tool_name));
        }
        self.transport_reply_routes.retain(|_, route| {
            route.connection_id != source_id || route.reply_tool.as_ref() != Some(tool_name)
        });
    }

    pub(super) fn handle_transport_message_ingress(
        &mut self,
        source_id: &str,
        request: TransportMessageIngressRequest,
    ) {
        if let Err(result) = self.begin_transport_message_ingress(source_id, request) {
            let _ = self.bus.send_to(
                source_id,
                None,
                HarnessOutputMessage::TransportMessageIngressResult(result),
            );
        }
    }

    fn begin_transport_message_ingress(
        &mut self,
        source_id: &str,
        request: TransportMessageIngressRequest,
    ) -> Result<(), TransportMessageIngressResult> {
        let fail = |message: &str| TransportMessageIngressResult {
            request_id: request.request_id.clone(),
            message_id: None,
            outcome: None,
            error: Some(message.to_owned()),
        };
        if !valid_request_id(&request.request_id) {
            return Err(fail("invalid_request_id"));
        }
        if self.agent_message_recipient_status(request.target_agent_id.as_str())
            != AgentMessageRecipientStatus::Live
        {
            return Err(fail("target_agent_not_live"));
        }
        let Some(extension_name) = self.transport_source_name(source_id) else {
            return Err(fail("extension_source_required"));
        };
        let Some(capability) = self.transport_capability(source_id, &request.draft) else {
            return Err(fail("transport_capability_not_registered"));
        };
        if let Err(error) = validate_draft(&request.draft) {
            return Err(fail(error));
        }
        if request.draft.identity_assurance
            == tau_proto::SenderIdentityAssurance::AuthenticatedTauAgent
            || request.draft.policy_status == SenderPolicyStatus::Internal
        {
            return Err(fail("reserved_trust_claim"));
        }
        let dedup_key = ingress_dedup_key(&extension_name, &request.draft)
            .ok_or_else(|| fail("stable_dedup_key_required"))?;
        self.restore_ingress_dedup_for_target(
            &dedup_key,
            &extension_name,
            &request.target_agent_id,
            &request.draft,
        );
        if let Some(existing) = self.transport_dedup.get(&dedup_key) {
            if existing.draft != request.draft
                || existing.target_agent_id != request.target_agent_id
            {
                return Err(fail("dedup_conflict"));
            }
            let result = TransportMessageIngressResult {
                request_id: request.request_id,
                message_id: Some(existing.message_id.clone()),
                outcome: Some(TransportMessageIngressOutcome::Duplicate),
                error: None,
            };
            if existing.committed {
                if existing.session_id == self.current_session_id
                    && let Some(reply_tool) = capability.reply_tool
                {
                    self.transport_reply_routes.insert(
                        existing.message_id.clone(),
                        TransportReplyRoute {
                            connection_id: source_id.to_owned(),
                            agent_id: request.target_agent_id.clone(),
                            session_generation: self.current_session_generation,
                            reply_tool: Some(reply_tool),
                            transport_name: request.draft.transport_name.clone(),
                            external_endpoint: request.draft.external_endpoint.clone(),
                            conversation: request.draft.conversation.clone(),
                        },
                    );
                }
                return Err(result);
            }
            self.pending_ingress_acks
                .entry(existing.message_id.clone())
                .or_default()
                .push(PendingIngressAck {
                    connection_id: source_id.to_owned(),
                    request_id: result.request_id,
                    session_generation: self.current_session_generation,
                });
            return Ok(());
        }
        let ordering_reservation = if let Some(ordering) = request.draft.ordering {
            let route_key = ordering_route_key(&extension_name, &request.draft);
            self.restore_route_sequence(
                &route_key,
                &extension_name,
                &request.target_agent_id,
                &request.draft,
            );
            if self
                .transport_route_sequences
                .get(&route_key)
                .into_iter()
                .chain(
                    self.pending_transport_route_sequences
                        .get(&route_key)
                        .into_iter()
                        .flat_map(|pending| pending.values()),
                )
                .max()
                .is_some_and(|last| ordering.source_sequence <= *last)
            {
                return Err(fail("source_sequence_out_of_order"));
            }
            Some((route_key, ordering.source_sequence))
        } else {
            None
        };
        let message_id = mint_message_id();
        if let Some((route_key, sequence)) = ordering_reservation {
            self.pending_transport_route_sequences
                .entry(route_key)
                .or_default()
                .insert(message_id.clone(), sequence);
        }
        let reply_path = capability
            .reply_tool
            .clone()
            .map(|tool_name| MessageReplyPath {
                tool_name,
                selector: ReplySelector::ReplyToMessage,
                lifetime: ReplyPathLifetime::ActiveSession,
            });
        let envelope = MessageEnvelope {
            message_id: message_id.clone(),
            transport: MessageTransportRef {
                name: request.draft.transport_name.clone(),
                instance: Some(extension_name),
            },
            source: request.draft.external_endpoint.clone(),
            destination: MessageEndpoint::Agent {
                session_id: Some(self.current_session_id.clone()),
                agent_id: request.target_agent_id.clone(),
                display_name: None,
            },
            conversation: request.draft.conversation.clone(),
            operation: request.draft.operation.clone(),
            trust: MessageTrust {
                content: MessageContentTrust::UntrustedExternal,
                identity: request.draft.identity_assurance,
                policy: request.draft.policy_status,
            },
            external_identity: request.draft.external_identity.clone(),
            ordering: request.draft.ordering,
            occurred_at: request.draft.occurred_at,
            reply_path,
        };
        if !self.insert_transport_dedup(
            dedup_key,
            TransportDedupRecord {
                draft: request.draft,
                target_agent_id: request.target_agent_id.clone(),
                message_id: message_id.clone(),
                committed: false,
                session_id: self.current_session_id.clone(),
            },
        ) {
            if let (Some(extension_name), Some(_)) =
                (envelope.transport.instance.as_ref(), envelope.ordering)
            {
                let route_key = ordering_route_key_from_envelope(extension_name, &envelope);
                if let Some(pending) = self.pending_transport_route_sequences.get_mut(&route_key) {
                    pending.remove(&message_id);
                    if pending.is_empty() {
                        self.pending_transport_route_sequences.remove(&route_key);
                    }
                }
            }
            return Err(fail("dedup_capacity_exhausted"));
        }
        self.pending_ingress_acks
            .entry(message_id)
            .or_default()
            .push(PendingIngressAck {
                connection_id: source_id.to_owned(),
                request_id: request.request_id,
                session_generation: self.current_session_generation,
            });
        let cid = self
            .agent_routes
            .get(request.target_agent_id.as_str())
            .cloned()
            .expect("live target has route");
        self.publish_for_agent(
            &cid,
            Event::AgentMessageIncoming(AgentMessageIncoming {
                recipient_id: request.target_agent_id,
                envelope,
            }),
        );
        Ok(())
    }

    pub(super) fn complete_ingress_commit(&mut self, message: &AgentMessageIncoming) -> bool {
        if let (Some(extension_name), Some(ordering)) = (
            message.envelope.transport.instance.as_ref(),
            message.envelope.ordering,
        ) {
            let route_key = ordering_route_key_from_envelope(extension_name, &message.envelope);
            if let Some(pending) = self.pending_transport_route_sequences.get_mut(&route_key) {
                pending.remove(&message.envelope.message_id);
                if pending.is_empty() {
                    self.pending_transport_route_sequences.remove(&route_key);
                }
            }
            self.transport_route_sequences
                .insert(route_key, ordering.source_sequence);
        }
        for record in self.transport_dedup.values_mut() {
            if record.message_id == message.envelope.message_id {
                record.committed = true;
            }
        }
        if let Some(reply_tool) = message
            .envelope
            .reply_path
            .as_ref()
            .map(|path| path.tool_name.clone())
            && let Some(waiters) = self.pending_ingress_acks.get(&message.envelope.message_id)
            && let Some(first) = waiters.first()
        {
            self.transport_reply_routes.insert(
                message.envelope.message_id.clone(),
                TransportReplyRoute {
                    connection_id: first.connection_id.clone(),
                    agent_id: message.recipient_id.clone(),
                    session_generation: self.current_session_generation,
                    reply_tool: Some(reply_tool),
                    transport_name: message.envelope.transport.name.clone(),
                    external_endpoint: message.envelope.source.clone(),
                    conversation: message.envelope.conversation.clone(),
                },
            );
        }
        let waiters = self
            .pending_ingress_acks
            .remove(&message.envelope.message_id)
            .unwrap_or_default();
        let live_commit = waiters.iter().any(|waiter| {
            waiter.session_generation == self.current_session_generation
                && self.bus.connection(&waiter.connection_id).is_some()
        });
        for (index, waiter) in waiters.into_iter().enumerate() {
            if waiter.session_generation != self.current_session_generation {
                continue;
            }
            let _ = self.bus.send_to(
                &waiter.connection_id,
                None,
                HarnessOutputMessage::TransportMessageIngressResult(
                    TransportMessageIngressResult {
                        request_id: waiter.request_id,
                        message_id: Some(message.envelope.message_id.clone()),
                        outcome: Some(if index == 0 {
                            TransportMessageIngressOutcome::Accepted
                        } else {
                            TransportMessageIngressOutcome::Duplicate
                        }),
                        error: None,
                    },
                ),
            );
        }
        live_commit
    }

    pub(super) fn handle_complete_transport_send(
        &mut self,
        source_id: &str,
        request: CompleteTransportSendRequest,
    ) {
        if let Err(result) = self.begin_complete_transport_send(source_id, request) {
            let _ = self.bus.send_to(
                source_id,
                None,
                HarnessOutputMessage::CompleteTransportSendResult(result),
            );
        }
    }

    fn begin_complete_transport_send(
        &mut self,
        source_id: &str,
        request: CompleteTransportSendRequest,
    ) -> Result<(), CompleteTransportSendResult> {
        let fail = |message: &str| CompleteTransportSendResult {
            request_id: request.request_id.clone(),
            message_id: None,
            accepted: false,
            error: Some(message.to_owned()),
        };
        let retry_key = (source_id.to_owned(), request.request_id.clone());
        if let Some(pending) = self.pending_transport_send_acks.get(&request.call_id) {
            return if pending.request == request {
                Ok(())
            } else {
                Err(fail("tool_call_completion_conflict"))
            };
        }
        if let Some(completed) = self.completed_transport_sends.get(&retry_key) {
            return Err(if completed.request == request {
                completed.result.clone()
            } else {
                fail("request_id_conflict")
            });
        }
        if let Some(pending) = self
            .pending_transport_send_acks
            .values()
            .find(|pending| pending.retry_key == retry_key)
        {
            return if pending.request == request {
                Ok(())
            } else {
                Err(fail("request_id_conflict"))
            };
        }
        let Some(cid) = self.tool_agents.get(&request.call_id).cloned() else {
            return Err(fail("unknown_or_completed_tool_call"));
        };
        let Some(actual_agent_id) = self
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
        else {
            return Err(fail("tool_call_agent_not_live"));
        };
        if !valid_request_id(&request.request_id)
            || request.call_id != request.tool_result.call_id
            || request.agent_id != actual_agent_id
        {
            return Err(fail("invalid_or_mismatched_completion"));
        }
        if !self.validate_tool_event_source(&request.call_id, source_id) {
            return Err(fail("tool_call_not_owned"));
        }
        if self.tool_turn.is_backgrounded(&request.call_id) {
            return Err(fail("background_transport_completion_not_supported"));
        }
        let Some(reply_to) = &request.in_reply_to else {
            return Err(fail("reply_to_required"));
        };
        let Some(route) = self.transport_reply_routes.get(reply_to).cloned() else {
            return Err(fail("unknown_or_stale_reply_route"));
        };
        if route.connection_id != source_id
            || route.agent_id != request.agent_id
            || route.session_generation != self.current_session_generation
            || route.reply_tool.as_ref() != Some(&request.tool_result.tool_name)
            || route.transport_name != request.draft.transport_name
            || route.external_endpoint != request.draft.external_endpoint
            || route.conversation != request.draft.conversation
            || self
                .transport_capability(source_id, &request.draft)
                .is_none()
            || request.draft.reply_tool.as_ref() != route.reply_tool.as_ref()
            || self
                .pending_tools
                .get(&request.call_id)
                .is_none_or(|tool| Some(&tool.name) != route.reply_tool.as_ref())
        {
            return Err(fail("reply_route_not_authorized"));
        }
        if validate_draft(&request.draft).is_err() {
            return Err(fail("invalid_outgoing_metadata"));
        }
        let message_id = mint_message_id();
        let extension_name = self
            .transport_source_name(source_id)
            .expect("validated extension route has extension");
        let envelope = MessageEnvelope {
            message_id: message_id.clone(),
            transport: MessageTransportRef {
                name: request.draft.transport_name.clone(),
                instance: Some(extension_name),
            },
            source: MessageEndpoint::Agent {
                session_id: Some(self.current_session_id.clone()),
                agent_id: request.agent_id.clone(),
                display_name: None,
            },
            destination: request.draft.external_endpoint.clone(),
            conversation: request.draft.conversation.clone(),
            operation: request.draft.operation.clone(),
            trust: MessageTrust {
                content: MessageContentTrust::AuthenticatedTauAgent,
                identity: tau_proto::SenderIdentityAssurance::AuthenticatedTauAgent,
                policy: SenderPolicyStatus::Internal,
            },
            external_identity: request.draft.external_identity.clone(),
            ordering: request.draft.ordering,
            occurred_at: request.draft.occurred_at,
            reply_path: None,
        };
        let result = CompleteTransportSendResult {
            request_id: request.request_id.clone(),
            message_id: Some(message_id.clone()),
            accepted: true,
            error: None,
        };
        self.pending_transport_send_acks.insert(
            request.call_id.clone(),
            PendingTransportSendAck {
                connection_id: source_id.to_owned(),
                retry_key,
                result,
                request: request.clone(),
                tool_result: Some(request.tool_result),
                message_id: message_id.clone(),
            },
        );
        self.publish_for_agent(
            &cid,
            Event::AgentMessageOutgoing(AgentMessageOutgoing {
                sender_id: request.agent_id,
                envelope,
                acceptance: request.acceptance,
                in_reply_to: request.in_reply_to.clone(),
            }),
        );
        Ok(())
    }

    pub(super) fn continue_transport_send_after_outgoing_commit(&mut self, message_id: &MessageId) {
        let pending = self
            .pending_transport_send_acks
            .values_mut()
            .find(|pending| &pending.message_id == message_id);
        let Some(pending) = pending else {
            return;
        };
        let Some(tool_result) = pending.tool_result.take() else {
            return;
        };
        let source_id = pending.connection_id.clone();
        self.handle_extension_tool_result(&source_id, tool_result);
    }

    pub(super) fn complete_transport_send_commit(&mut self, call_id: &tau_proto::ToolCallId) {
        let Some(pending) = self.pending_transport_send_acks.remove(call_id) else {
            return;
        };
        self.completed_transport_sends.insert(
            pending.retry_key.clone(),
            CompletedTransportSend {
                request: pending.request,
                result: pending.result.clone(),
            },
        );
        self.completed_transport_send_order
            .push_back(pending.retry_key);
        while self.completed_transport_sends.len() > MAX_SEND_RESULTS {
            if let Some(oldest) = self.completed_transport_send_order.pop_front() {
                self.completed_transport_sends.remove(&oldest);
            }
        }
        let _ = self.bus.send_to(
            &pending.connection_id,
            None,
            HarnessOutputMessage::CompleteTransportSendResult(pending.result),
        );
    }

    fn transport_capability(
        &self,
        source_id: &str,
        draft: &TransportMessageDraft,
    ) -> Option<TransportCapability> {
        self.transport_capabilities
            .get(source_id)?
            .iter()
            .find_map(|capability| {
                (capability.session_generation == self.current_session_generation
                    && capability.transport_name == draft.transport_name
                    && capability.reply_tool == draft.reply_tool)
                    .then(|| capability.clone())
            })
    }

    fn transport_source_name(&self, source_id: &str) -> Option<ExtensionName> {
        let metadata = self.bus.connection(source_id)?;
        (!matches!(metadata.kind, tau_proto::ClientKind::Ui))
            .then(|| ExtensionName::from(metadata.name.clone()))
    }

    fn restore_ingress_dedup_for_target(
        &mut self,
        dedup_key: &TransportDedupKey,
        extension_name: &ExtensionName,
        target_agent_id: &tau_proto::AgentId,
        draft: &TransportMessageDraft,
    ) {
        if self.transport_dedup.contains_key(dedup_key) {
            return;
        }
        let restored = self
            .agent_store
            .load_agent(target_agent_id.as_str())
            .ok()
            .flatten()
            .and_then(|tree| {
                tree.all_message_envelopes().find_map(|item| {
                    let envelope = &item.envelope;
                    let matches_scope = item.direction == tau_proto::MessageDirection::Incoming
                        && envelope.transport.name == draft.transport_name
                        && envelope.transport.instance.as_ref() == Some(extension_name)
                        && envelope
                            .external_identity
                            .as_ref()
                            .and_then(|identity| identity.dedup_key.as_deref())
                            == draft
                                .external_identity
                                .as_ref()
                                .and_then(|identity| identity.dedup_key.as_deref());
                    if !matches_scope {
                        return None;
                    }
                    let session_id = match &envelope.destination {
                        MessageEndpoint::Agent {
                            session_id: Some(session_id),
                            ..
                        } => session_id.clone(),
                        _ => return None,
                    };
                    Some(TransportDedupRecord {
                        draft: draft_from_envelope(envelope),
                        target_agent_id: target_agent_id.clone(),
                        message_id: envelope.message_id.clone(),
                        committed: true,
                        session_id,
                    })
                })
            });
        if let Some(record) = restored {
            let _ = self.insert_transport_dedup(dedup_key.clone(), record);
        }
    }

    fn insert_transport_dedup(
        &mut self,
        key: TransportDedupKey,
        record: TransportDedupRecord,
    ) -> bool {
        while self.transport_dedup.len() >= MAX_DEDUP_RECORDS
            && !self.transport_dedup.contains_key(&key)
        {
            let removable = self.transport_dedup_order.iter().position(|candidate| {
                self.transport_dedup
                    .get(candidate)
                    .is_some_and(|record| record.committed)
            });
            let Some(index) = removable else {
                return false;
            };
            let oldest = self
                .transport_dedup_order
                .remove(index)
                .expect("dedup eviction index exists");
            self.transport_dedup.remove(&oldest);
        }
        if !self.transport_dedup.contains_key(&key) {
            self.transport_dedup_order.push_back(key.clone());
        }
        self.transport_dedup.insert(key, record);
        true
    }

    fn restore_route_sequence(
        &mut self,
        route_key: &TransportOrderingRouteKey,
        extension_name: &ExtensionName,
        target_agent_id: &tau_proto::AgentId,
        draft: &TransportMessageDraft,
    ) {
        if self.transport_route_sequences.contains_key(route_key) {
            return;
        }
        let max_sequence = self
            .agent_store
            .load_agent(target_agent_id.as_str())
            .ok()
            .flatten()
            .and_then(|tree| {
                tree.all_message_envelopes()
                    .filter_map(|item| {
                        let envelope = &item.envelope;
                        (item.direction == tau_proto::MessageDirection::Incoming
                            && envelope.transport.instance.as_ref() == Some(extension_name)
                            && envelope.transport.name == draft.transport_name
                            && ordering_route_key_from_envelope(extension_name, envelope)
                                == *route_key)
                            .then_some(envelope.ordering?.source_sequence)
                    })
                    .max()
            });
        if let Some(sequence) = max_sequence {
            self.transport_route_sequences
                .insert(route_key.clone(), sequence);
        }
    }
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_REQUEST_ID_BYTES && !value.chars().any(char::is_control)
}

fn valid_transport_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TRANSPORT_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_draft(draft: &TransportMessageDraft) -> Result<(), &'static str> {
    if !valid_transport_name(&draft.transport_name) {
        return Err("invalid_transport_name");
    }
    if !matches!(draft.external_endpoint, MessageEndpoint::External { .. }) {
        return Err("external_endpoint_required");
    }
    let encoded = tau_proto::encode_message_to_vec(draft).map_err(|_| "invalid_metadata")?;
    if encoded.len() > MAX_DRAFT_BYTES {
        return Err("metadata_too_large");
    }
    if let Some(key) = draft
        .external_identity
        .as_ref()
        .and_then(|identity| identity.dedup_key.as_deref())
        && (key.is_empty() || key.len() > MAX_DEDUP_KEY_BYTES || key.chars().any(char::is_control))
    {
        return Err("invalid_dedup_key");
    }
    Ok(())
}

fn ingress_dedup_key(
    extension_name: &ExtensionName,
    draft: &TransportMessageDraft,
) -> Option<TransportDedupKey> {
    Some(TransportDedupKey {
        extension_name: extension_name.clone(),
        transport_name: draft.transport_name.clone(),
        dedup_key: draft.external_identity.as_ref()?.dedup_key.clone()?,
    })
}

fn ordering_route_key(
    extension_name: &ExtensionName,
    draft: &TransportMessageDraft,
) -> TransportOrderingRouteKey {
    let conversation_id = draft
        .conversation
        .as_ref()
        .and_then(|conversation| conversation.stable_id.clone());
    let thread_id = draft
        .conversation
        .as_ref()
        .and_then(|conversation| conversation.thread.as_ref())
        .map(|thread| thread.stable_id.clone());
    TransportOrderingRouteKey {
        extension_name: extension_name.clone(),
        transport_name: draft.transport_name.clone(),
        conversation_id,
        thread_id,
    }
}

fn ordering_route_key_from_envelope(
    extension_name: &ExtensionName,
    envelope: &MessageEnvelope,
) -> TransportOrderingRouteKey {
    TransportOrderingRouteKey {
        extension_name: extension_name.clone(),
        transport_name: envelope.transport.name.clone(),
        conversation_id: envelope
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.stable_id.clone()),
        thread_id: envelope
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.thread.as_ref())
            .map(|thread| thread.stable_id.clone()),
    }
}

fn draft_from_envelope(envelope: &MessageEnvelope) -> TransportMessageDraft {
    TransportMessageDraft {
        transport_name: envelope.transport.name.clone(),
        external_endpoint: envelope.source.clone(),
        conversation: envelope.conversation.clone(),
        operation: envelope.operation.clone(),
        identity_assurance: envelope.trust.identity,
        policy_status: envelope.trust.policy,
        external_identity: envelope.external_identity.clone(),
        ordering: envelope.ordering,
        occurred_at: envelope.occurred_at,
        reply_tool: envelope
            .reply_path
            .as_ref()
            .map(|path| path.tool_name.clone()),
    }
}

fn mint_message_id() -> MessageId {
    MessageId::new(format!(
        "msg-{}-{}",
        tau_proto::UnixMicros::now(),
        NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use tau_proto::{
        ExternalActorKind, ExternalMessageIdentity, MessageOperation, MessagePayload,
        SenderIdentityAssurance, TextFormat,
    };

    /// Exercises the actual harness RPC path: capability registration, durable
    /// commit acknowledgement, one transcript fact, and stable duplicate id.
    #[test]
    fn harness_ingress_rpc_commits_once_and_retries_stably() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "transport-test");
        let cid = super::super::tests::ensure_test_user_agent(&mut harness);
        let agent_id = harness
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("durable target");

        harness.handle_register_transport_capability(
            "transport-test",
            RegisterTransportCapabilityRequest {
                request_id: "register-1".to_owned(),
                transport_name: "slack".to_owned(),
                reply_tool: None,
            },
        );
        let request = TransportMessageIngressRequest {
            request_id: "ingress-1".to_owned(),
            target_agent_id: agent_id.clone(),
            draft: draft("event-1"),
        };
        harness.handle_transport_message_ingress("transport-test", request.clone());
        let first = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "ingress-1" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("commit ack");
        assert!(first.error.is_none(), "ingress failed: {first:?}");
        assert_eq!(
            first.outcome,
            Some(TransportMessageIngressOutcome::Accepted)
        );

        let mut retry = request;
        retry.request_id = "ingress-2".to_owned();
        harness.handle_transport_message_ingress("transport-test", retry);
        let duplicate = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "ingress-2" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("duplicate ack");
        assert_eq!(
            duplicate.outcome,
            Some(TransportMessageIngressOutcome::Duplicate)
        );
        assert_eq!(duplicate.message_id, first.message_id);
        let tree = harness
            .agent_store
            .agent(agent_id.as_str())
            .expect("target tree");
        assert_eq!(tree.all_message_envelopes().count(), 1);
    }

    /// Capacity backpressure must not leave a phantom ordering reservation that
    /// rejects a later valid occurrence after committed history becomes
    /// evictable.
    #[test]
    fn ingress_capacity_rejection_rolls_back_ordering_reservation() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "capacity-test");
        let cid = super::super::tests::ensure_test_user_agent(&mut harness);
        let agent_id = harness
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("durable target");
        harness.handle_register_transport_capability(
            "capacity-test",
            RegisterTransportCapabilityRequest {
                request_id: "register-capacity".to_owned(),
                transport_name: "slack".to_owned(),
                reply_tool: None,
            },
        );
        for index in 0..MAX_DEDUP_RECORDS {
            let key = TransportDedupKey {
                extension_name: ExtensionName::from("capacity-test"),
                transport_name: "slack".to_owned(),
                dedup_key: format!("parked-{index}"),
            };
            assert!(harness.insert_transport_dedup(
                key,
                TransportDedupRecord {
                    draft: draft(&format!("parked-{index}")),
                    target_agent_id: agent_id.clone(),
                    message_id: MessageId::new(format!("msg-parked-{index}")),
                    committed: false,
                    session_id: harness.current_session_id.clone(),
                },
            ));
        }
        let mut ordered = draft("ordered-capacity");
        ordered.ordering = Some(tau_proto::MessageOrdering { source_sequence: 1 });
        harness.handle_transport_message_ingress(
            "capacity-test",
            TransportMessageIngressRequest {
                request_id: "capacity-rejected".to_owned(),
                target_agent_id: agent_id.clone(),
                draft: ordered.clone(),
            },
        );
        let rejected = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "capacity-rejected" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("capacity result");
        assert_eq!(rejected.error.as_deref(), Some("dedup_capacity_exhausted"));
        assert!(harness.pending_transport_route_sequences.is_empty());

        harness
            .transport_dedup
            .values_mut()
            .next()
            .expect("seeded record")
            .committed = true;
        let mut retry = ordered;
        retry
            .external_identity
            .as_mut()
            .expect("identity")
            .dedup_key = Some("ordered-after-capacity".to_owned());
        harness.handle_transport_message_ingress(
            "capacity-test",
            TransportMessageIngressRequest {
                request_id: "capacity-accepted".to_owned(),
                target_agent_id: agent_id,
                draft: retry,
            },
        );
        let accepted = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "capacity-accepted" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("accepted result");
        assert_eq!(
            accepted.outcome,
            Some(TransportMessageIngressOutcome::Accepted)
        );
    }

    /// Exercises the successful egress state machine end to end and prevents
    /// regressions where ACK precedes the outgoing fact/terminal result or an
    /// exact retry emits either fact again.
    #[test]
    fn harness_send_completion_commits_fact_then_terminal_and_retries_stably() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "transport-send");
        let cid = super::super::tests::ensure_test_user_agent(&mut harness);
        let agent_id = harness
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("durable target");
        let call_id = tau_proto::ToolCallId::from("call-transport-send");
        let tool_name = ToolName::new("slack_send");
        harness.publish_for_agent(
            &cid,
            Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
                agent_prompt_id: "prompt-transport-send".into(),
                agent_id: agent_id.clone(),
                output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: call_id.clone(),
                    name: tool_name.clone(),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Null,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        );
        harness.tool_agents.insert(call_id.clone(), cid.clone());
        harness.pending_tools.insert(
            call_id.clone(),
            super::super::PendingTool {
                name: tool_name.clone(),
                internal_name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
            },
        );
        harness.pending_tool_providers.insert(
            call_id.clone(),
            tau_proto::ConnectionId::from("transport-send"),
        );
        harness.transport_capabilities.insert(
            "transport-send".to_owned(),
            vec![TransportCapability {
                transport_name: "slack".to_owned(),
                reply_tool: Some(tool_name.clone()),
                session_generation: harness.current_session_generation,
            }],
        );
        let reply_to = MessageId::new("msg-route");
        let endpoint = MessageEndpoint::External {
            stable_id: Some("U1".to_owned()),
            display_name: None,
            actor_kind: ExternalActorKind::Human,
        };
        harness.transport_reply_routes.insert(
            reply_to.clone(),
            TransportReplyRoute {
                connection_id: "transport-send".to_owned(),
                agent_id: agent_id.clone(),
                session_generation: harness.current_session_generation,
                reply_tool: Some(tool_name.clone()),
                transport_name: "slack".to_owned(),
                external_endpoint: endpoint.clone(),
                conversation: None,
            },
        );
        let request = CompleteTransportSendRequest {
            request_id: "send-1".to_owned(),
            call_id: call_id.clone(),
            agent_id: agent_id.clone(),
            in_reply_to: Some(reply_to),
            draft: TransportMessageDraft {
                transport_name: "slack".to_owned(),
                external_endpoint: endpoint,
                conversation: None,
                operation: MessageOperation::Create {
                    payload: MessagePayload::Text {
                        text: "reply".to_owned(),
                        format: TextFormat::Plain,
                    },
                },
                identity_assurance: SenderIdentityAssurance::Unknown,
                policy_status: SenderPolicyStatus::Allowlisted,
                external_identity: None,
                ordering: None,
                occurred_at: None,
                reply_tool: Some(tool_name.clone()),
            },
            acceptance: tau_proto::MessageTransportAcceptance::SubmittedToTransport,
            tool_result: tau_proto::ToolResult {
                call_id: call_id.clone(),
                tool_name,
                tool_type: tau_proto::ToolType::Function,
                result: tau_proto::CborValue::Text("ok".to_owned()),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            },
        };
        harness.handle_complete_transport_send("transport-send", request.clone());
        let committed = harness.event_log.entries_for_test();
        let outgoing_seq = committed
            .iter()
            .find(|entry| matches!(entry.event, Event::AgentMessageOutgoing(_)))
            .expect("outgoing committed")
            .seq;
        let terminal_seq = committed
            .iter()
            .find(|entry| matches!(entry.event, Event::ProviderToolResult(_)))
            .expect("terminal committed")
            .seq;
        assert!(
            outgoing_seq < terminal_seq,
            "outgoing fact must commit before terminal result and ACK"
        );
        let tree = harness
            .agent_store
            .agent(agent_id.as_str())
            .expect("sender tree");
        let outgoing_count = tree
            .all_message_envelopes()
            .filter(|item| item.direction == tau_proto::MessageDirection::Outgoing)
            .count();
        let terminal_count = tree
            .all_entries()
            .filter(|entry| matches!(entry, tau_core::AgentEntry::ToolResults { .. }))
            .count();
        assert_eq!((outgoing_count, terminal_count), (1, 1));
        let ack = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::CompleteTransportSendResult(result)
                    if result.request_id == "send-1" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("completion ack");
        assert!(ack.accepted);
        let canonical_id = ack.message_id.clone();
        harness.handle_complete_transport_send("transport-send", request);
        let tree = harness
            .agent_store
            .agent(agent_id.as_str())
            .expect("sender tree");
        assert_eq!(
            tree.all_message_envelopes()
                .filter(|item| item.direction == tau_proto::MessageDirection::Outgoing)
                .count(),
            1
        );
        assert_eq!(
            tree.all_entries()
                .filter(|entry| matches!(entry, tau_core::AgentEntry::ToolResults { .. }))
                .count(),
            1
        );
        let retry_results = frames
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match &frame.frame {
                HarnessOutputMessage::CompleteTransportSendResult(result)
                    if result.request_id == "send-1" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retry_results.len(), 2);
        assert!(
            retry_results
                .iter()
                .all(|result| result.message_id == canonical_id)
        );
    }

    use super::*;

    fn draft(dedup_key: &str) -> TransportMessageDraft {
        TransportMessageDraft {
            transport_name: "slack".to_owned(),
            external_endpoint: MessageEndpoint::External {
                stable_id: Some("U123".to_owned()),
                display_name: Some("Alice".to_owned()),
                actor_kind: ExternalActorKind::Human,
            },
            conversation: None,
            operation: MessageOperation::Create {
                payload: MessagePayload::Text {
                    text: "hello".to_owned(),
                    format: TextFormat::Plain,
                },
            },
            identity_assurance: SenderIdentityAssurance::VerifiedAccount,
            policy_status: SenderPolicyStatus::Allowlisted,
            external_identity: Some(ExternalMessageIdentity {
                dedup_key: Some(dedup_key.to_owned()),
                ..ExternalMessageIdentity::default()
            }),
            ordering: None,
            occurred_at: None,
            reply_tool: None,
        }
    }

    /// Dedup identity must be scoped to the authenticated extension instance,
    /// preventing one bridge from suppressing another bridge's message.
    #[test]
    fn ingress_dedup_key_is_source_scoped() {
        let input = draft("event-1");
        assert_ne!(
            ingress_dedup_key(&ExtensionName::from("std-slack-a"), &input),
            ingress_dedup_key(&ExtensionName::from("std-slack-b"), &input)
        );
    }

    /// Control characters in a stable identity could corrupt diagnostics and
    /// must fail validation before any durable commit.
    #[test]
    fn ingress_rejects_control_character_dedup_key() {
        assert_eq!(validate_draft(&draft("event\n1")), Err("invalid_dedup_key"));
    }

    /// Persisted envelopes must reconstruct the exact normalized draft so an
    /// identical retry can be acknowledged and a content-conflicting retry can
    /// be rejected after restart.
    #[test]
    fn persisted_envelope_reconstructs_dedup_comparison_draft() {
        let input = draft("event-1");
        let envelope = MessageEnvelope {
            message_id: MessageId::new("msg-1"),
            transport: MessageTransportRef {
                name: input.transport_name.clone(),
                instance: Some(ExtensionName::from("std-slack")),
            },
            source: input.external_endpoint.clone(),
            destination: MessageEndpoint::User,
            conversation: input.conversation.clone(),
            operation: input.operation.clone(),
            trust: MessageTrust {
                content: MessageContentTrust::UntrustedExternal,
                identity: input.identity_assurance,
                policy: input.policy_status,
            },
            external_identity: input.external_identity.clone(),
            ordering: input.ordering,
            occurred_at: input.occurred_at,
            reply_path: None,
        };
        assert_eq!(draft_from_envelope(&envelope), input);
    }
}
