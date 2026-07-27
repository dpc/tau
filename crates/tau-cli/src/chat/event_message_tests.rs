use super::*;

/// The shared interactive-UI event wrapper keeps draft and focus observations
/// durable-by-default on the wire despite their transient event defaults.
#[test]
fn durable_emit_message_uses_false_metadata_for_liveness_events() {
    for event in [
        Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            text: "typing".to_owned(),
        }),
        Event::UiFocusChanged(UiFocusChanged {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            focused: true,
        }),
    ] {
        assert!(!event.defaults_to_persist());
        let HarnessInputMessage::Emit(emit) = durable_emit_message(&event) else {
            panic!("UI event must use Emit");
        };
        assert!(emit.persist);
        assert_eq!(emit.event.as_ref(), &event);
    }
}
