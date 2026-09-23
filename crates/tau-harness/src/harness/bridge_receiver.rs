//! Ordinary receiving endpoints for configured external bridges.
//!
//! This deliberately does not reuse peer pool fairness or own designation
//! state.

use tau_proto::{
    BridgeReceiverErrorKind as ErrorKind, BridgeReceiverMode, BridgeReceiverOutcome as Outcome,
    BridgeReceiverRequest, BridgeReceiverResult, BridgeReceiverUnavailable as Unavailable,
};

use super::start_coordinator::StartPhase;
use super::*;

/// Reserved creation metadata distinguishing bridges from cooperative peers.
pub(super) const BRIDGE_RECEIVER_AGENT_METADATA_KEY: &str = "tau.bridge_receiver_endpoint";

/// Durable creation purpose for a no-bootstrap ordinary receiving endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReceivingPurpose {
    /// Existing cooperative inter-session endpoint lifecycle.
    Peer,
    /// Configured external-message bridge endpoint lifecycle.
    Bridge,
}

impl ReceivingPurpose {
    /// Reserved metadata key persisted in the ordinary immutable creation fact.
    pub(super) fn metadata_key(self) -> &'static str {
        match self {
            Self::Peer => super::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY,
            Self::Bridge => BRIDGE_RECEIVER_AGENT_METADATA_KEY,
        }
    }
}

impl Harness {
    /// Resolve privately, without publishing any external message or
    /// designation.
    pub(super) fn handle_bridge_receiver_request(
        &mut self,
        source: &tau_proto::ConnectionId,
        request: BridgeReceiverRequest,
        admission: ExtensionFrameAdmission,
    ) {
        let outcome = self.resolve_bridge_receiver(source, &request, &admission);
        let _ = self.runtime_io.bus.send_to(
            source,
            None,
            HarnessOutputMessage::BridgeReceiverResult(BridgeReceiverResult {
                request_id: request.request_id,
                outcome,
            }),
        );
    }

    /// Validate current connection/session authority before any selection or
    /// start.
    pub(super) fn resolve_bridge_receiver(
        &mut self,
        source: &tau_proto::ConnectionId,
        request: &BridgeReceiverRequest,
        admission: &ExtensionFrameAdmission,
    ) -> Outcome {
        if !self.extensions.entries.get(source).is_some_and(|entry| {
            entry
                .peer_capabilities
                .contains(&tau_proto::PeerCapability::MessageBridge)
        }) {
            return receiver_error(
                ErrorKind::Unauthorized,
                "requester is not a configured message bridge",
            );
        }
        if request.request_id.is_empty()
            || request.request_id.len() > 128
            || request.session_id != self.session_runtime.current_session_id
        {
            return receiver_error(
                ErrorKind::InvalidRequest,
                "invalid correlation or current session",
            );
        }
        // Shutdown can release activation-deferred RPCs while settling a
        // declaration, before its terminal SessionShutdown fact is published.
        // Those frames retain their old admission and cannot provision
        // receivers.
        if !self.extension_frame_admission_is_current(admission)
            || self.session_runtime.shutdown_published
        {
            return unavailable(Unavailable::NoEligibleAgent);
        }
        // Operational requests already wait behind activation. Session
        // discovery can still be pending; do not mistake that for an
        // absent saved target.
        if !self.session_runtime.turn_state.is_idle()
            || !self.agent_runtime.agent_registry.roster_valid
        {
            return unavailable(Unavailable::NotReady);
        }
        if let Some(role) = request.role.as_deref()
            && !self.config.available_roles.contains_key(role)
        {
            return receiver_error(ErrorKind::InvalidRole, "receiver role is not available");
        }
        if let BridgeReceiverMode::Register { tool_call_id } = &request.mode {
            if self
                .tool_routing
                .tool_runtime
                .pending_tool_providers
                .get(tool_call_id)
                != Some(source)
            {
                return receiver_error(
                    ErrorKind::InvalidCaller,
                    "register must name this bridge's live routed tool call",
                );
            }
            let caller = self.tool_routing.tool_runtime.tool_agents.get(tool_call_id);
            return match caller.filter(|id| {
                self.bridge_receiver_candidate(id, request.role.as_deref())
                    .is_some()
            }) {
                Some(agent_id) => Outcome::Selected {
                    agent_id: agent_id.clone(),
                },
                None => receiver_error(
                    ErrorKind::InvalidCaller,
                    "register caller is not an eligible receiving agent",
                ),
            };
        }
        let preferred = match &request.mode {
            BridgeReceiverMode::Restore { agent_id } => agent_id.as_ref(),
            BridgeReceiverMode::Select { preferred_agent_id }
            | BridgeReceiverMode::Ensure { preferred_agent_id } => preferred_agent_id.as_ref(),
            BridgeReceiverMode::Register { .. } => unreachable!(),
        };
        if let Some(id) = preferred {
            if self
                .bridge_receiver_candidate(id, request.role.as_deref())
                .is_some()
            {
                return Outcome::Selected {
                    agent_id: id.clone(),
                };
            }
            if self.bridge_receiver_start_pending(Some(id), request.role.as_deref()) {
                return unavailable(Unavailable::NotReady);
            }
        }
        if matches!(request.mode, BridgeReceiverMode::Restore { .. }) {
            return unavailable(Unavailable::NoEligibleAgent);
        }
        let selected = self
            .agent_runtime
            .agent_registry
            .agents
            .keys()
            .filter_map(|id| {
                self.bridge_receiver_candidate(id, request.role.as_deref())
                    .map(|time| receiver_order(time, id.clone()))
            })
            .min();
        if let Some((_, _, agent_id)) = selected {
            return Outcome::Selected { agent_id };
        }
        if self.bridge_receiver_start_pending(None, request.role.as_deref()) {
            return unavailable(Unavailable::NotReady);
        }
        if !matches!(request.mode, BridgeReceiverMode::Ensure { .. }) || request.role.is_none() {
            return unavailable(Unavailable::NoEligibleAgent);
        }
        self.create_bridge_receiver(source, request)
    }

    /// Inspect only loaded runtime and retained immutable creation facts.
    fn bridge_receiver_candidate(
        &self,
        id: &AgentId,
        role: Option<&str>,
    ) -> Option<Option<tau_proto::UnixMicros>> {
        let registry = &self.agent_runtime.agent_registry;
        let agent = registry.agents.get(id)?;
        if agent.dispatch.terminating
            || agent.identity.session_id != self.session_runtime.current_session_id
            || !registry.roster_loaded.contains(id)
            || registry.pending_operator_unloads.contains_key(id)
            || Self::agent_uses_non_tool_prompt_surface(agent)
        {
            return None;
        }
        let (creation, time) = self.session_runtime.agent_store.loaded_agent_creation(id)?;
        if role.is_some_and(|role| creation.role != role)
            || !self.config.available_roles.contains_key(&creation.role)
        {
            return None;
        }
        Some(time)
    }

    /// Coalesce starts before their membership/creation becomes selectable.
    fn bridge_receiver_start_pending(&self, exact: Option<&AgentId>, role: Option<&str>) -> bool {
        let registry = &self.agent_runtime.agent_registry;
        let matches = |id: &AgentId, candidate_role: Option<&str>| {
            exact.is_none_or(|exact| exact == id)
                && role.is_none_or(|role| candidate_role == Some(role))
        };
        registry.agents.iter().any(|(id, agent)| {
            !agent.dispatch.terminating
                && !registry.pending_operator_unloads.contains_key(id)
                && !Self::agent_uses_non_tool_prompt_surface(agent)
                && matches(id, agent.identity.role.as_deref())
                && !registry.roster_loaded.contains(id)
        }) || registry
            .start_coordinator
            .operations
            .values()
            .any(|operation| {
                operation.phase != StartPhase::ClosingFailure
                    && operation.pending.query.tool_call_id.is_some()
                    && matches(&operation.pending.cid, Some(&operation.pending.role))
            })
            || registry.pending_start_requests.iter().any(|pending| {
                pending.query.tool_call_id.is_some() && matches(&pending.cid, Some(&pending.role))
            })
    }

    /// Reuse ordinary role preparation and endpoint creation without inference.
    fn create_bridge_receiver(
        &mut self,
        source: &tau_proto::ConnectionId,
        request: &BridgeReceiverRequest,
    ) -> Outcome {
        let query = tau_proto::StartAgentRequest {
            // Request correlations can repeat after a bridge restart. They must
            // never rebind an old extension side query with a different role.
            query_id: format!("bridge-{}", tau_proto::ObservationId::random()),
            instruction: String::new(),
            role: request.role.clone(),
            trusted_internal_spans: Vec::new(),
            input_stats: Default::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: None,
        };
        let pending = match self.prepare_start_agent_request(source, query) {
            Ok(Some(pending)) => pending,
            Ok(None) => return unavailable(Unavailable::NotReady),
            Err(error) => return receiver_error(ErrorKind::CreationFailed, error),
        };
        let id = pending.cid.clone();
        if let Err(error) = self.start_agent_request_inner(
            pending,
            false,
            Some(ReceivingPurpose::Bridge),
            false,
            None,
        ) {
            if self.agent_runtime.agent_registry.agents.contains_key(&id) {
                self.remove_agent_expected(&id);
            }
            return receiver_error(ErrorKind::CreationFailed, error.to_string());
        }
        if self
            .bridge_receiver_candidate(&id, request.role.as_deref())
            .is_some()
        {
            Outcome::Selected { agent_id: id }
        } else {
            unavailable(Unavailable::NotReady)
        }
    }
}

/// Construct a classified absence without coupling it to a saved designation.
fn unavailable(reason: Unavailable) -> Outcome {
    Outcome::Unavailable { reason }
}

/// Construct a visible bounded lifecycle failure.
fn receiver_error(kind: ErrorKind, message: impl Into<String>) -> Outcome {
    Outcome::Error {
        kind,
        message: message.into(),
    }
}

/// Known timestamps sort before legacy unknown ones, then by stable identity.
fn receiver_order(
    time: Option<tau_proto::UnixMicros>,
    id: AgentId,
) -> (bool, Option<tau_proto::UnixMicros>, AgentId) {
    (time.is_none(), time, id)
}

#[cfg(test)]
mod tests;
