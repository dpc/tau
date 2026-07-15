//! Source-bound canonical transport message intake and send completion.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tau_proto::{
    AgentMessageIncoming, AgentMessageOutgoing, CommittedTransportIngressRoute,
    CompleteTransportSendRequest, CompleteTransportSendResult, Event, ExtensionName,
    HarnessOutputMessage, MessageContentTrust, MessageEndpoint, MessageEnvelope, MessageId,
    MessageOperation, MessageReplyPath, MessageTransportRef, MessageTrust,
    RegisterTransportCapabilityRequest, RegisterTransportCapabilityResult, ReplyPathLifetime,
    ReplySelector, SenderPolicyStatus, ToolName, TransportIngressRejection, TransportMessageDraft,
    TransportMessageIngressDisposition, TransportMessageIngressOutcome,
    TransportMessageIngressRequest, TransportMessageIngressResult, TransportReplyActivation,
    TransportReplyInactiveReason, TransportSendAuthorization,
};

use super::transport_ingress_locator::{LocatorFailure, LocatorLookup, LocatorReservation};
use super::{
    AgentMessageRecipientStatus, CompletedTransportSend, Harness, PendingIngressAck,
    PendingTransportSendAck, TransportCapability, TransportDedupKey, TransportDedupRecord,
    TransportOrderingRouteKey, TransportReplyRoute,
};

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_TRANSPORT_NAME_BYTES: usize = 48;
const MAX_DRAFT_BYTES: usize = 128 * 1024;
/// Maximum encoded metadata retained by one proactive capability registration.
const MAX_SEND_DESTINATIONS_BYTES: usize = 16 * 1024;
/// Maximum distinct transport capabilities retained for one extension peer.
const MAX_CAPABILITIES_PER_SOURCE: usize = 16;
const MAX_DEDUP_KEY_BYTES: usize = 512;
const MAX_CAPABILITY_FIELD_BYTES: usize = 512;
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
                    if let Some(request_seq) = waiter.request_seq {
                        tracing::trace!(
                            schema = "slack_latency_v1",
                            request_seq,
                            activation_to_commit_us = monotonic_us(waiter.activation_started_at),
                            outcome = "commit_failed",
                            "transport.ingress.commit_finished"
                        );
                    }
                    let _ = self.bus.send_to(
                        &waiter.connection_id,
                        None,
                        HarnessOutputMessage::TransportMessageIngressResult(
                            TransportMessageIngressResult {
                                request_id: waiter.request_id,
                                disposition: TransportMessageIngressDisposition::Rejected {
                                    reason: TransportIngressRejection::DurableCommitFailed,
                                },
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
                self.transport_ingress_locator.fail_ambiguous_publish();
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
        if let Some(tool) = &request.send_tool
            && !self
                .registry
                .providers_for(tool.as_str())
                .iter()
                .any(|provider| provider.connection_id.as_str() == source_id)
        {
            return fail("send_tool_not_owned");
        }
        if request.send_destinations.len() > 64
            || (!request.send_destinations.is_empty() && request.send_tool.is_none())
        {
            return fail("invalid_send_destinations");
        }
        let mut aliases = std::collections::HashSet::new();
        let mut native_routes = Vec::new();
        for destination in &request.send_destinations {
            let alias = destination.alias.as_bytes();
            if alias.is_empty()
                || alias.len() > 64
                || !alias[0].is_ascii_lowercase()
                || !alias.iter().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
                || !aliases.insert(destination.alias.as_str())
            {
                return fail("invalid_send_destination_alias");
            }
            let Some(stable_id) = destination.conversation.stable_id.as_deref() else {
                return fail("invalid_send_destination_route");
            };
            let endpoint_fields_valid = match &destination.external_endpoint {
                MessageEndpoint::External {
                    stable_id,
                    display_name,
                    ..
                } => [stable_id.as_deref(), display_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .all(valid_capability_field),
                _ => false,
            };
            if !endpoint_fields_valid
                || !valid_capability_field(stable_id)
                || destination
                    .conversation
                    .display_name
                    .as_deref()
                    .is_some_and(|value| !valid_capability_field(value))
                || destination
                    .conversation
                    .thread
                    .as_ref()
                    .is_some_and(|thread| !valid_capability_field(&thread.stable_id))
                || destination.conversation.reply_to.is_some()
            {
                return fail("invalid_send_destination_route");
            }
            let native_route = (
                stable_id,
                destination
                    .conversation
                    .thread
                    .as_ref()
                    .map(|thread| thread.stable_id.as_str()),
            );
            if native_routes.contains(&native_route) {
                return fail("invalid_send_destination_route");
            }
            native_routes.push(native_route);
            let encoded = match tau_proto::encode_message_to_vec(destination) {
                Ok(encoded) => encoded,
                Err(_) => return fail("invalid_send_destination_metadata"),
            };
            if encoded.len() > MAX_DRAFT_BYTES {
                return fail("send_destination_metadata_too_large");
            }
        }
        let encoded = match tau_proto::encode_message_to_vec(&request.send_destinations) {
            Ok(encoded) => encoded,
            Err(_) => return fail("invalid_send_destinations"),
        };
        if encoded.len() > MAX_SEND_DESTINATIONS_BYTES {
            return fail("send_destinations_too_large");
        }
        let Some(next_capability_epoch) = self.next_transport_capability_epoch.checked_add(1)
        else {
            return fail("transport_capability_epoch_exhausted");
        };
        let registration_epoch = self.next_transport_capability_epoch;
        self.next_transport_capability_epoch = next_capability_epoch;
        let capability = TransportCapability {
            transport_name: request.transport_name.clone(),
            send_tool: request.send_tool,
            session_generation: self.current_session_generation,
            registration_epoch,
            send_destinations: request.send_destinations,
        };
        let capabilities = self
            .transport_capabilities
            .entry(source_id.to_owned())
            .or_default();
        if capabilities.len() >= MAX_CAPABILITIES_PER_SOURCE
            && !capabilities
                .iter()
                .any(|existing| existing.transport_name == capability.transport_name)
        {
            return fail("transport_capability_limit_reached");
        }
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

    pub(super) fn revoke_transport_send_tool(&mut self, source_id: &str, tool_name: &ToolName) {
        if let Some(capabilities) = self.transport_capabilities.get_mut(source_id) {
            capabilities.retain(|capability| capability.send_tool.as_ref() != Some(tool_name));
        }
        self.transport_reply_routes.retain(|_, route| {
            route.connection_id != source_id || route.send_tool.as_ref() != Some(tool_name)
        });
    }

    pub(super) fn handle_transport_message_ingress(
        &mut self,
        source_id: &str,
        request: TransportMessageIngressRequest,
    ) {
        let transport_class = if request.draft.transport_name == "slack" {
            "slack"
        } else {
            "other"
        };
        let request_seq = slack_request_seq(&request);
        let started_at = Instant::now();
        if let Some(request_seq) = request_seq {
            tracing::trace!(
                schema = "slack_latency_v1",
                request_seq,
                transport_class,
                "transport.ingress.activation_started"
            );
        }
        let result =
            self.begin_transport_message_ingress(source_id, request, started_at, request_seq);
        let outcome = match &result {
            Ok(()) => "published",
            Err(TransportMessageIngressResult {
                disposition:
                    TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: TransportMessageIngressOutcome::Duplicate,
                        canonical: _,
                        reply_activation: _,
                    },
                request_id: _,
            }) => "duplicate_committed",
            Err(_) => "rejected",
        };
        if let Some(request_seq) = request_seq {
            tracing::trace!(
                schema = "slack_latency_v1",
                request_seq,
                transport_class,
                duration_us = monotonic_us(started_at),
                outcome,
                "transport.ingress.activation_finished"
            );
        }
        if let Err(result) = result {
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
        activation_started_at: Instant,
        request_seq: Option<u64>,
    ) -> Result<(), TransportMessageIngressResult> {
        let fail = |reason| TransportMessageIngressResult {
            request_id: request.request_id.clone(),
            disposition: TransportMessageIngressDisposition::Rejected { reason },
        };
        if !valid_request_id(&request.request_id) {
            return Err(fail(TransportIngressRejection::InvalidRequest));
        }
        let Some(extension_name) = self.transport_source_name(source_id) else {
            return Err(fail(TransportIngressRejection::UnauthorizedSource));
        };
        let capability = self.transport_capability(source_id, &request.draft);
        if validate_draft(&request.draft).is_err() {
            return Err(fail(TransportIngressRejection::InvalidRequest));
        }
        if request.draft.identity_assurance
            == tau_proto::SenderIdentityAssurance::AuthenticatedTauAgent
            || request.draft.policy_status == SenderPolicyStatus::Internal
        {
            return Err(fail(TransportIngressRejection::InvalidRequest));
        }
        let dedup_key = ingress_dedup_key(&extension_name, &request.draft)
            .ok_or_else(|| fail(TransportIngressRejection::InvalidRequest))?;
        if !self.transport_dedup.contains_key(&dedup_key) {
            match self
                .transport_ingress_locator
                .lookup(&self.agent_store, &dedup_key)
            {
                Ok(LocatorLookup::Missing) => {}
                Ok(LocatorLookup::Found(record)) => {
                    if !self.insert_transport_dedup(dedup_key.clone(), *record) {
                        return Err(fail(TransportIngressRejection::CapacityExceeded));
                    }
                }
                Err(error) => return Err(fail(locator_rejection(error))),
            }
        }
        if let Some(existing) = self.transport_dedup.get(&dedup_key) {
            if !drafts_authority_equal(&existing.draft, &request.draft)
                || existing.target_agent_id != request.target_agent_id
            {
                return Err(fail(TransportIngressRejection::DedupConflict));
            }
            if existing.committed {
                let existing = existing.clone();
                let activation = self.activate_duplicate_route(
                    source_id,
                    &extension_name,
                    capability.as_ref(),
                    &existing,
                );
                return Err(committed_result(
                    request.request_id,
                    &extension_name,
                    &existing,
                    TransportMessageIngressOutcome::Duplicate,
                    activation,
                ));
            }
            let Some(capability) = capability else {
                return Err(fail(TransportIngressRejection::InactiveCapability));
            };
            self.pending_ingress_acks
                .entry(existing.message_id.clone())
                .or_default()
                .push(PendingIngressAck {
                    connection_id: source_id.to_owned(),
                    request_id: request.request_id,
                    session_generation: self.current_session_generation,
                    capability_epoch: capability.registration_epoch,
                    outcome: TransportMessageIngressOutcome::Duplicate,
                    activation_started_at,
                    request_seq,
                });
            return Ok(());
        }
        let Some(capability) = capability else {
            return Err(fail(TransportIngressRejection::InactiveCapability));
        };
        if self.agent_message_recipient_status(request.target_agent_id.as_str())
            != AgentMessageRecipientStatus::Live
        {
            return Err(fail(TransportIngressRejection::InactiveTarget));
        }
        if self
            .agent_store
            .agent_is_ephemeral(&request.target_agent_id)
        {
            return Err(fail(TransportIngressRejection::DurableCommitFailed));
        }
        let ordering_reservation = if let Some(ordering) = request.draft.ordering {
            let route_key = ordering_route_key(&extension_name, &request.draft);
            if let Err(reason) = self.restore_route_sequence(
                &route_key,
                &extension_name,
                &request.target_agent_id,
                &request.draft,
            ) {
                return Err(fail(reason));
            }
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
                return Err(fail(TransportIngressRejection::OrderingConflict));
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
            .send_tool
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
                instance: Some(extension_name.clone()),
            },
            source: request.draft.external_endpoint.clone(),
            destination: MessageEndpoint::Agent {
                session_id: Some(self.current_session_id.clone()),
                agent_id: request.target_agent_id.clone(),
                display_name: None,
            },
            conversation: request.draft.conversation.clone(),
            operation: request.draft.operation.clone(),
            transport_identity_mentioned: request.draft.transport_identity_mentioned,
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
        let candidate_record = TransportDedupRecord {
            draft: request.draft.clone(),
            target_agent_id: request.target_agent_id.clone(),
            message_id: message_id.clone(),
            committed: false,
            session_id: self.current_session_id.clone(),
        };
        let mut locator_record = candidate_record.clone();
        locator_record.committed = true;
        match self
            .transport_ingress_locator
            .reserve(&self.agent_store, &dedup_key, &locator_record)
        {
            Ok(LocatorReservation::Reserved) => {}
            Ok(LocatorReservation::Found(existing)) => {
                rollback_ordering_reservation(
                    &mut self.pending_transport_route_sequences,
                    &envelope,
                    &message_id,
                );
                if !drafts_authority_equal(&existing.draft, &request.draft)
                    || existing.target_agent_id != request.target_agent_id
                {
                    return Err(fail(TransportIngressRejection::DedupConflict));
                }
                let activation = self.activate_duplicate_route(
                    source_id,
                    &extension_name,
                    Some(&capability),
                    &existing,
                );
                return Err(committed_result(
                    request.request_id,
                    &extension_name,
                    &existing,
                    TransportMessageIngressOutcome::Duplicate,
                    activation,
                ));
            }
            Err(error) => {
                rollback_ordering_reservation(
                    &mut self.pending_transport_route_sequences,
                    &envelope,
                    &message_id,
                );
                return Err(fail(locator_rejection(error)));
            }
        }
        if !self.insert_transport_dedup(dedup_key.clone(), candidate_record) {
            self.transport_ingress_locator.cancel_reservation();
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
            return Err(fail(TransportIngressRejection::CapacityExceeded));
        }
        self.pending_ingress_acks
            .entry(message_id)
            .or_default()
            .push(PendingIngressAck {
                connection_id: source_id.to_owned(),
                request_id: request.request_id,
                session_generation: self.current_session_generation,
                capability_epoch: capability.registration_epoch,
                outcome: TransportMessageIngressOutcome::Accepted,
                activation_started_at,
                request_seq,
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
        let Some(extension_name) = message.envelope.transport.instance.as_ref() else {
            return false;
        };
        let Some(key) = ingress_dedup_key(extension_name, &draft_from_envelope(&message.envelope))
        else {
            return false;
        };
        let Some(record) = self
            .transport_dedup
            .get(&key)
            .filter(|record| record.message_id == message.envelope.message_id)
            .cloned()
        else {
            return false;
        };
        let mut committed_record = record.clone();
        committed_record.committed = true;
        if let Err(error) = self
            .transport_ingress_locator
            .commit(key.clone(), committed_record.clone())
        {
            self.transport_dedup.remove(&key);
            self.transport_dedup_order
                .retain(|candidate| candidate != &key);
            self.reject_ingress_waiters(&message.envelope.message_id, locator_rejection(error));
            return false;
        }
        if let Some(runtime_record) = self.transport_dedup.get_mut(&key) {
            runtime_record.committed = true;
        }
        let waiters = self
            .pending_ingress_acks
            .remove(&message.envelope.message_id)
            .unwrap_or_default();
        let inactive_reasons = waiters
            .iter()
            .map(|waiter| self.waiter_inactive_reason(waiter, message, extension_name))
            .collect::<Vec<_>>();
        let active_index = inactive_reasons.iter().position(Option::is_none);
        if let Some(index) = active_index {
            let waiter = &waiters[index];
            let send_tool = message
                .envelope
                .reply_path
                .as_ref()
                .map(|path| path.tool_name.clone())
                .expect("current route owner requires a reply path");
            self.transport_reply_routes.insert(
                message.envelope.message_id.clone(),
                TransportReplyRoute {
                    connection_id: waiter.connection_id.clone(),
                    agent_id: message.recipient_id.clone(),
                    session_generation: waiter.session_generation,
                    send_tool: Some(send_tool),
                    transport_name: message.envelope.transport.name.clone(),
                    external_endpoint: message.envelope.source.clone(),
                    conversation: message.envelope.conversation.clone(),
                },
            );
        }
        let live_commit = waiters.iter().any(|waiter| {
            waiter.session_generation == self.current_session_generation
                && self.bus.connection(&waiter.connection_id).is_some()
        });
        for (index, waiter) in waiters.into_iter().enumerate() {
            if let Some(request_seq) = waiter.request_seq {
                tracing::trace!(
                    schema = "slack_latency_v1",
                    request_seq,
                    activation_to_commit_us = monotonic_us(waiter.activation_started_at),
                    outcome = if waiter.outcome == TransportMessageIngressOutcome::Accepted {
                        "accepted"
                    } else {
                        "duplicate"
                    },
                    "transport.ingress.commit_finished"
                );
            }
            let reply_activation = if active_index == Some(index) {
                TransportReplyActivation::Active
            } else {
                TransportReplyActivation::Inactive(
                    inactive_reasons[index]
                        .unwrap_or(TransportReplyInactiveReason::SupersededWaiter),
                )
            };
            let _ = self.bus.send_to(
                &waiter.connection_id,
                None,
                HarnessOutputMessage::TransportMessageIngressResult(
                    TransportMessageIngressResult {
                        request_id: waiter.request_id,
                        disposition: TransportMessageIngressDisposition::Committed {
                            message_id: message.envelope.message_id.clone(),
                            outcome: waiter.outcome,
                            canonical: Box::new(committed_route_from_message(message)),
                            reply_activation,
                        },
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
        let route_authorized = match &request.authorization {
            tau_proto::TransportSendAuthorization::Reply { message_id } => {
                if request.in_reply_to.as_ref() != Some(message_id) {
                    false
                } else if let Some(route) = self.transport_reply_routes.get(message_id) {
                    route.connection_id == source_id
                        && route.agent_id == request.agent_id
                        && route.session_generation == self.current_session_generation
                        && route.send_tool.as_ref() == Some(&request.tool_result.tool_name)
                        && route.transport_name == request.draft.transport_name
                        && route.external_endpoint == request.draft.external_endpoint
                        && route.conversation == request.draft.conversation
                        && request.draft.send_tool.as_ref() == route.send_tool.as_ref()
                } else {
                    false
                }
            }
            tau_proto::TransportSendAuthorization::ConfiguredDestination { alias } => {
                request.in_reply_to.is_none()
                    && self
                        .transport_capabilities
                        .get(source_id)
                        .is_some_and(|caps| {
                            caps.iter().any(|cap| {
                                cap.session_generation == self.current_session_generation
                                    && cap.transport_name == request.draft.transport_name
                                    && cap.send_tool.as_ref()
                                        == Some(&request.tool_result.tool_name)
                                    && cap.send_destinations.iter().any(|destination| {
                                        destination.alias == *alias
                                            && destination.external_endpoint
                                                == request.draft.external_endpoint
                                            && Some(&destination.conversation)
                                                == request.draft.conversation.as_ref()
                                    })
                            })
                        })
            }
        };
        if !route_authorized {
            return Err(fail("send_route_not_authorized"));
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
            transport_identity_mentioned: false,
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
                configured_destination: match &request.authorization {
                    TransportSendAuthorization::ConfiguredDestination { alias } => {
                        Some(alias.clone())
                    }
                    TransportSendAuthorization::Reply { .. } => None,
                },
                tool_call_id: Some(request.call_id),
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
                    && capability.send_tool == draft.send_tool)
                    .then(|| capability.clone())
            })
    }

    fn transport_source_name(&self, source_id: &str) -> Option<ExtensionName> {
        let metadata = self.bus.connection(source_id)?;
        (!matches!(metadata.kind, tau_proto::ClientKind::Ui))
            .then(|| ExtensionName::from(metadata.name.clone()))
    }

    fn activate_duplicate_route(
        &mut self,
        source_id: &str,
        extension_name: &ExtensionName,
        capability: Option<&TransportCapability>,
        record: &TransportDedupRecord,
    ) -> TransportReplyActivation {
        let reason = if record.draft.send_tool.is_none() {
            Some(TransportReplyInactiveReason::NoReplyPath)
        } else if record.session_id != self.current_session_id {
            Some(TransportReplyInactiveReason::NonCurrentSession)
        } else if self.bus.connection(source_id).is_none() {
            Some(TransportReplyInactiveReason::InactiveConnection)
        } else if self.agent_message_recipient_status(record.target_agent_id.as_str())
            != AgentMessageRecipientStatus::Live
        {
            Some(TransportReplyInactiveReason::InactiveTarget)
        } else if !capability.is_some_and(|capability| {
            capability.session_generation == self.current_session_generation
                && capability.send_tool == record.draft.send_tool
        }) || self.transport_source_name(source_id).as_ref() != Some(extension_name)
        {
            Some(TransportReplyInactiveReason::InactiveCapability)
        } else {
            None
        };
        if let Some(reason) = reason {
            return TransportReplyActivation::Inactive(reason);
        }
        self.transport_reply_routes.insert(
            record.message_id.clone(),
            TransportReplyRoute {
                connection_id: source_id.to_owned(),
                agent_id: record.target_agent_id.clone(),
                session_generation: self.current_session_generation,
                send_tool: record.draft.send_tool.clone(),
                transport_name: record.draft.transport_name.clone(),
                external_endpoint: record.draft.external_endpoint.clone(),
                conversation: record.draft.conversation.clone(),
            },
        );
        TransportReplyActivation::Active
    }

    fn waiter_inactive_reason(
        &self,
        waiter: &PendingIngressAck,
        message: &AgentMessageIncoming,
        extension_name: &ExtensionName,
    ) -> Option<TransportReplyInactiveReason> {
        if message.envelope.reply_path.is_none() {
            return Some(TransportReplyInactiveReason::NoReplyPath);
        }
        let canonical_session = match &message.envelope.destination {
            MessageEndpoint::Agent {
                session_id: Some(session_id),
                agent_id: _,
                display_name: _,
            } => session_id,
            MessageEndpoint::Agent {
                session_id: None,
                agent_id: _,
                display_name: _,
            }
            | MessageEndpoint::External {
                stable_id: _,
                display_name: _,
                identity_alias: _,
                actor_kind: _,
            }
            | MessageEndpoint::User => {
                return Some(TransportReplyInactiveReason::NonCurrentSession);
            }
        };
        if canonical_session != &self.current_session_id {
            return Some(TransportReplyInactiveReason::NonCurrentSession);
        }
        if waiter.session_generation != self.current_session_generation {
            return Some(TransportReplyInactiveReason::NonCurrentGeneration);
        }
        if self.bus.connection(&waiter.connection_id).is_none() {
            return Some(TransportReplyInactiveReason::InactiveConnection);
        }
        if self.agent_message_recipient_status(message.recipient_id.as_str())
            != AgentMessageRecipientStatus::Live
        {
            return Some(TransportReplyInactiveReason::InactiveTarget);
        }
        if self.transport_source_name(&waiter.connection_id).as_ref() != Some(extension_name)
            || !message.envelope.reply_path.as_ref().is_some_and(|path| {
                self.transport_capabilities
                    .get(&waiter.connection_id)
                    .into_iter()
                    .flatten()
                    .any(|capability| {
                        capability.session_generation == self.current_session_generation
                            && capability.registration_epoch == waiter.capability_epoch
                            && capability.transport_name == message.envelope.transport.name
                            && capability.send_tool.as_ref() == Some(&path.tool_name)
                    })
            })
        {
            return Some(TransportReplyInactiveReason::InactiveCapability);
        }
        None
    }

    fn reject_ingress_waiters(
        &mut self,
        message_id: &MessageId,
        reason: TransportIngressRejection,
    ) {
        for waiter in self
            .pending_ingress_acks
            .remove(message_id)
            .unwrap_or_default()
        {
            let _ = self.bus.send_to(
                &waiter.connection_id,
                None,
                HarnessOutputMessage::TransportMessageIngressResult(
                    TransportMessageIngressResult {
                        request_id: waiter.request_id,
                        disposition: TransportMessageIngressDisposition::Rejected { reason },
                    },
                ),
            );
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
    ) -> Result<(), TransportIngressRejection> {
        if self.transport_route_sequences.contains_key(route_key) {
            return Ok(());
        }
        let _ = target_agent_id;
        let max_sequence = self
            .transport_ingress_locator
            .max_source_sequence(extension_name, draft)
            .map_err(locator_rejection)?;
        if let Some(sequence) = max_sequence {
            self.transport_route_sequences
                .insert(route_key.clone(), sequence);
        }
        Ok(())
    }
}

/// Convert a harness-local monotonic interval to a saturating microsecond
/// field.
fn monotonic_us(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Extract only the Slack extension's bounded process-local request ordinal.
///
/// Arbitrary extension-controlled request identifiers are never logged.
fn slack_request_seq(request: &TransportMessageIngressRequest) -> Option<u64> {
    if request.draft.transport_name != "slack" {
        return None;
    }
    let digits = request.request_id.strip_prefix("slack-in-")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
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

fn valid_capability_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CAPABILITY_FIELD_BYTES
        && !value.chars().any(char::is_control)
}

fn validate_draft(draft: &TransportMessageDraft) -> Result<(), &'static str> {
    if !valid_transport_name(&draft.transport_name) {
        return Err("invalid_transport_name");
    }
    if !matches!(draft.external_endpoint, MessageEndpoint::External { .. }) {
        return Err("external_endpoint_required");
    }
    if let MessageEndpoint::External {
        stable_id,
        identity_alias: Some(alias),
        ..
    } = &draft.external_endpoint
        && (stable_id.is_none()
            || draft.identity_assurance != tau_proto::SenderIdentityAssurance::VerifiedAccount
            || !valid_identity_alias(&alias.value))
    {
        return Err("invalid_identity_alias");
    }
    if draft.transport_identity_mentioned
        && !matches!(
            &draft.operation,
            MessageOperation::Create { .. } | MessageOperation::Edit { .. }
        )
    {
        return Err("invalid_transport_identity_mention");
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

fn valid_identity_alias(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
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

/// Reconstructs the exact normalized first draft from a canonical envelope.
pub(super) fn draft_from_envelope(envelope: &MessageEnvelope) -> TransportMessageDraft {
    TransportMessageDraft {
        transport_name: envelope.transport.name.clone(),
        external_endpoint: envelope.source.clone(),
        conversation: envelope.conversation.clone(),
        operation: envelope.operation.clone(),
        transport_identity_mentioned: envelope.transport_identity_mentioned,
        identity_assurance: envelope.trust.identity,
        policy_status: envelope.trust.policy,
        external_identity: envelope.external_identity.clone(),
        ordering: envelope.ordering,
        occurred_at: envelope.occurred_at,
        send_tool: envelope
            .reply_path
            .as_ref()
            .map(|path| path.tool_name.clone()),
    }
}

fn locator_rejection(failure: LocatorFailure) -> TransportIngressRejection {
    match failure {
        LocatorFailure::Unavailable => TransportIngressRejection::CanonicalUnavailable,
        LocatorFailure::Ambiguous => TransportIngressRejection::CanonicalAmbiguous,
        LocatorFailure::Pruned => TransportIngressRejection::CanonicalPruned,
        LocatorFailure::Capacity => TransportIngressRejection::CapacityExceeded,
        LocatorFailure::Durable => TransportIngressRejection::DurableCommitFailed,
        LocatorFailure::Ordering => TransportIngressRejection::OrderingConflict,
    }
}

fn rollback_ordering_reservation(
    pending: &mut std::collections::HashMap<
        TransportOrderingRouteKey,
        std::collections::HashMap<MessageId, u64>,
    >,
    envelope: &MessageEnvelope,
    message_id: &MessageId,
) {
    let (Some(extension_name), Some(_)) = (envelope.transport.instance.as_ref(), envelope.ordering)
    else {
        return;
    };
    let route_key = ordering_route_key_from_envelope(extension_name, envelope);
    if let Some(sequences) = pending.get_mut(&route_key) {
        sequences.remove(message_id);
        if sequences.is_empty() {
            pending.remove(&route_key);
        }
    }
}

fn committed_result(
    request_id: String,
    extension_name: &ExtensionName,
    record: &TransportDedupRecord,
    outcome: TransportMessageIngressOutcome,
    reply_activation: TransportReplyActivation,
) -> TransportMessageIngressResult {
    TransportMessageIngressResult {
        request_id,
        disposition: TransportMessageIngressDisposition::Committed {
            message_id: record.message_id.clone(),
            outcome,
            canonical: Box::new(committed_route_from_draft(
                &record.target_agent_id,
                extension_name,
                &record.draft,
            )),
            reply_activation,
        },
    }
}

fn committed_route_from_draft(
    target_agent_id: &tau_proto::AgentId,
    extension_name: &ExtensionName,
    draft: &TransportMessageDraft,
) -> CommittedTransportIngressRoute {
    CommittedTransportIngressRoute {
        target_agent_id: target_agent_id.clone(),
        transport: MessageTransportRef {
            name: draft.transport_name.clone(),
            instance: Some(extension_name.clone()),
        },
        external_endpoint: draft.external_endpoint.clone(),
        conversation: draft.conversation.clone(),
        external_identity: draft.external_identity.clone().unwrap_or_default(),
        identity_assurance: draft.identity_assurance,
        policy_status: draft.policy_status,
    }
}

fn committed_route_from_message(message: &AgentMessageIncoming) -> CommittedTransportIngressRoute {
    CommittedTransportIngressRoute {
        target_agent_id: message.recipient_id.clone(),
        transport: message.envelope.transport.clone(),
        external_endpoint: message.envelope.source.clone(),
        conversation: message.envelope.conversation.clone(),
        external_identity: message
            .envelope
            .external_identity
            .clone()
            .unwrap_or_default(),
        identity_assurance: message.envelope.trust.identity,
        policy_status: message.envelope.trust.policy,
    }
}

#[derive(Eq, PartialEq)]
/// Immutable ingress authority projection excluding only presentation labels.
struct IngressAuthority<'a> {
    /// Stable transport family.
    transport_name: &'a str,
    /// Stable external endpoint identity.
    endpoint: ExternalEndpointAuthority<'a>,
    /// Stable conversation and thread identity.
    conversation: Option<ConversationAuthority<'a>>,
    /// Exact immutable operation and payload.
    operation: &'a tau_proto::MessageOperation,
    /// Whether text addressed the transport's receiving identity.
    transport_identity_mentioned: bool,
    /// Canonical identity assurance.
    identity_assurance: tau_proto::SenderIdentityAssurance,
    /// Canonical routing policy.
    policy_status: SenderPolicyStatus,
    /// Exact native occurrence identity.
    external_identity: &'a Option<tau_proto::ExternalMessageIdentity>,
    /// Exact source ordering.
    ordering: &'a Option<tau_proto::MessageOrdering>,
    /// Exact claimed occurrence time.
    occurred_at: &'a Option<tau_proto::UnixMicros>,
    /// Exact reply tool authority.
    send_tool: &'a Option<ToolName>,
}

#[derive(Eq, PartialEq)]
/// Presentation-free external endpoint authority.
enum ExternalEndpointAuthority<'a> {
    /// Valid stable external actor.
    External {
        /// Transport-stable actor id.
        stable_id: &'a Option<String>,
        /// Transport actor class.
        actor_kind: tau_proto::ExternalActorKind,
    },
    /// A non-external endpoint, rejected by draft validation.
    Invalid,
}

#[derive(Eq, PartialEq)]
/// Presentation-free conversation authority.
struct ConversationAuthority<'a> {
    /// Conversation family.
    kind: tau_proto::ConversationKind,
    /// Transport-stable conversation id.
    stable_id: &'a Option<String>,
    /// Exact thread relation.
    thread: &'a Option<tau_proto::MessageThread>,
    /// Exact immediate reply relation.
    reply_to: &'a Option<tau_proto::MessageRef>,
}

fn draft_authority(draft: &TransportMessageDraft) -> IngressAuthority<'_> {
    let endpoint = match &draft.external_endpoint {
        MessageEndpoint::External {
            stable_id,
            display_name: _,
            identity_alias: _,
            actor_kind,
        } => ExternalEndpointAuthority::External {
            stable_id,
            actor_kind: *actor_kind,
        },
        MessageEndpoint::Agent {
            session_id: _,
            agent_id: _,
            display_name: _,
        }
        | MessageEndpoint::User => ExternalEndpointAuthority::Invalid,
    };
    let conversation = draft.conversation.as_ref().map(|conversation| {
        let tau_proto::MessageConversation {
            kind,
            stable_id,
            display_name: _,
            thread,
            reply_to,
        } = conversation;
        ConversationAuthority {
            kind: *kind,
            stable_id,
            thread,
            reply_to,
        }
    });
    IngressAuthority {
        transport_name: &draft.transport_name,
        endpoint,
        conversation,
        operation: &draft.operation,
        transport_identity_mentioned: draft.transport_identity_mentioned,
        identity_assurance: draft.identity_assurance,
        policy_status: draft.policy_status,
        external_identity: &draft.external_identity,
        ordering: &draft.ordering,
        occurred_at: &draft.occurred_at,
        send_tool: &draft.send_tool,
    }
}

fn drafts_authority_equal(left: &TransportMessageDraft, right: &TransportMessageDraft) -> bool {
    draft_authority(left) == draft_authority(right)
}

fn mint_message_id() -> MessageId {
    MessageId::new(format!(
        "msg-{}-{}-{:032x}",
        tau_proto::UnixMicros::now(),
        NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed),
        rand::random::<u128>(),
    ))
}

#[cfg(test)]
mod tests {
    use tau_proto::{
        ExternalActorKind, ExternalMessageIdentity, MessageOperation, MessagePayload, MessageRef,
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
                send_tool: None,
                send_destinations: Vec::new(),
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
        let first_id = match &first.disposition {
            TransportMessageIngressDisposition::Committed {
                message_id,
                outcome: TransportMessageIngressOutcome::Accepted,
                canonical: _,
                reply_activation: _,
            } => message_id.clone(),
            disposition => panic!("unexpected first disposition: {disposition:?}"),
        };

        let mut retry = request;
        retry.request_id = "ingress-2".to_owned();
        if let MessageEndpoint::External {
            stable_id: _,
            display_name,
            identity_alias: _,
            actor_kind: _,
        } = &mut retry.draft.external_endpoint
        {
            *display_name = Some("Renamed Alice".to_owned());
        }
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
        match &duplicate.disposition {
            TransportMessageIngressDisposition::Committed {
                message_id,
                outcome: TransportMessageIngressOutcome::Duplicate,
                canonical,
                reply_activation: _,
            } => {
                assert_eq!(message_id, &first_id);
                assert!(matches!(
                    canonical.external_endpoint,
                    MessageEndpoint::External {
                        stable_id: _,
                        display_name: Some(ref display),
                        identity_alias: None,
                        actor_kind: _,
                    } if display == "Alice"
                ));
            }
            disposition => panic!("unexpected duplicate disposition: {disposition:?}"),
        }
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
                send_tool: None,
                send_destinations: Vec::new(),
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
        assert_eq!(
            rejected.disposition,
            TransportMessageIngressDisposition::Rejected {
                reason: TransportIngressRejection::CapacityExceeded,
            }
        );
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
        assert!(matches!(
            accepted.disposition,
            TransportMessageIngressDisposition::Committed {
                outcome: TransportMessageIngressOutcome::Accepted,
                ..
            }
        ));
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
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
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
                allows_provider_image: false,
            },
        );
        harness.pending_tool_providers.insert(
            call_id.clone(),
            tau_proto::ConnectionId::from("transport-send"),
        );
        let endpoint = MessageEndpoint::External {
            stable_id: None,
            display_name: Some("team-ops".to_owned()),
            identity_alias: None,
            actor_kind: ExternalActorKind::Unknown,
        };
        let conversation = tau_proto::MessageConversation {
            kind: tau_proto::ConversationKind::Channel,
            stable_id: Some("C1".to_owned()),
            display_name: Some("team-ops".to_owned()),
            thread: None,
            reply_to: None,
        };
        harness.transport_capabilities.insert(
            "transport-send".to_owned(),
            vec![TransportCapability {
                transport_name: "slack".to_owned(),
                send_tool: Some(tool_name.clone()),
                session_generation: harness.current_session_generation,
                registration_epoch: 1,
                send_destinations: vec![tau_proto::TransportSendDestinationCapability {
                    alias: "team-ops".to_owned(),
                    external_endpoint: endpoint.clone(),
                    conversation: conversation.clone(),
                }],
            }],
        );
        let request = CompleteTransportSendRequest {
            request_id: "send-1".to_owned(),
            call_id: call_id.clone(),
            agent_id: agent_id.clone(),
            in_reply_to: None,
            authorization: tau_proto::TransportSendAuthorization::ConfiguredDestination {
                alias: "team-ops".to_owned(),
            },
            draft: TransportMessageDraft {
                transport_name: "slack".to_owned(),
                external_endpoint: endpoint,
                conversation: Some(conversation),
                operation: MessageOperation::Create {
                    payload: MessagePayload::Text {
                        text: "reply".to_owned(),
                        format: TextFormat::Plain,
                    },
                },
                transport_identity_mentioned: false,
                identity_assurance: SenderIdentityAssurance::Unknown,
                policy_status: SenderPolicyStatus::Allowlisted,
                external_identity: None,
                ordering: None,
                occurred_at: None,
                send_tool: Some(tool_name.clone()),
            },
            acceptance: tau_proto::MessageTransportAcceptance::SubmittedToTransport,
            tool_result: tau_proto::ToolResult {
                call_id: call_id.clone(),
                tool_name,
                tool_type: tau_proto::ToolType::Function,
                result: tau_proto::CborValue::Text("ok".to_owned()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            },
        };
        harness.handle_complete_transport_send("transport-send", request.clone());
        let committed = harness.event_log.entries_for_test();
        let outgoing = committed
            .iter()
            .find_map(|entry| match &entry.event {
                Event::AgentMessageOutgoing(outgoing) => Some(outgoing),
                _ => None,
            })
            .expect("outgoing audit fact");
        assert_eq!(outgoing.configured_destination.as_deref(), Some("team-ops"));
        assert_eq!(outgoing.tool_call_id.as_ref(), Some(&call_id));
        assert!(outgoing.in_reply_to.is_none());
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
                identity_alias: None,
                actor_kind: ExternalActorKind::Human,
            },
            conversation: None,
            operation: MessageOperation::Create {
                payload: MessagePayload::Text {
                    text: "hello".to_owned(),
                    format: TextFormat::Plain,
                },
            },
            transport_identity_mentioned: false,
            identity_assurance: SenderIdentityAssurance::VerifiedAccount,
            policy_status: SenderPolicyStatus::Allowlisted,
            external_identity: Some(ExternalMessageIdentity {
                dedup_key: Some(dedup_key.to_owned()),
                ..ExternalMessageIdentity::default()
            }),
            ordering: None,
            occurred_at: None,
            send_tool: None,
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

    /// Canonical selectors include process-random entropy so independent
    /// harness daemons cannot collide through equal clocks and local
    /// counters.
    #[test]
    fn minted_message_ids_are_collision_resistant() {
        let ids = (0..10_000)
            .map(|_| mint_message_id())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 10_000);
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
        let mut input = draft("event-1");
        input.transport_identity_mentioned = true;
        input.operation = MessageOperation::Create {
            payload: MessagePayload::Text {
                text: "hello @slack_bridge".to_owned(),
                format: TextFormat::Plain,
            },
        };
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
            transport_identity_mentioned: input.transport_identity_mentioned,
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

    /// Presentation retries may reuse the first canonical occurrence, while
    /// every stable route, trust, operation, ordering, time, and tool field
    /// remains part of immutable dedup authority.
    #[test]
    fn ingress_authority_projection_excludes_only_presentation() {
        let mut original = draft("event-1");
        original.conversation = Some(tau_proto::MessageConversation {
            kind: tau_proto::ConversationKind::Channel,
            stable_id: Some("C1".to_owned()),
            display_name: Some("first".to_owned()),
            thread: Some(tau_proto::MessageThread {
                stable_id: "1.0".to_owned(),
                root: None,
            }),
            reply_to: None,
        });
        let mut retry = original.clone();
        if let MessageEndpoint::External {
            stable_id: _,
            display_name,
            identity_alias,
            actor_kind: _,
        } = &mut retry.external_endpoint
        {
            *display_name = Some("renamed".to_owned());
            *identity_alias = Some(tau_proto::ExternalIdentityAlias {
                value: "renamed-alias".to_owned(),
                authority: tau_proto::ExternalIdentityAliasAuthority::OperatorConfigured,
            });
        }
        retry
            .conversation
            .as_mut()
            .expect("conversation")
            .display_name = Some("renamed-channel".to_owned());
        assert!(drafts_authority_equal(&original, &retry));
        assert_eq!(validate_draft(&retry), Ok(()));

        let mut invalid_alias = original.clone();
        if let MessageEndpoint::External { identity_alias, .. } =
            &mut invalid_alias.external_endpoint
        {
            *identity_alias = Some(tau_proto::ExternalIdentityAlias {
                value: "Uppercase".to_owned(),
                authority: tau_proto::ExternalIdentityAliasAuthority::OperatorConfigured,
            });
        }
        assert_eq!(
            validate_draft(&invalid_alias),
            Err("invalid_identity_alias")
        );

        let mut changed = original.clone();
        changed.transport_identity_mentioned = true;
        assert!(!drafts_authority_equal(&original, &changed));

        let mut invalid = original.clone();
        invalid.transport_identity_mentioned = true;
        invalid.operation = MessageOperation::Delete {
            target: MessageRef::default(),
        };
        assert_eq!(
            validate_draft(&invalid),
            Err("invalid_transport_identity_mention")
        );

        let mut changed = original.clone();
        if let MessageEndpoint::External {
            stable_id,
            display_name: _,
            identity_alias: _,
            actor_kind: _,
        } = &mut changed.external_endpoint
        {
            *stable_id = Some("U999".to_owned());
        }
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        if let MessageEndpoint::External {
            stable_id: _,
            display_name: _,
            identity_alias: _,
            actor_kind,
        } = &mut changed.external_endpoint
        {
            *actor_kind = ExternalActorKind::Bot;
        }
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed
            .conversation
            .as_mut()
            .expect("conversation")
            .stable_id = Some("C2".to_owned());
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.conversation.as_mut().expect("conversation").kind =
            tau_proto::ConversationKind::Group;
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed
            .external_identity
            .as_mut()
            .expect("identity")
            .event_id = Some("Ev2".to_owned());
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.operation = MessageOperation::Delete {
            target: tau_proto::MessageRef::default(),
        };
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.identity_assurance = SenderIdentityAssurance::DisplayOnly;
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.policy_status = SenderPolicyStatus::LaxPermitted;
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.ordering = Some(tau_proto::MessageOrdering { source_sequence: 1 });
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original.clone();
        changed.occurred_at = Some(tau_proto::UnixMicros::new(1));
        assert!(!drafts_authority_equal(&original, &changed));
        let mut changed = original;
        changed.send_tool = Some(ToolName::new("slack_send"));
        assert!(!drafts_authority_equal(&retry, &changed));
    }

    /// A clean cold reopen must consult all retained agent history before
    /// target liveness, so a retry cannot move an occurrence from stopped A
    /// to live B.
    #[test]
    fn cold_reopen_preserves_original_target_across_agents() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let (agent_a, agent_b) = {
            let mut harness =
                super::super::tests::echo_harness(temp.path()).expect("first harness");
            harness
                .initialized_sessions
                .insert(harness.current_session_id.clone());
            let _frames = super::super::tests::connect_test_tool(&mut harness, "transport-cold");
            let cid_a = super::super::tests::ensure_test_user_agent(&mut harness);
            let agent_a = harness
                .agents
                .get(&cid_a)
                .and_then(|agent| agent.agent_id.as_deref())
                .map(crate::parse_agent_id)
                .expect("agent A");
            let session_id = harness.current_session_id.clone();
            let role = harness.selected_role.clone();
            let cid_b = harness.create_durable_user_agent(session_id.clone(), &role);
            let agent_b = harness
                .agents
                .get(&cid_b)
                .and_then(|agent| agent.agent_id.as_deref())
                .map(crate::parse_agent_id)
                .expect("agent B");
            harness.handle_register_transport_capability(
                "transport-cold",
                RegisterTransportCapabilityRequest {
                    request_id: "register-cold".to_owned(),
                    transport_name: "slack".to_owned(),
                    send_tool: None,
                    send_destinations: Vec::new(),
                },
            );
            harness.handle_transport_message_ingress(
                "transport-cold",
                TransportMessageIngressRequest {
                    request_id: "cold-first".to_owned(),
                    target_agent_id: agent_a.clone(),
                    draft: draft("cold-event"),
                },
            );
            assert!(harness.unload_agent_from_session_if_loaded(&session_id, agent_a.as_str()));
            (agent_a, agent_b)
        };
        std::fs::remove_file(temp.path().join("agents/.transport-ingress-locator-v2.log"))
            .expect("remove locator log");
        std::fs::remove_file(
            temp.path()
                .join("agents/.transport-ingress-locator-v2.head.cbor"),
        )
        .expect("remove locator head");

        let mut harness = super::super::tests::echo_harness(temp.path()).expect("reopened harness");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "transport-cold");
        harness.handle_register_transport_capability(
            "transport-cold",
            RegisterTransportCapabilityRequest {
                request_id: "register-cold-2".to_owned(),
                transport_name: "slack".to_owned(),
                send_tool: None,
                send_destinations: Vec::new(),
            },
        );
        harness.handle_transport_message_ingress(
            "transport-cold",
            TransportMessageIngressRequest {
                request_id: "cold-move".to_owned(),
                target_agent_id: agent_b,
                draft: draft("cold-event"),
            },
        );
        let move_result = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "cold-move" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("move result");
        assert_eq!(
            move_result.disposition,
            TransportMessageIngressDisposition::Rejected {
                reason: TransportIngressRejection::DedupConflict,
            }
        );

        harness.handle_transport_message_ingress(
            "transport-cold",
            TransportMessageIngressRequest {
                request_id: "cold-original".to_owned(),
                target_agent_id: agent_a.clone(),
                draft: draft("cold-event"),
            },
        );
        let original = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "cold-original" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("original result");
        match original.disposition {
            TransportMessageIngressDisposition::Committed {
                canonical,
                reply_activation:
                    TransportReplyActivation::Inactive(
                        TransportReplyInactiveReason::NoReplyPath
                        | TransportReplyInactiveReason::InactiveTarget,
                    ),
                ..
            } => assert_eq!(canonical.target_agent_id, agent_a),
            disposition => panic!("unexpected original result: {disposition:?}"),
        }
    }

    /// Reply activation belongs only to the exact current live capability; a
    /// later duplicate after revocation still returns canonical history but
    /// must not retain or recreate reply authority.
    #[test]
    fn duplicate_after_reply_capability_revocation_is_typed_inactive() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "transport-active");
        let cid = super::super::tests::ensure_test_user_agent(&mut harness);
        let agent_id = harness
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("agent");
        let send_tool = ToolName::new("slack_send");
        harness.transport_capabilities.insert(
            "transport-active".to_owned(),
            vec![TransportCapability {
                transport_name: "slack".to_owned(),
                send_tool: Some(send_tool.clone()),
                session_generation: harness.current_session_generation,
                registration_epoch: 1,
                send_destinations: Vec::new(),
            }],
        );
        let mut active_draft = draft("active-event");
        active_draft.send_tool = Some(send_tool.clone());
        let request = TransportMessageIngressRequest {
            request_id: "active-first".to_owned(),
            target_agent_id: agent_id,
            draft: active_draft,
        };
        harness.handle_transport_message_ingress("transport-active", request.clone());
        let first = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "active-first" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("first result");
        let message_id = match first.disposition {
            TransportMessageIngressDisposition::Committed {
                message_id,
                reply_activation: TransportReplyActivation::Active,
                ..
            } => message_id,
            disposition => panic!("unexpected active result: {disposition:?}"),
        };
        assert!(harness.transport_reply_routes.contains_key(&message_id));

        harness.revoke_transport_send_tool("transport-active", &send_tool);
        let mut retry = request;
        retry.request_id = "active-revoked".to_owned();
        harness.handle_transport_message_ingress("transport-active", retry);
        let revoked = frames
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "active-revoked" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("revoked result");
        assert!(matches!(
            revoked.disposition,
            TransportMessageIngressDisposition::Committed {
                outcome: TransportMessageIngressOutcome::Duplicate,
                reply_activation: TransportReplyActivation::Inactive(
                    TransportReplyInactiveReason::InactiveCapability
                ),
                ..
            }
        ));
        assert!(!harness.transport_reply_routes.contains_key(&message_id));
    }

    /// A stale first waiter must never capture the route from a live
    /// current-generation waiter queued behind it.
    #[test]
    fn stale_first_waiter_does_not_activate_before_current_waiter() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        harness
            .initialized_sessions
            .insert(harness.current_session_id.clone());
        let frames = super::super::tests::connect_test_tool(&mut harness, "waiter-test");
        let cid = super::super::tests::ensure_test_user_agent(&mut harness);
        let agent_id = harness
            .agents
            .get(&cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .map(crate::parse_agent_id)
            .expect("agent");
        harness.current_session_generation = 1;
        let send_tool = ToolName::new("slack_send");
        harness.transport_capabilities.insert(
            "waiter-test".to_owned(),
            vec![TransportCapability {
                transport_name: "slack".to_owned(),
                send_tool: Some(send_tool.clone()),
                session_generation: 1,
                registration_epoch: 7,
                send_destinations: Vec::new(),
            }],
        );
        let extension_name = ExtensionName::from("waiter-test");
        let mut canonical_draft = draft("waiter-event");
        canonical_draft.send_tool = Some(send_tool.clone());
        let message_id = MessageId::new("msg-waiter");
        let message = AgentMessageIncoming {
            recipient_id: agent_id.clone(),
            envelope: MessageEnvelope {
                message_id: message_id.clone(),
                transport: MessageTransportRef {
                    name: "slack".to_owned(),
                    instance: Some(extension_name.clone()),
                },
                source: canonical_draft.external_endpoint.clone(),
                destination: MessageEndpoint::Agent {
                    session_id: Some(harness.current_session_id.clone()),
                    agent_id: agent_id.clone(),
                    display_name: None,
                },
                conversation: None,
                operation: canonical_draft.operation.clone(),
                transport_identity_mentioned: canonical_draft.transport_identity_mentioned,
                trust: MessageTrust {
                    content: MessageContentTrust::UntrustedExternal,
                    identity: canonical_draft.identity_assurance,
                    policy: canonical_draft.policy_status,
                },
                external_identity: canonical_draft.external_identity.clone(),
                ordering: None,
                occurred_at: None,
                reply_path: Some(MessageReplyPath {
                    tool_name: send_tool,
                    selector: ReplySelector::ReplyToMessage,
                    lifetime: ReplyPathLifetime::ActiveSession,
                }),
            },
        };
        let key = ingress_dedup_key(&extension_name, &canonical_draft).expect("dedup key");
        let pending_record = TransportDedupRecord {
            draft: canonical_draft,
            target_agent_id: agent_id,
            message_id: message_id.clone(),
            committed: false,
            session_id: harness.current_session_id.clone(),
        };
        let mut locator_record = pending_record.clone();
        locator_record.committed = true;
        assert!(matches!(
            harness
                .transport_ingress_locator
                .reserve(&harness.agent_store, &key, &locator_record),
            Ok(LocatorReservation::Reserved)
        ));
        assert!(harness.insert_transport_dedup(key, pending_record));
        let revoked_waiter = PendingIngressAck {
            connection_id: "waiter-test".to_owned(),
            request_id: "revoked-epoch".to_owned(),
            session_generation: 1,
            capability_epoch: 6,
            outcome: TransportMessageIngressOutcome::Duplicate,
            activation_started_at: Instant::now(),
            request_seq: None,
        };
        assert_eq!(
            harness.waiter_inactive_reason(&revoked_waiter, &message, &extension_name),
            Some(TransportReplyInactiveReason::InactiveCapability)
        );
        let mut disconnected_waiter = revoked_waiter.clone();
        disconnected_waiter.connection_id = "missing-connection".to_owned();
        disconnected_waiter.capability_epoch = 7;
        assert_eq!(
            harness.waiter_inactive_reason(&disconnected_waiter, &message, &extension_name),
            Some(TransportReplyInactiveReason::InactiveConnection)
        );
        let mut old_session_message = message.clone();
        if let MessageEndpoint::Agent {
            session_id,
            agent_id: _,
            display_name: _,
        } = &mut old_session_message.envelope.destination
        {
            *session_id = Some("old-session".into());
        }
        let mut current_waiter = revoked_waiter.clone();
        current_waiter.capability_epoch = 7;
        assert_eq!(
            harness.waiter_inactive_reason(&current_waiter, &old_session_message, &extension_name),
            Some(TransportReplyInactiveReason::NonCurrentSession)
        );
        let route = harness
            .agent_routes
            .remove(message.recipient_id.as_str())
            .expect("live route");
        assert_eq!(
            harness.waiter_inactive_reason(&current_waiter, &message, &extension_name),
            Some(TransportReplyInactiveReason::InactiveTarget)
        );
        harness
            .agent_routes
            .insert(message.recipient_id.to_string(), route);
        harness.pending_ingress_acks.insert(
            message_id.clone(),
            vec![
                PendingIngressAck {
                    connection_id: "waiter-test".to_owned(),
                    request_id: "stale-waiter".to_owned(),
                    session_generation: 0,
                    capability_epoch: 7,
                    outcome: TransportMessageIngressOutcome::Accepted,
                    activation_started_at: Instant::now(),
                    request_seq: None,
                },
                PendingIngressAck {
                    connection_id: "waiter-test".to_owned(),
                    request_id: "current-waiter".to_owned(),
                    session_generation: 1,
                    capability_epoch: 7,
                    outcome: TransportMessageIngressOutcome::Duplicate,
                    activation_started_at: Instant::now(),
                    request_seq: None,
                },
                PendingIngressAck {
                    connection_id: "waiter-test".to_owned(),
                    request_id: "superseded-waiter".to_owned(),
                    session_generation: 1,
                    capability_epoch: 7,
                    outcome: TransportMessageIngressOutcome::Duplicate,
                    activation_started_at: Instant::now(),
                    request_seq: None,
                },
            ],
        );
        assert!(harness.complete_ingress_commit(&message));
        let results = frames
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match &frame.frame {
                HarnessOutputMessage::TransportMessageIngressResult(result)
                    if result.request_id == "stale-waiter"
                        || result.request_id == "current-waiter"
                        || result.request_id == "superseded-waiter" =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(results.iter().any(|result| {
            result.request_id == "stale-waiter"
                && matches!(
                    result.disposition,
                    TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: _,
                        canonical: _,
                        reply_activation: TransportReplyActivation::Inactive(
                            TransportReplyInactiveReason::NonCurrentGeneration
                        ),
                    }
                )
        }));
        assert!(results.iter().any(|result| {
            result.request_id == "superseded-waiter"
                && matches!(
                    result.disposition,
                    TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: _,
                        canonical: _,
                        reply_activation: TransportReplyActivation::Inactive(
                            TransportReplyInactiveReason::SupersededWaiter
                        ),
                    }
                )
        }));
        assert!(results.iter().any(|result| {
            result.request_id == "current-waiter"
                && matches!(
                    result.disposition,
                    TransportMessageIngressDisposition::Committed {
                        message_id: _,
                        outcome: _,
                        canonical: _,
                        reply_activation: TransportReplyActivation::Active,
                    }
                )
        }));
        assert_eq!(
            harness
                .transport_reply_routes
                .get(&message_id)
                .map(|route| route.session_generation),
            Some(1)
        );
    }

    /// Capability metadata uses exact nonempty byte bounds and rejects
    /// controls, preventing oversized or log-confusing aliases, endpoints,
    /// and threads.
    #[test]
    fn capability_field_validation_boundaries() {
        assert!(valid_capability_field("x"));
        assert!(valid_capability_field(
            &"x".repeat(MAX_CAPABILITY_FIELD_BYTES)
        ));
        assert!(!valid_capability_field(""));
        assert!(!valid_capability_field(
            &"x".repeat(MAX_CAPABILITY_FIELD_BYTES + 1)
        ));
        assert!(!valid_capability_field("route\nspoof"));
    }

    /// Capability registration is an adversarial RPC boundary, so count,
    /// aggregate-size, and duplicate-native-route limits must reject through
    /// the same authenticated handler used by extensions without installing
    /// state.
    #[test]
    fn capability_registration_rpc_rejects_bounded_and_duplicate_routes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        let frames = super::super::tests::connect_test_tool(&mut harness, "capability-rpc");
        let tool = ToolName::new("slack_send");
        harness.registry.register(
            "capability-rpc",
            tau_proto::ToolSpec {
                name: tool.clone(),
                model_visible_name: None,
                description: None,
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
        let destination = |alias: String, conversation_id: String, display: String| {
            tau_proto::TransportSendDestinationCapability {
                alias,
                external_endpoint: MessageEndpoint::External {
                    stable_id: None,
                    display_name: Some(display.clone()),
                    identity_alias: None,
                    actor_kind: ExternalActorKind::Unknown,
                },
                conversation: tau_proto::MessageConversation {
                    kind: tau_proto::ConversationKind::Channel,
                    stable_id: Some(conversation_id),
                    display_name: Some(display),
                    thread: None,
                    reply_to: None,
                },
            }
        };
        let cases = [
            (
                "too-many",
                (0..65)
                    .map(|index| {
                        destination(
                            format!("route-{index}"),
                            format!("C{index}"),
                            format!("route-{index}"),
                        )
                    })
                    .collect(),
                "invalid_send_destinations",
            ),
            (
                "duplicate-native",
                vec![
                    destination("first".to_owned(), "C1".to_owned(), "first".to_owned()),
                    destination("second".to_owned(), "C1".to_owned(), "second".to_owned()),
                ],
                "invalid_send_destination_route",
            ),
            (
                "aggregate",
                (0..64)
                    .map(|index| {
                        destination(
                            format!("route-{index}"),
                            format!("C{index}"),
                            "x".repeat(MAX_CAPABILITY_FIELD_BYTES),
                        )
                    })
                    .collect(),
                "send_destinations_too_large",
            ),
        ];
        for (request_id, send_destinations, expected) in cases {
            harness.handle_register_transport_capability(
                "capability-rpc",
                RegisterTransportCapabilityRequest {
                    request_id: request_id.to_owned(),
                    transport_name: "slack".to_owned(),
                    send_tool: Some(tool.clone()),
                    send_destinations,
                },
            );
            let result = frames
                .lock()
                .expect("frames")
                .iter()
                .find_map(|frame| match &frame.frame {
                    HarnessOutputMessage::RegisterTransportCapabilityResult(result)
                        if result.request_id == request_id =>
                    {
                        Some(result.clone())
                    }
                    _ => None,
                })
                .expect("registration result");
            assert_eq!(result.error.as_deref(), Some(expected), "{request_id}");
            assert!(!result.accepted);
        }
        assert!(
            harness
                .transport_capabilities
                .get("capability-rpc")
                .is_none_or(Vec::is_empty)
        );
    }

    /// A peer cannot bypass per-registration metadata bounds by accumulating
    /// arbitrarily many distinct transport names.
    #[test]
    fn capability_registration_rpc_bounds_distinct_transports_per_source() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        let frames = super::super::tests::connect_test_tool(&mut harness, "capability-count");
        let tool = ToolName::new("bounded_send");
        harness.registry.register(
            "capability-count",
            tau_proto::ToolSpec {
                name: tool.clone(),
                model_visible_name: None,
                description: None,
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
        for index in 0..MAX_CAPABILITIES_PER_SOURCE {
            harness.handle_register_transport_capability(
                "capability-count",
                RegisterTransportCapabilityRequest {
                    request_id: format!("register-{index}"),
                    transport_name: format!("transport-{index}"),
                    send_tool: None,
                    send_destinations: Vec::new(),
                },
            );
        }
        harness.handle_register_transport_capability(
            "capability-count",
            RegisterTransportCapabilityRequest {
                request_id: "replace-at-limit".to_owned(),
                transport_name: "transport-0".to_owned(),
                send_tool: Some(tool.clone()),
                send_destinations: Vec::new(),
            },
        );
        let snapshot = |capabilities: &[TransportCapability]| {
            capabilities
                .iter()
                .map(|capability| {
                    (
                        capability.transport_name.clone(),
                        capability.send_tool.clone(),
                        capability.session_generation,
                        capability.send_destinations.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let before_rejection = snapshot(&harness.transport_capabilities["capability-count"]);
        harness.handle_register_transport_capability(
            "capability-count",
            RegisterTransportCapabilityRequest {
                request_id: "register-over-limit".to_owned(),
                transport_name: "transport-over-limit".to_owned(),
                send_tool: None,
                send_destinations: Vec::new(),
            },
        );
        let results = frames
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match &frame.frame {
                HarnessOutputMessage::RegisterTransportCapabilityResult(result) => {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            results[..=MAX_CAPABILITIES_PER_SOURCE]
                .iter()
                .all(|result| result.accepted)
        );
        assert_eq!(
            results[MAX_CAPABILITIES_PER_SOURCE + 1].error.as_deref(),
            Some("transport_capability_limit_reached")
        );
        assert_eq!(
            harness.transport_capabilities["capability-count"]
                .iter()
                .find(|capability| capability.transport_name == "transport-0")
                .and_then(|capability| capability.send_tool.as_ref()),
            Some(&tool)
        );
        assert_eq!(
            snapshot(&harness.transport_capabilities["capability-count"]),
            before_rejection
        );
        assert_eq!(
            harness.transport_capabilities["capability-count"].len(),
            MAX_CAPABILITIES_PER_SOURCE
        );
    }

    /// Tool revocation removes only capabilities and reply routes owned by the
    /// exact source/tool pair, preserving unrelated transport registrations.
    #[test]
    fn transport_send_tool_revocation_cleans_exact_routes() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut harness =
            super::super::tests::echo_harness(temp.path()).expect("test harness starts");
        let tool = ToolName::new("slack_send");
        for source in ["source-a", "source-b"] {
            harness.transport_capabilities.insert(
                source.to_owned(),
                vec![TransportCapability {
                    transport_name: "slack".to_owned(),
                    send_tool: Some(tool.clone()),
                    session_generation: harness.current_session_generation,
                    registration_epoch: 1,
                    send_destinations: Vec::new(),
                }],
            );
            harness.transport_reply_routes.insert(
                MessageId::new(format!("msg-{source}")),
                TransportReplyRoute {
                    connection_id: source.to_owned(),
                    agent_id: tau_proto::AgentId::parse("agent-a").expect("agent"),
                    session_generation: harness.current_session_generation,
                    send_tool: Some(tool.clone()),
                    transport_name: "slack".to_owned(),
                    external_endpoint: MessageEndpoint::External {
                        stable_id: Some("U1".to_owned()),
                        display_name: None,
                        identity_alias: None,
                        actor_kind: ExternalActorKind::Human,
                    },
                    conversation: None,
                },
            );
        }
        harness.revoke_transport_send_tool("source-a", &tool);
        assert!(harness.transport_capabilities["source-a"].is_empty());
        assert!(
            !harness
                .transport_reply_routes
                .contains_key(&MessageId::new("msg-source-a"))
        );
        assert_eq!(harness.transport_capabilities["source-b"].len(), 1);
        assert!(
            harness
                .transport_reply_routes
                .contains_key(&MessageId::new("msg-source-b"))
        );
    }
}
