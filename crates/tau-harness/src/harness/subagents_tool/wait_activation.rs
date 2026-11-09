//! Runtime-only provenance for a wait interrupted by accepted input.

use tau_proto::{ActivationKind, CborValue, ObservationId, ToolCallId};

/// Exact accepted activation, retained even while a wait is claimed.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WaitActivation {
    /// Existing observation identity; never a replacement tool identity.
    pub(super) observation: ObservationId,
    /// Closed source class and its actual tool identity, where applicable.
    pub(super) source: WaitActivationSource,
}

/// Accepted source provenance; only tool variants can carry tool identities.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum WaitActivationSource {
    /// Incremental source from the identified tool.
    PartialToolOutput(ToolCallId),
    /// Terminal notice; old notices may lack their source identity.
    BackgroundCompletion(Option<ToolCallId>),
    /// Authenticated UI input.
    User,
    /// Ordinary inter-agent message.
    AgentMessage,
    /// Canonical external message fact.
    ExternalMessage,
    /// Harness-authored prompt.
    InternalPrompt,
    /// Configured timer input.
    Timer,
    /// Isolated watched-agent notification.
    WatchNotification,
    /// Loop-guard intervention.
    LoopGuard,
    /// Source classification unavailable in retained queue metadata.
    Other,
}

impl WaitActivationSource {
    /// Preserve the queue's existing source classification and correlation.
    pub(super) fn from_prompt(prompt: &crate::agent::PendingPrompt) -> Self {
        match prompt.activation_kind() {
            ActivationKind::VisibleUser => Self::User,
            ActivationKind::AgentMessage => Self::AgentMessage,
            ActivationKind::ExternalMessage => Self::ExternalMessage,
            ActivationKind::InternalPrompt => Self::InternalPrompt,
            ActivationKind::Timer => Self::Timer,
            ActivationKind::WatchNotification => Self::WatchNotification,
            ActivationKind::LoopGuard => Self::LoopGuard,
            ActivationKind::BackgroundCompletion => Self::BackgroundCompletion(
                prompt
                    .background_completion
                    .as_ref()
                    .map(|source| source.call_id.clone()),
            ),
            ActivationKind::Other => Self::Other,
        }
    }
}

impl WaitActivation {
    /// Render only control metadata, without reproducing activating payload.
    pub(super) fn receipt(&self) -> CborValue {
        let reason = match &self.source {
            WaitActivationSource::PartialToolOutput(_) => "partial_tool_output",
            WaitActivationSource::BackgroundCompletion(_) => "completion",
            WaitActivationSource::User => "user",
            WaitActivationSource::AgentMessage => "agent_message",
            WaitActivationSource::ExternalMessage => "external_message",
            WaitActivationSource::InternalPrompt => "internal_prompt",
            WaitActivationSource::Timer => "timer",
            WaitActivationSource::WatchNotification => "watch_notification",
            WaitActivationSource::LoopGuard => "loop_guard",
            WaitActivationSource::Other => "other",
        };
        let cause = match &self.source {
            WaitActivationSource::PartialToolOutput(call_id)
            | WaitActivationSource::BackgroundCompletion(Some(call_id)) => call_id.to_string(),
            WaitActivationSource::BackgroundCompletion(None) => "unavailable".to_owned(),
            _ => self.observation.to_string(),
        };
        let mut receipt = format!("interrupted: {reason}\ncause: ");
        tau_proto::write_provider_tool_header_text(&cause, &mut receipt)
            .expect("writing a header to String cannot fail");
        CborValue::Text(receipt)
    }
}

#[cfg(test)]
impl From<ObservationId> for WaitActivation {
    fn from(observation: ObservationId) -> Self {
        Self {
            observation,
            source: WaitActivationSource::Other,
        }
    }
}
