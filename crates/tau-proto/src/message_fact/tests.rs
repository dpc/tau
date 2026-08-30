use super::*;
use crate::{
    CborValue, EXTENSION_NAME_MAX_BYTES, Event, HarnessInputMessage, MessageExtensionData,
    decode_harness_input_from_slice, encode_message_to_vec,
};

/// Sender-authentication outcomes retain their concise model/wire spellings
/// across serialization and replay decoding.
#[test]
fn sender_auth_outcomes_round_trip_with_stable_spellings() {
    for (value, spelling) in [
        (
            MessageSenderAuth::VerifiedAllowlisted,
            "verified_allowlisted",
        ),
        (
            MessageSenderAuth::VerifiedConversationAuthorized,
            "verified_conversation_authorized",
        ),
        (MessageSenderAuth::TrustedMembership, "trusted_membership"),
    ] {
        let encoded = serde_json::to_string(&value).expect("encode sender auth");
        assert_eq!(encoded, format!("\"{spelling}\""));
        assert_eq!(
            serde_json::from_str::<MessageSenderAuth>(&encoded).expect("decode sender auth"),
            value
        );
    }
}

/// All client constructors emit the required opaque field as CBOR null while
/// retaining each fact's distinct current wire shape.
#[test]
fn constructors_default_required_extension_data_to_null() {
    let publisher = crate::MessagePublisherId::parse("bridge-main")
        .expect("canonical publisher id must satisfy the identifier grammar");
    let agent = MessageAgentTarget::new("agent");
    let target = MessageFactRef {
        publisher_extension_id: RawMessagePublisherId::new(publisher.as_str()),
        message_id: MessageFactId::new("m1"),
    };
    let party = MessageParty {
        stable_id: "u1".to_owned(),
        display_name: Some("Alice".to_owned()),
        sender_auth: None,
    };
    let conversation = Some(MessageConversation {
        stable_id: "c1".to_owned(),
        display_name: Some("General".to_owned()),
        alias: None,
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

/// Every fact projects to the generic boundary with its specified role and
/// activation behavior, without resolving operation references.
#[test]
fn all_message_facts_project_with_generic_roles_and_escaping() {
    let publisher = crate::MessagePublisherId::parse("bridge-main")
        .expect("canonical publisher id must satisfy the identifier grammar");
    let agent = MessageAgentTarget::new("agent-1");
    let target = MessageFactRef {
        publisher_extension_id: RawMessagePublisherId::new(publisher.as_str()),
        message_id: MessageFactId::new("m<&1"),
    };
    let party = MessageParty {
        stable_id: "u\"1".to_owned(),
        display_name: Some("Ali\u{202e}ce".to_owned()),
        sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
    };
    let conversation = Some(MessageConversation {
        stable_id: "c1".to_owned(),
        display_name: Some("Gen&eral".to_owned()),
        alias: Some("room&alias".to_owned()),
    });
    let mut delivered = MessageDelivered::new(
        publisher.clone(),
        agent.clone(),
        MessageFactId::new("m1"),
        party.clone(),
        conversation.clone(),
        "<hello>",
    );
    delivered.extension_data =
        MessageExtensionData::new(CborValue::Text("opaque sentinel".to_owned()))
            .expect("bounded opaque data");
    let facts = [
        Event::MessageDelivered(delivered),
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
            Some(party),
            conversation,
            "👍",
        )),
        Event::MessageSent(MessageSent::new(
            publisher,
            agent,
            MessageFactId::new("m2"),
            Some(MessageParty {
                stable_id: "recipient-1".to_owned(),
                display_name: Some("Recipient".to_owned()),
                sender_auth: None,
            }),
            None,
            "sent",
        )),
    ];

    let mut rendered = Vec::new();
    for fact in &facts {
        let projection = project_message_fact(fact)
            .expect("message fact")
            .expect("valid projection");
        let (expected_role, expected_activation) = match fact {
            Event::MessageSent(_) => (ContextRole::Assistant, false),
            Event::MessageDelivered(_)
            | Event::MessageEdited(_)
            | Event::MessageDeleted(_)
            | Event::MessageReactionAdded(_)
            | Event::MessageReactionRemoved(_) => (ContextRole::User, true),
            _ => unreachable!("fixture contains only message facts"),
        };
        assert_eq!(projection.item.role, expected_role);
        assert_eq!(projection.activates_model, expected_activation);
        let (ContentPart::Text { text }
        | ContentPart::SyntheticCompactionSummary { text }
        | ContentPart::HarnessInternalText { text }) = &projection.item.content[0]
        else {
            panic!("projection must contain narrative text")
        };
        assert!(text.starts_with("<message event="));
        assert!(text.contains("<hello>") || !matches!(fact, Event::MessageDelivered(_)));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains("opaque sentinel"));
        rendered.push(text.clone());
    }
    assert_eq!(
        rendered,
        vec![
            "<message event=\"created\" publisher=\"bridge-main\" message_ref=\"m1\" sender_ref=\"u&quot;1\" sender_display=\"Ali\\u{202E}ce\" sender_auth=\"verified_allowlisted\" conversation=\"room&amp;alias\" content_trust=\"external\"><hello></message>",
            "<message event=\"edited\" publisher=\"bridge-main\" message_ref=\"m&lt;&amp;1\" sender_ref=\"u&quot;1\" sender_display=\"Ali\\u{202E}ce\" sender_auth=\"verified_allowlisted\" conversation=\"room&amp;alias\" content_trust=\"external\">edited</message>",
            "<message event=\"deleted\" publisher=\"bridge-main\" message_ref=\"m&lt;&amp;1\" sender_ref=\"u&quot;1\" sender_display=\"Ali\\u{202E}ce\" sender_auth=\"verified_allowlisted\" conversation=\"room&amp;alias\"/>",
            "<message event=\"reaction_added\" publisher=\"bridge-main\" message_ref=\"m&lt;&amp;1\" sender_ref=\"u&quot;1\" sender_display=\"Ali\\u{202E}ce\" sender_auth=\"verified_allowlisted\" conversation=\"room&amp;alias\" reaction=\"👍\"/>",
            "<message event=\"reaction_removed\" publisher=\"bridge-main\" message_ref=\"m&lt;&amp;1\" sender_ref=\"u&quot;1\" sender_display=\"Ali\\u{202E}ce\" sender_auth=\"verified_allowlisted\" conversation=\"room&amp;alias\" reaction=\"👍\"/>",
            "<message event=\"sent\" publisher=\"bridge-main\" message_ref=\"m2\" recipient_ref=\"recipient-1\" recipient_display=\"Recipient\">sent</message>",
        ]
    );
}

/// Message bodies replace every exact own close while preserving attributes,
/// entities, foreign wrappers, near variants, Unicode, and newlines.
#[test]
fn message_fact_body_uses_exact_close_framing() {
    let fact = Event::MessageDelivered(MessageDelivered::new(
        crate::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("agent-1"),
        MessageFactId::new("m1"),
        MessageParty {
            stable_id: "sender\"&".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "Don't &apos; <user>x</user> </MESSAGE> </message > 雪\n</message></message>",
    ));
    let projection = project_message_fact(&fact)
        .expect("message fact")
        .expect("valid projection");
    let (ContentPart::Text { text }
    | ContentPart::SyntheticCompactionSummary { text }
    | ContentPart::HarnessInternalText { text }) = &projection.item.content[0]
    else {
        panic!("projection must contain narrative text")
    };

    assert_eq!(text.matches("</message>").count(), 1);
    assert_eq!(
        text,
        "<message event=\"created\" publisher=\"bridge-main\" message_ref=\"m1\" sender_ref=\"sender&quot;&amp;\" content_trust=\"external\">Don't &apos; <user>x</user> </MESSAGE> </message > 雪\\u{000A}&lt;/message&gt;&lt;/message&gt;</message>"
    );
}

/// Universal validation follows deterministic first-match precedence.
#[test]
fn message_projection_failure_precedence_is_stable() {
    let mut fact = MessageDelivered::new(
        crate::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("invalid target"),
        MessageFactId::new(""),
        MessageParty {
            stable_id: String::new(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "",
    );
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidTarget))
    );
    fact.agent_id = MessageAgentTarget::new("agent");
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidMessageId))
    );
    fact.message_id = MessageFactId::new("m1");
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidParty))
    );
    fact.sender.stable_id = "u1".to_owned();
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::EmptyText))
    );
    fact.conversation = Some(MessageConversation {
        stable_id: String::new(),
        display_name: None,
        alias: None,
    });
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidConversation))
    );
    fact.conversation = None;
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact.clone())),
        Some(Err(MessageProjectionFailure::EmptyText))
    );
    fact.text = "x".repeat(131_073);
    assert_eq!(
        project_message_fact(&Event::MessageDelivered(fact)),
        Some(Err(MessageProjectionFailure::TextTooLarge))
    );
}

/// Operation-specific reference, metadata, and reaction limits map to their
/// exact deterministic categories.
#[test]
fn message_projection_classifies_operation_metadata_failures() {
    let agent = MessageAgentTarget::new("agent");
    let valid_ref = MessageFactRef {
        publisher_extension_id: RawMessagePublisherId::new("bridge"),
        message_id: MessageFactId::new("m1"),
    };
    let invalid_reference = Event::MessageDeleted(MessageDeleted::new(
        crate::MessagePublisherId::parse("bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        agent.clone(),
        MessageFactRef {
            publisher_extension_id: RawMessagePublisherId::new("invalid publisher"),
            message_id: MessageFactId::new("m1"),
        },
        None,
        None,
    ));
    assert_eq!(
        project_message_fact(&invalid_reference),
        Some(Err(MessageProjectionFailure::InvalidReference))
    );

    let invalid_party = Event::MessageDeleted(MessageDeleted::new(
        crate::MessagePublisherId::parse("bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        agent.clone(),
        valid_ref.clone(),
        Some(MessageParty {
            stable_id: "u1".to_owned(),
            display_name: Some("x".repeat(257)),
            sender_auth: None,
        }),
        None,
    ));
    assert_eq!(
        project_message_fact(&invalid_party),
        Some(Err(MessageProjectionFailure::InvalidParty))
    );

    let invalid_conversation = Event::MessageEdited(MessageEdited::new(
        crate::MessagePublisherId::parse("bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        agent.clone(),
        valid_ref.clone(),
        None,
        Some(MessageConversation {
            stable_id: String::new(),
            display_name: None,
            alias: None,
        }),
        "edit",
    ));
    assert_eq!(
        project_message_fact(&invalid_conversation),
        Some(Err(MessageProjectionFailure::InvalidConversation))
    );

    let invalid_reaction = Event::MessageReactionAdded(MessageReactionAdded::new(
        crate::MessagePublisherId::parse("bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        agent,
        valid_ref,
        None,
        None,
        "",
    ));
    assert_eq!(
        project_message_fact(&invalid_reaction),
        Some(Err(MessageProjectionFailure::InvalidReaction))
    );
}

/// Operation facts use the same first-match precedence across every applicable
/// invalid field.
#[test]
fn operation_message_projection_failure_precedence_is_stable() {
    let mut fact = MessageReactionAdded::new(
        crate::MessagePublisherId::parse("bridge")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("invalid target"),
        MessageFactRef {
            publisher_extension_id: RawMessagePublisherId::new("invalid publisher"),
            message_id: MessageFactId::new(""),
        },
        Some(MessageParty {
            stable_id: String::new(),
            display_name: None,
            sender_auth: None,
        }),
        Some(MessageConversation {
            stable_id: String::new(),
            display_name: None,
            alias: None,
        }),
        "",
    );
    assert_eq!(
        project_message_fact(&Event::MessageReactionAdded(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidTarget))
    );
    fact.agent_id = MessageAgentTarget::new("agent");
    assert_eq!(
        project_message_fact(&Event::MessageReactionAdded(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidReference))
    );
    fact.target = MessageFactRef {
        publisher_extension_id: RawMessagePublisherId::new("bridge"),
        message_id: MessageFactId::new("m1"),
    };
    assert_eq!(
        project_message_fact(&Event::MessageReactionAdded(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidParty))
    );
    fact.actor = None;
    assert_eq!(
        project_message_fact(&Event::MessageReactionAdded(fact.clone())),
        Some(Err(MessageProjectionFailure::InvalidConversation))
    );
    fact.conversation = None;
    assert_eq!(
        project_message_fact(&Event::MessageReactionAdded(fact)),
        Some(Err(MessageProjectionFailure::InvalidReaction))
    );
}

/// Publisher identifiers use the shared configured extension-name grammar while
/// raw reference DTOs remain wire-decodable for post-commit diagnosis.
#[test]
fn publisher_id_grammar_is_bounded_ascii() {
    for valid in ["a", "Bridge_2", "bridge-main"] {
        assert!(MessagePublisherId::parse(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "bridge.main",
        "bridge main",
        "é",
        &"x".repeat(EXTENSION_NAME_MAX_BYTES + 1),
    ] {
        assert!(MessagePublisherId::parse(invalid).is_err(), "{invalid}");
        let raw = RawMessagePublisherId::new(invalid);
        assert_eq!(raw.as_str(), invalid, "raw claims remain lossless");
    }
}

/// Representative Telegram and XMPP deliveries fit the universal schema without
/// transport-specific typed fields.
#[test]
fn telegram_and_xmpp_fit_delivered_schema() {
    for fact in [
        MessageDelivered {
            publisher_extension_id: crate::MessagePublisherId::parse("telegram-work")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent-a"),
            message_id: MessageFactId::new("chat:-100:message:42"),
            sender: MessageParty {
                stable_id: "123456".to_owned(),
                display_name: Some("alice".to_owned()),
                sender_auth: None,
            },
            conversation: Some(MessageConversation {
                stable_id: "-100".to_owned(),
                display_name: Some("Ops".to_owned()),
                alias: None,
            }),
            text: "telegram body".to_owned(),
            extension_data: MessageExtensionData::default(),
        },
        MessageDelivered {
            publisher_extension_id: crate::MessagePublisherId::parse("xmpp-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent-a"),
            message_id: MessageFactId::new("room@example.test:stanza-7"),
            sender: MessageParty {
                stable_id: "room@example.test/alice".to_owned(),
                display_name: Some("alice".to_owned()),
                sender_auth: None,
            },
            conversation: Some(MessageConversation {
                stable_id: "room@example.test".to_owned(),
                display_name: None,
                alias: None,
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
