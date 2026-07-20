//! Rendering tests for committed canonical external-message facts.

use tau_proto::{
    CborValue, MessageDeleted, MessageDelivered, MessageEdited, MessageExtensionData,
    MessageFactId, MessageReactionAdded, MessageReactionRemoved, MessageSent,
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
        sender_auth: None,
    };
    let conversation = Some(MessageConversation {
        stable_id: "conversation-1".to_owned(),
        display_name: Some("General".to_owned()),
        alias: None,
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

/// All six events use compact directional headings and immediate content with
/// byte-for-byte live/replay parity, while hiding superseded stable metadata.
#[test]
fn six_facts_render_distinct_safe_live_and_replay_output() {
    let expected = [
        "External `bridge-main` message from \"Ali\\u{202E}ce\" in General for Tau target agent-1:\nhello\\u{000A}world",
        "External `bridge-main` message edited by \"Ali\\u{202E}ce\" in General for Tau target agent-1:\nedited body",
        "External `bridge-main` message deleted by \"Ali\\u{202E}ce\" in General for Tau target agent-1:",
        "External `bridge-main` reaction added by \"Ali\\u{202E}ce\" in General for Tau target agent-1:\n👍",
        "External `bridge-main` reaction removed by \"Ali\\u{202E}ce\" in General for Tau target agent-1:\n👍",
        "External `bridge-main` message sent to \"Ali\\u{202E}ce\" in General for Tau target agent-1:\nsent body",
    ];
    for (event, expected) in representative_facts().into_iter().zip(expected) {
        let recorded_at = tau_proto::UnixMicros::new(1_700_000_000_000_000);
        let live = render_delivery(tau_proto::EventDelivery::live(recorded_at, event.clone()));
        let replay = render_delivery(tau_proto::EventDelivery::replay(recorded_at, event));
        assert_eq!(live, replay);
        assert_eq!(live, expected);
        assert!(!live.contains("delivered-1"));
        assert!(!live.contains("sent-1"));
        assert!(!live.contains("unknown-message"));
        assert!(!live.contains("user-1"));
        assert!(!live.contains("conversation-1"));
        assert!(!live.contains("opaque sentinel"));
    }
}

/// The requested Slack-DM presentation remains the exact two-line primary
/// acceptance shape without field labels or opaque identifiers.
#[test]
fn delivered_message_matches_compact_primary_shape() {
    let delivered = Event::MessageDelivered(MessageDelivered::new(
        MessagePublisherId::new("fedi-slack"),
        MessageAgentTarget::new("agent-1"),
        MessageFactId::new("slack-message:opaque"),
        MessageParty {
            stable_id: "slack-sender:opaque".to_owned(),
            display_name: Some("Dawid (dpc)".to_owned()),
            sender_auth: None,
        },
        Some(MessageConversation {
            stable_id: "D123".to_owned(),
            display_name: Some("dpc-dm".to_owned()),
            alias: None,
        }),
        "Can you see this?",
    ));

    assert_eq!(
        render(&delivered, MessageFactTargetContext::Implied).as_deref(),
        Some("External `fedi-slack` message from \"Dawid (dpc)\" in dpc-dm:\nCan you see this?")
    );
}

/// Presentation labels suppress opaque stable identifiers, while absent or
/// blank metadata falls back to stable party/conversation/reference values.
#[test]
fn presentation_values_prefer_useful_labels_with_stable_fallbacks() {
    let mut facts = representative_facts();
    let Event::MessageDelivered(delivered) = &mut facts[0] else {
        unreachable!("first fixture is delivered")
    };
    delivered.sender.display_name = Some(" \t".to_owned());
    let conversation = delivered
        .conversation
        .as_mut()
        .expect("delivered conversation");
    conversation.display_name = None;
    conversation.alias = Some("#friendly-route".to_owned());
    let delivered_rendered =
        render(&facts[0], MessageFactTargetContext::Implied).expect("delivered render");
    assert_eq!(
        delivered_rendered,
        "External `bridge-main` message from \"user-1\" in #friendly-route:\nhello\\u{000A}world"
    );
    assert!(!delivered_rendered.contains("conversation-1"));

    let Event::MessageEdited(edited) = &mut facts[1] else {
        unreachable!("second fixture is edited")
    };
    edited.actor = None;
    edited.conversation = None;
    assert_eq!(
        render(&facts[1], MessageFactTargetContext::Implied).as_deref(),
        Some(
            "External `bridge-main` message edited for message `other-bridge`/unknown-message:\nedited body"
        )
    );
}

/// Every fact kind uses stable participant and conversation identifiers when
/// its corresponding optional presentation values are unavailable.
#[test]
fn all_fact_kinds_use_stable_presentation_fallbacks() {
    for mut event in representative_facts() {
        match &mut event {
            Event::MessageDelivered(fact) => fact.sender.display_name = None,
            Event::MessageEdited(fact) => {
                fact.actor.as_mut().expect("edited actor").display_name = None;
            }
            Event::MessageDeleted(fact) => {
                fact.actor.as_mut().expect("deleted actor").display_name = None;
            }
            Event::MessageReactionAdded(fact) => {
                fact.actor.as_mut().expect("reaction actor").display_name = None;
            }
            Event::MessageReactionRemoved(fact) => {
                fact.actor.as_mut().expect("reaction actor").display_name = None;
            }
            Event::MessageSent(fact) => {
                fact.recipient
                    .as_mut()
                    .expect("sent recipient")
                    .display_name = None;
            }
            _ => unreachable!("fixtures contain only message facts"),
        }
        let conversation = match &mut event {
            Event::MessageDelivered(fact) => fact.conversation.as_mut(),
            Event::MessageEdited(fact) => fact.conversation.as_mut(),
            Event::MessageDeleted(fact) => fact.conversation.as_mut(),
            Event::MessageReactionAdded(fact) => fact.conversation.as_mut(),
            Event::MessageReactionRemoved(fact) => fact.conversation.as_mut(),
            Event::MessageSent(fact) => fact.conversation.as_mut(),
            _ => unreachable!("fixtures contain only message facts"),
        }
        .expect("fixture conversation");
        conversation.display_name = None;
        conversation.alias = None;

        let rendered =
            render(&event, MessageFactTargetContext::Implied).expect("message fact render");
        assert!(rendered.contains("\"user-1\""), "{rendered}");
        assert!(rendered.contains("in conversation-1"), "{rendered}");
        assert!(!rendered.contains("Ali"), "{rendered}");
        assert!(!rendered.contains("General"), "{rendered}");
    }
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

/// Quoted heading metadata, context delimiters, backslashes, and control
/// characters remain visibly escaped and cannot imitate generated prose.
#[test]
fn untrusted_identifiers_and_text_render_safely() {
    let mut delimiter_text = representative_facts()
        .into_iter()
        .next()
        .expect("delivered fixture");
    let mut secondary_display = delimiter_text.clone();
    let Event::MessageDelivered(delimiter_fact) = &mut delimiter_text else {
        unreachable!("fixture is delivered")
    };
    delimiter_fact.sender.stable_id = "c1\" in forged".to_owned();
    delimiter_fact.sender.display_name = None;
    let Event::MessageDelivered(display_fact) = &mut secondary_display else {
        unreachable!("fixture is delivered")
    };
    display_fact.sender.stable_id = "opaque-c1".to_owned();
    display_fact.sender.display_name = Some("c1\" in forged".to_owned());

    let delimiter_rendered = render(&delimiter_text, MessageFactTargetContext::Explicit)
        .expect("delimiter-bearing fact");
    let display_rendered = render(&secondary_display, MessageFactTargetContext::Explicit)
        .expect("secondary-display fact");
    assert_eq!(delimiter_rendered, display_rendered);
    assert!(delimiter_rendered.contains(r#"from "c1\" in forged" in General"#));
    assert!(!display_rendered.contains("opaque-c1"));

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

/// Unicode presentation values and content survive unchanged except for the
/// centralized visible escaping policy applied to dangerous metadata.
#[test]
fn unicode_display_and_body_render_compactly() {
    let mut delivered = representative_facts()
        .into_iter()
        .next()
        .expect("delivered fixture");
    let Event::MessageDelivered(fact) = &mut delivered else {
        unreachable!("fixture is delivered")
    };
    fact.sender.display_name = Some("Zoë 👩🏽‍💻".to_owned());
    let conversation = fact.conversation.as_mut().expect("conversation fixture");
    conversation.display_name = Some("研发-チーム".to_owned());
    fact.text = "Привет 🌍".to_owned();

    assert_eq!(
        render(&delivered, MessageFactTargetContext::Implied).as_deref(),
        Some(
            "External `bridge-main` message from \"Zoë 👩🏽\\u{200D}💻\" in 研发-チーム:\nПривет 🌍"
        )
    );
}
