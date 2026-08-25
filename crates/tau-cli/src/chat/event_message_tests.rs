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
            text: Some("typing".to_owned()),
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

/// One-shot prompt submission must transfer the prompt allocation into the Emit
/// frame rather than copying prompt-sized bytes before protocol encoding.
#[test]
fn owned_durable_emit_message_preserves_prompt_allocation_identity() {
    let text = "large prompt sentinel ".repeat(4_096);
    let text_pointer = text.as_ptr();
    let event = Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        text,
        agent_id: "agent"
            .parse::<tau_proto::AgentId>()
            .expect("known-safe AgentId must be valid"),
        literal: false,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });

    let HarnessInputMessage::Emit(emit) = durable_emit_message_owned(event) else {
        panic!("UI event must use Emit");
    };
    let Event::UiPromptSubmitted(submitted) = emit.event.as_ref() else {
        panic!("Emit must retain the submitted-prompt event");
    };

    assert_eq!(submitted.text.as_ptr(), text_pointer);
    assert!(emit.persist);
}

/// Moving a one-shot event must change only ownership: its encoded CBOR remains
/// byte-for-byte identical to the previous borrowed-event wrapper.
#[test]
fn owned_and_borrowed_durable_emit_messages_encode_identically() {
    let event = Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        text: "literal prompt".to_owned(),
        agent_id: "agent"
            .parse::<tau_proto::AgentId>()
            .expect("known-safe AgentId must be valid"),
        literal: true,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    let borrowed = durable_emit_message(&event);
    let owned = durable_emit_message_owned(event);

    assert_eq!(
        tau_proto::encode_harness_input_to_vec(&owned).expect("encode owned message"),
        tau_proto::encode_harness_input_to_vec(&borrowed).expect("encode borrowed message")
    );
}
