//! Directed canonical acceptance; no journal barrier or independent tombstone.

use tau_proto::{
    MessageAcceptanceOutcome as Outcome, MessageAcceptanceRequest, MessageAcceptanceResult,
};

use super::*;

/// Private attempt authority captured before asynchronous interception.
#[derive(Clone)]
pub(crate) struct PendingMessageAcceptance {
    /// Exact requesting connection, never a publisher-wide broadcast address.
    source: tau_proto::ConnectionId,
    /// Fresh caller-owned correlation for this attempt.
    request_id: String,
    /// Configured instance identity; reconnects cannot inherit pending
    /// authority.
    instance_id: tau_proto::ExtensionInstanceId,
    /// Exact session and generation captured on frame admission.
    admission: ExtensionFrameAdmission,
}

impl Harness {
    /// Authenticate and project before entering ordinary canonical publication.
    pub(super) fn handle_message_acceptance_request(
        &mut self,
        source: &tau_proto::ConnectionId,
        request: MessageAcceptanceRequest,
        admission: ExtensionFrameAdmission,
    ) {
        let refusal = |reason: &str| Outcome::Rejected {
            reason: reason.to_owned(),
        };
        let Some(entry) = self.extensions.entries.get(source) else {
            self.send_message_acceptance_result(
                source,
                request.request_id,
                refusal("not a configured message bridge"),
            );
            return;
        };
        if !entry
            .peer_capabilities
            .contains(&tau_proto::PeerCapability::MessageBridge)
            || request.request_id.is_empty()
            || request.request_id.len() > 128
            || request.session_id != self.session_runtime.current_session_id
        {
            self.send_message_acceptance_result(
                source,
                request.request_id,
                refusal("unauthorized or invalid acceptance request"),
            );
            return;
        }
        let pending = PendingMessageAcceptance {
            source: source.clone(),
            request_id: request.request_id,
            instance_id: entry.instance_id,
            admission,
        };
        let mut message =
            request
                .report
                .with_publisher(tau_proto::MessagePublisherId::from_extension_name(
                    &entry.name,
                ));
        message.idempotent_acceptance = true;
        let event = Event::MessageDelivered(message);
        if !matches!(tau_proto::project_message_fact(&event), Some(Ok(_))) {
            self.finish_message_acceptance(
                &pending,
                refusal("message cannot be projected as recipient input"),
            );
            return;
        }
        self.enqueue_message_acceptance(event, pending);
    }

    /// Recheck authority and retained identity at the serialized final commit.
    pub(super) fn admit_message_acceptance(
        &mut self,
        pending: &PendingMessageAcceptance,
        event: &Event,
    ) -> bool {
        let reason = if !self.extension_frame_admission_is_current(&pending.admission)
            || self.session_runtime.shutdown_published
            || !self
                .extensions
                .entries
                .get(&pending.source)
                .is_some_and(|entry| {
                    entry.instance_id == pending.instance_id
                        && entry.state != ExtensionState::Disconnected
                        && entry
                            .peer_capabilities
                            .contains(&tau_proto::PeerCapability::MessageBridge)
                }) {
            Some("acceptance authority is no longer current")
        } else if let Event::MessageDelivered(message) = event {
            let target = message.agent_id.as_str().parse::<AgentId>().ok();
            if !target.as_ref().is_some_and(|target| {
                let registry = &self.agent_runtime.agent_registry;
                registry
                    .agent_routes
                    .get(target)
                    .and_then(|cid| registry.agents.get(cid))
                    .is_some_and(|agent| {
                        !agent.dispatch.terminating
                            && agent.identity.session_id == self.session_runtime.current_session_id
                            && agent.identity.agent_id.as_deref() == Some(target.as_str())
                    })
                    && registry.roster_loaded.contains(target)
                    && !registry.pending_operator_unloads.contains_key(target)
                    && self.session_runtime.agent_store.agent(target).is_some()
            }) {
                Some("recipient is not loaded")
            } else if let Some((matches, _)) = target
                .as_ref()
                .and_then(|target| self.session_runtime.agent_store.agent(target))
                .and_then(|tree| tree.message_acceptance_matches(message))
            {
                let outcome = if matches {
                    Outcome::Accepted { duplicate: true }
                } else {
                    Outcome::Rejected {
                        reason: "message ID already accepted with different content".to_owned(),
                    }
                };
                self.finish_message_acceptance(pending, outcome);
                return false;
            } else {
                None
            }
        } else {
            Some("acceptance lost its canonical message")
        };
        if let Some(reason) = reason {
            self.finish_message_acceptance(
                pending,
                Outcome::Rejected {
                    reason: reason.to_owned(),
                },
            );
            false
        } else {
            true
        }
    }

    /// Confirm canonical ownership after normal fold, broadcast and wake
    /// handling.
    pub(super) fn complete_message_acceptance(
        &mut self,
        pending: &PendingMessageAcceptance,
        event: &Event,
    ) {
        let accepted = if let Event::MessageDelivered(message) = event {
            message
                .agent_id
                .as_str()
                .parse::<AgentId>()
                .ok()
                .and_then(|id| self.session_runtime.agent_store.agent(&id))
                .and_then(|tree| tree.message_acceptance_matches(message))
                .is_some_and(|(matches, _)| matches)
        } else {
            false
        };
        self.finish_message_acceptance(
            pending,
            if accepted {
                Outcome::Accepted { duplicate: false }
            } else {
                Outcome::Unknown {
                    reason: "canonical acceptance could not be established".to_owned(),
                }
            },
        );
    }

    /// Settle an attempt without consulting any filesystem completion state.
    pub(super) fn finish_message_acceptance(
        &mut self,
        pending: &PendingMessageAcceptance,
        outcome: Outcome,
    ) {
        self.send_message_acceptance_result(&pending.source, pending.request_id.clone(), outcome);
    }

    /// Deliver only to the original connection; a lost response remains
    /// unknown.
    fn send_message_acceptance_result(
        &mut self,
        source: &tau_proto::ConnectionId,
        request_id: String,
        outcome: Outcome,
    ) {
        let _ = self.runtime_io.bus.send_to(
            source,
            None,
            HarnessOutputMessage::MessageAcceptanceResult(MessageAcceptanceResult {
                request_id,
                outcome,
            }),
        );
    }
}
