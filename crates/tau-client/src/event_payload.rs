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
    tau_proto::SessionShutdown,
    tau_proto::EventName::SESSION_SHUTDOWN,
    tau_proto::Event::SessionShutdown
);
