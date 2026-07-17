//! Rendering tests for committed extension-published message facts.

use tau_proto::{
    CborValue, MessageDeleted, MessageDelivered, MessageEdited, MessageExtensionData,
    MessageReactionAdded, MessageReactionRemoved, MessageSent,
};

use super::*;

/// Build the six representative valid facts used to compare distinct UI
/// operations and live/replay-stable output.
fn representative_facts() -> [Event; 6] {
    let publisher = MessagePublisherId::new("bridge-main");
    let agent = MessageAgentTarget::new("agent-1");
    let party = MessageParty {
        stable_id: "user-1".to_owned(),
        display_name: Some("Ali\u{202e}ce".to_owned()),
    };
    let conversation = Some(MessageConversation {
        stable_id: "conversation-1".to_owned(),
        display_name: Some("General".to_owned()),
    });
    let reference = MessageFactRef {
        publisher_extension_id: MessagePublisherId::new("other-bridge"),
        message_id: MessageFactId::new("unknown-message"),
    };
    let mut delivered = MessageDelivered::new(
        publisher.clone(),
        agent.clone(),
        MessageFactId::new("delivered-1"),
        party.clone(),
        conversation.clone(),
        "hello\nworld",
    );
    delivered.extension_data =
        MessageExtensionData::new(CborValue::Text("opaque sentinel".to_owned()))
            .expect("bounded opaque fixture");
    [
        Event::MessageDelivered(delivered),
        Event::MessageEdited(MessageEdited::new(
            publisher.clone(),
            agent.clone(),
            reference.clone(),
            Some(party.clone()),
            conversation.clone(),
            "edited body",
        )),
        Event::MessageDeleted(MessageDeleted::new(
            publisher.clone(),
            agent.clone(),
            reference.clone(),
            Some(party.clone()),
            conversation.clone(),
        )),
        Event::MessageReactionAdded(MessageReactionAdded::new(
            publisher.clone(),
            agent.clone(),
            reference.clone(),
            Some(party.clone()),
            conversation.clone(),
            "👍",
        )),
        Event::MessageReactionRemoved(MessageReactionRemoved::new(
            publisher.clone(),
            agent.clone(),
            reference,
            Some(party.clone()),
            conversation.clone(),
            "👍",
        )),
        Event::MessageSent(MessageSent::new(
            publisher,
            agent,
            MessageFactId::new("sent-1"),
            Some(party),
            conversation,
            "sent body",
        )),
    ]
}

/// Apply the production chat delivery fold, which intentionally discards only
/// the live/replay marker before invoking the common event renderer.
fn render_delivery(delivery: tau_proto::EventDelivery) -> String {
    let (event, _replay, _recorded_at) = delivery.into_parts();
    render(&event, MessageFactTargetContext::Explicit).expect("message fact render")
}

/// All six events render distinct bounded headings, preserve stable IDs as
/// primary identifiers, and never disclose extension-private data.
#[test]
fn six_facts_render_distinct_safe_live_and_replay_output() {
    let expected_headings = [
        "External message delivered",
        "External message edited",
        "External message deleted",
        "External message reaction added",
        "External message reaction removed",
        "External message sent",
    ];
    let expected_specific_fields = [
        ["Message ID: delivered-1", "Sender: user-1", "Text:"],
        [
            "Referenced message ID: unknown-message",
            "Actor: user-1",
            "Text:",
        ],
        [
            "Referenced message ID: unknown-message",
            "Actor: user-1",
            "Conversation: conversation-1",
        ],
        [
            "Referenced message ID: unknown-message",
            "Actor: user-1",
            "Reaction: 👍",
        ],
        [
            "Referenced message ID: unknown-message",
            "Actor: user-1",
            "Reaction: 👍",
        ],
        ["Message ID: sent-1", "Recipient: user-1", "Text:"],
    ];
    for ((event, expected_heading), expected_fields) in representative_facts()
        .into_iter()
        .zip(expected_headings)
        .zip(expected_specific_fields)
    {
        let recorded_at = tau_proto::UnixMicros::new(1_700_000_000_000_000);
        let live = render_delivery(tau_proto::EventDelivery::live(recorded_at, event.clone()));
        let replay = render_delivery(tau_proto::EventDelivery::replay(recorded_at, event));
        assert_eq!(live, replay);
        assert!(live.starts_with(expected_heading));
        assert!(live.contains("Publisher: bridge-main"));
        assert!(live.contains("Tau target: agent-1"));
        assert!(live.contains("user-1 [display: Ali\\u{202E}ce]"));
        assert!(!live.contains("opaque sentinel"));
        for expected_field in expected_fields {
            assert!(
                live.contains(expected_field),
                "missing {expected_field}: {live}"
            );
        }
    }
}

/// Operation targets stay unresolved and display both stable namespace
/// components rather than guessing ownership or reply authority.
#[test]
fn unknown_reference_is_rendered_as_stable_identifiers() {
    let edited = representative_facts()
        .into_iter()
        .nth(1)
        .expect("edited fixture");
    let rendered = render(&edited, MessageFactTargetContext::Implied).expect("edited render");
    assert!(rendered.contains("Referenced publisher: other-bridge"));
    assert!(rendered.contains("Referenced message ID: unknown-message"));
    assert!(!rendered.contains("Tau target:"));
}

/// A universal-field failure produces only the stable event, publisher, and
/// categorical reason, identically for every delivery pass.
#[test]
fn invalid_fact_renders_deterministic_payload_free_diagnostic() {
    let mut delivered = representative_facts()
        .into_iter()
        .next()
        .expect("delivered fixture");
    let Event::MessageDelivered(fact) = &mut delivered else {
        unreachable!("fixture is delivered")
    };
    fact.agent_id = MessageAgentTarget::new("../invalid");
    fact.text = "secret body".to_owned();

    let expected = "Unprojectable message fact\nEvent: message.delivered\nPublisher: bridge-main\nReason: invalid_target";
    assert_eq!(
        render(&delivered, MessageFactTargetContext::Explicit).as_deref(),
        Some(expected)
    );
    assert_eq!(
        render(&delivered, MessageFactTargetContext::Implied).as_deref(),
        Some(expected)
    );
    assert!(!expected.contains("secret body"));
}

/// Valid target parsing is exposed separately so unavailable agents remain
/// normal globally visible facts rather than invalid diagnostics.
#[test]
fn target_parser_distinguishes_valid_and_invalid_claims() {
    let valid = representative_facts()
        .into_iter()
        .next()
        .expect("delivered fixture");
    assert_eq!(
        target_agent_id(&valid),
        Some(MessageFactTarget::Valid(
            AgentId::parse("agent-1").expect("valid agent id")
        ))
    );

    let mut invalid = valid;
    let Event::MessageDelivered(fact) = &mut invalid else {
        unreachable!("fixture is delivered")
    };
    fact.agent_id = MessageAgentTarget::new("");
    assert_eq!(target_agent_id(&invalid), Some(MessageFactTarget::Invalid));
    assert!(
        render(&invalid, MessageFactTargetContext::Explicit)
            .expect("invalid diagnostic")
            .contains("Reason: invalid_target")
    );
}

/// Literal display delimiters and generated control escapes cannot make
/// distinct untrusted facts produce identical terminal text.
#[test]
fn untrusted_identifiers_and_text_render_injectively() {
    let mut delimiter_text = representative_facts()
        .into_iter()
        .next()
        .expect("delivered fixture");
    let mut secondary_display = delimiter_text.clone();
    let Event::MessageDelivered(delimiter_fact) = &mut delimiter_text else {
        unreachable!("fixture is delivered")
    };
    delimiter_fact.sender.stable_id = "c1 [display: General]".to_owned();
    delimiter_fact.sender.display_name = None;
    let Event::MessageDelivered(display_fact) = &mut secondary_display else {
        unreachable!("fixture is delivered")
    };
    display_fact.sender.stable_id = "c1".to_owned();
    display_fact.sender.display_name = Some("General".to_owned());

    let delimiter_rendered = render(&delimiter_text, MessageFactTargetContext::Explicit)
        .expect("delimiter-bearing fact");
    let display_rendered = render(&secondary_display, MessageFactTargetContext::Explicit)
        .expect("secondary-display fact");
    assert_ne!(delimiter_rendered, display_rendered);
    assert!(delimiter_rendered.contains(r"Sender: c1 \[display: General\]"));
    assert!(display_rendered.contains("Sender: c1 [display: General]"));

    let mut literal_escape = delimiter_text.clone();
    let mut control_character = delimiter_text;
    let Event::MessageDelivered(literal_fact) = &mut literal_escape else {
        unreachable!("fixture is delivered")
    };
    literal_fact.text = r"\u{000A}".to_owned();
    let Event::MessageDelivered(control_fact) = &mut control_character else {
        unreachable!("fixture is delivered")
    };
    control_fact.text = "\n".to_owned();

    let literal_rendered =
        render(&literal_escape, MessageFactTargetContext::Explicit).expect("literal escape fact");
    let control_rendered = render(&control_character, MessageFactTargetContext::Explicit)
        .expect("control-character fact");
    assert_ne!(literal_rendered, control_rendered);
    assert!(literal_rendered.contains(r"\\u{000A}"));
    assert!(control_rendered.contains(r"\u{000A}"));
}
