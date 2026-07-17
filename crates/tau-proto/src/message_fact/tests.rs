use super::*;
use crate::{
    EXTENSION_NAME_MAX_BYTES, Event, HarnessInputMessage, MessageExtensionData,
    decode_harness_input_from_slice, encode_message_to_vec,
};

/// All client constructors emit the required opaque field as CBOR null while
/// retaining each fact's distinct v11 wire shape.
#[test]
fn constructors_default_required_extension_data_to_null() {
    let publisher = MessagePublisherId::new("bridge-main");
    let agent = MessageAgentTarget::new("agent");
    let target = MessageFactRef {
        publisher_extension_id: publisher.clone(),
        message_id: MessageFactId::new("m1"),
    };
    let party = MessageParty {
        stable_id: "u1".to_owned(),
        display_name: Some("Alice".to_owned()),
    };
    let conversation = Some(MessageConversation {
        stable_id: "c1".to_owned(),
        display_name: Some("General".to_owned()),
    });
    let facts = [
        Event::MessageDelivered(MessageDelivered::new(
            publisher.clone(),
            agent.clone(),
            MessageFactId::new("m1"),
            party.clone(),
            conversation.clone(),
            "delivered",
        )),
        Event::MessageEdited(MessageEdited::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            Some(party.clone()),
            conversation.clone(),
            "edited",
        )),
        Event::MessageDeleted(MessageDeleted::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            Some(party.clone()),
            conversation.clone(),
        )),
        Event::MessageReactionAdded(MessageReactionAdded::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            Some(party.clone()),
            conversation.clone(),
            "👍",
        )),
        Event::MessageReactionRemoved(MessageReactionRemoved::new(
            publisher.clone(),
            agent.clone(),
            target,
            Some(party.clone()),
            conversation.clone(),
            "👍",
        )),
        Event::MessageSent(MessageSent::new(
            publisher,
            agent,
            MessageFactId::new("m2"),
            Some(party),
            conversation,
            "sent",
        )),
    ];

    for fact in facts {
        let json = serde_json::to_value(&fact).expect("serialize constructor result");
        assert_eq!(json["payload"]["extension_data"], serde_json::Value::Null);
        let encoded = encode_message_to_vec(&HarnessInputMessage::emit(fact.clone()))
            .expect("encode constructor result");
        assert_eq!(
            decode_harness_input_from_slice(&encoded).expect("decode constructor result"),
            HarnessInputMessage::emit(fact)
        );
    }
}

/// Publisher identifiers use the shared configured extension-name grammar while
/// raw reference DTOs remain wire-decodable for post-commit diagnosis.
#[test]
fn publisher_id_grammar_is_bounded_ascii() {
    for valid in ["a", "Bridge_2", "bridge-main"] {
        assert!(MessagePublisherId::new(valid).is_valid(), "{valid}");
    }
    for invalid in [
        "",
        "bridge.main",
        "bridge main",
        "é",
        &"x".repeat(EXTENSION_NAME_MAX_BYTES + 1),
    ] {
        assert!(!MessagePublisherId::new(invalid).is_valid(), "{invalid}");
    }
}

/// Representative Telegram and XMPP deliveries fit the universal schema without
/// transport-specific typed fields, reserving live bridge migration for v12.
#[test]
fn telegram_and_xmpp_fit_delivered_schema() {
    for fact in [
        MessageDelivered {
            publisher_extension_id: MessagePublisherId::new("telegram-work"),
            agent_id: MessageAgentTarget::new("agent-a"),
            message_id: MessageFactId::new("chat:-100:message:42"),
            sender: MessageParty {
                stable_id: "123456".to_owned(),
                display_name: Some("alice".to_owned()),
            },
            conversation: Some(MessageConversation {
                stable_id: "-100".to_owned(),
                display_name: Some("Ops".to_owned()),
            }),
            text: "telegram body".to_owned(),
            extension_data: MessageExtensionData::default(),
        },
        MessageDelivered {
            publisher_extension_id: MessagePublisherId::new("xmpp-main"),
            agent_id: MessageAgentTarget::new("agent-a"),
            message_id: MessageFactId::new("room@example.test:stanza-7"),
            sender: MessageParty {
                stable_id: "room@example.test/alice".to_owned(),
                display_name: Some("alice".to_owned()),
            },
            conversation: Some(MessageConversation {
                stable_id: "room@example.test".to_owned(),
                display_name: None,
            }),
            text: "xmpp body".to_owned(),
            extension_data: MessageExtensionData::default(),
        },
    ] {
        let object = serde_json::to_value(&fact)
            .expect("serialize schema fixture")
            .as_object()
            .cloned()
            .expect("fact object");
        assert_eq!(
            object
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "agent_id",
                "conversation",
                "extension_data",
                "message_id",
                "publisher_extension_id",
                "sender",
                "text",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
    }
}
