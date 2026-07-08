/// Typed event payloads that can be selected from a [`tau_proto::Event`].
///
/// This crate currently provides implementations only for the first-party
/// payloads listed in this module. Downstream crates cannot implement this
/// trait for additional `tau_proto` payloads because both the trait and payload
/// type would be foreign to them; use [`crate::ExtensionBuilder::on_raw`] or
/// [`crate::ExtensionBuilder::on_raw_live`] for unsupported first-party
/// variants or extension-owned custom events.
pub trait EventPayload {
    /// Canonical event name for this payload type.
    const NAME: tau_proto::EventName;

    /// Returns the typed payload when `event` carries this payload type.
    fn from_event(event: &tau_proto::Event) -> Option<&Self>;
}

macro_rules! impl_event_payload {
    ($ty:ty, $name:expr, $variant:path) => {
        impl EventPayload for $ty {
            const NAME: tau_proto::EventName = $name;

            fn from_event(event: &tau_proto::Event) -> Option<&Self> {
                match event {
                    $variant(payload) => Some(payload),
                    _ => None,
                }
            }
        }
    };
}

impl_event_payload!(
    tau_proto::ToolStarted,
    tau_proto::EventName::TOOL_STARTED,
    tau_proto::Event::ToolStarted
);
impl_event_payload!(
    tau_proto::ToolCancelRequest,
    tau_proto::EventName::TOOL_CANCEL_REQUEST,
    tau_proto::Event::ToolCancelRequest
);
impl_event_payload!(
    tau_proto::AgentStarted,
    tau_proto::EventName::AGENT_STARTED,
    tau_proto::Event::AgentStarted
);
impl_event_payload!(
    tau_proto::AgentMetadataSet,
    tau_proto::EventName::AGENT_METADATA_SET,
    tau_proto::Event::AgentMetadataSet
);
impl_event_payload!(
    tau_proto::AgentMetadataUnset,
    tau_proto::EventName::AGENT_METADATA_UNSET,
    tau_proto::Event::AgentMetadataUnset
);
impl_event_payload!(
    tau_proto::AgentReplayComplete,
    tau_proto::EventName::AGENT_REPLAY_COMPLETE,
    tau_proto::Event::AgentReplayComplete
);
impl_event_payload!(
    tau_proto::AgentPromptSubmitted,
    tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
    tau_proto::Event::AgentPromptSubmitted
);
impl_event_payload!(
    tau_proto::HarnessNotice,
    tau_proto::EventName::HARNESS_NOTICE,
    tau_proto::Event::HarnessNotice
);
impl_event_payload!(
    tau_proto::SessionStarted,
    tau_proto::EventName::SESSION_STARTED,
    tau_proto::Event::SessionStarted
);
impl_event_payload!(
    tau_proto::SessionAgentLoaded,
    tau_proto::EventName::SESSION_AGENT_LOADED,
    tau_proto::Event::SessionAgentLoaded
);
impl_event_payload!(
    tau_proto::SessionAgentUnloaded,
    tau_proto::EventName::SESSION_AGENT_UNLOADED,
    tau_proto::Event::SessionAgentUnloaded
);
impl_event_payload!(
    tau_proto::SessionShutdown,
    tau_proto::EventName::SESSION_SHUTDOWN,
    tau_proto::Event::SessionShutdown
);
impl_event_payload!(
    tau_proto::SessionReplayComplete,
    tau_proto::EventName::SESSION_REPLAY_COMPLETE,
    tau_proto::Event::SessionReplayComplete
);
impl_event_payload!(
    tau_proto::StartAgentAccepted,
    tau_proto::EventName::AGENT_START_ACCEPTED,
    tau_proto::Event::StartAgentAccepted
);
impl_event_payload!(
    tau_proto::StartAgentResult,
    tau_proto::EventName::AGENT_START_RESULT,
    tau_proto::Event::StartAgentResult
);
impl_event_payload!(
    tau_proto::UiShellCommand,
    tau_proto::EventName::UI_SHELL_COMMAND,
    tau_proto::Event::UiShellCommand
);
