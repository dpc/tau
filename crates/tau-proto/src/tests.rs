use super::*;

fn test_session_id(value: impl Into<String>) -> SessionId {
    SessionId::parse(value).expect("test session id")
}

fn test_agent_prompt_id(value: impl Into<String>) -> AgentPromptId {
    AgentPromptId::parse(value).expect("test agent prompt id")
}

/// Session and prompt identifiers accept only the approved bounded ASCII
/// grammar.
#[test]
fn session_scoped_identifiers_enforce_their_grammar() {
    let max = "a".repeat(SESSION_SCOPED_ID_MAX_LEN);
    assert_eq!(
        SessionId::parse("session_A-1")
            .expect("valid session id")
            .as_str(),
        "session_A-1"
    );
    assert_eq!(
        AgentPromptId::parse(max.clone())
            .expect("maximum-length prompt id")
            .as_str(),
        max
    );

    for invalid in ["", ".", "has/slash", "has space", "nul\0", "é"] {
        assert!(SessionId::parse(invalid).is_err(), "{invalid:?}");
        assert!(AgentPromptId::parse(invalid).is_err(), "{invalid:?}");
    }
    assert!(SessionId::parse("a".repeat(SESSION_SCOPED_ID_MAX_LEN + 1)).is_err());
    assert!(AgentPromptId::parse("a".repeat(SESSION_SCOPED_ID_MAX_LEN + 1)).is_err());
}

/// Serde decoding must reject invalid controlled identifiers so journals and
/// protocol messages fail closed instead of retaining unsafe historical values.
#[test]
fn session_scoped_identifier_deserialization_fails_closed() {
    for encoded in [r#""""#, r#""bad\u001bvalue""#, r#""bad.value""#] {
        assert!(serde_json::from_str::<SessionId>(encoded).is_err());
        assert!(serde_json::from_str::<AgentPromptId>(encoded).is_err());
    }

    let mut invalid_session = Vec::new();
    let mut invalid_prompt = Vec::new();
    ciborium::into_writer("bad.session", &mut invalid_session)
        .expect("encode invalid historical session id");
    ciborium::into_writer("bad prompt", &mut invalid_prompt)
        .expect("encode invalid historical prompt id");
    assert!(ciborium::from_reader::<SessionId, _>(invalid_session.as_slice()).is_err());
    assert!(ciborium::from_reader::<AgentPromptId, _>(invalid_prompt.as_slice()).is_err());
}

/// Every approved ASCII-ID family enforces its independent byte cap and the
/// complete shared lexical grammar at construction.
#[test]
fn controlled_identifier_families_enforce_all_boundaries() {
    macro_rules! assert_family {
        ($type:ty, $max:expr) => {{
            let allowed = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
            for character in allowed.chars() {
                assert!(<$type>::parse(character.to_string()).is_ok());
            }
            assert!(<$type>::parse("x".repeat($max)).is_ok());
            assert!(<$type>::parse("").is_err());
            assert!(<$type>::parse("x".repeat($max + 1)).is_err());

            for byte in 0_u8..=31 {
                assert!(<$type>::parse(format!("a{}b", char::from(byte))).is_err());
            }
            assert!(<$type>::parse(format!("a{}b", char::from(127))).is_err());
            for codepoint in 128_u32..=159 {
                let character = char::from_u32(codepoint).expect("C1 codepoint");
                assert!(<$type>::parse(format!("a{character}b")).is_err());
            }
            for invalid in ["é", "a/b", r"a\b", "a.b", "a b", "a:b", "a+b", "a=b", "a@b"] {
                assert!(<$type>::parse(invalid).is_err(), "{invalid:?}");
            }
        }};
    }

    assert_family!(AgentMessageId, 128);
    assert_family!(ActionInvocationId, 64);
    assert_family!(ShellCommandId, 64);
    assert_family!(AgentInitializationId, 64);
    assert_family!(AccountingRuntimeId, 32);
    assert_family!(AgentActivationCorrelationId, 128);
    assert_family!(ConnectionId, 128);
    assert_family!(ExtensionName, EXTENSION_NAME_MAX_BYTES);
    assert_family!(MessagePublisherId, 128);
}

/// Every approved ASCII-ID family keeps transparent JSON/CBOR output and
/// rejects malformed values through both deserializers.
#[test]
fn controlled_identifier_families_fail_closed_in_json_and_cbor() {
    macro_rules! assert_serde {
        ($type:ty, $valid:literal) => {{
            let identifier = <$type>::parse($valid).expect("valid controlled identifier");
            assert_eq!(
                serde_json::to_string(&identifier).expect("encode identifier as JSON"),
                concat!("\"", $valid, "\"")
            );
            assert_eq!(
                serde_json::from_str::<$type>(concat!("\"", $valid, "\""))
                    .expect("decode valid identifier from JSON"),
                identifier
            );

            let mut valid_cbor = Vec::new();
            ciborium::into_writer($valid, &mut valid_cbor)
                .expect("encode valid identifier as CBOR");
            assert_eq!(
                ciborium::from_reader::<$type, _>(valid_cbor.as_slice())
                    .expect("decode valid identifier from CBOR"),
                identifier
            );

            for invalid in ["", "bad.value", "bad/value", "bad value", "é"] {
                let json = serde_json::to_string(invalid).expect("encode invalid JSON string");
                assert!(serde_json::from_str::<$type>(&json).is_err());
                let mut cbor = Vec::new();
                ciborium::into_writer(invalid, &mut cbor)
                    .expect("encode invalid identifier as CBOR text");
                assert!(ciborium::from_reader::<$type, _>(cbor.as_slice()).is_err());
            }
        }};
    }

    assert_serde!(AgentMessageId, "message_1");
    assert_serde!(ActionInvocationId, "action_1");
    assert_serde!(ShellCommandId, "shell_1");
    assert_serde!(AgentInitializationId, "initialization_1");
    assert_serde!(AccountingRuntimeId, "runtime_1");
    assert_serde!(AgentActivationCorrelationId, "activation_1");
    assert_serde!(ConnectionId, "connection_1");
    assert_serde!(ExtensionName, "extension_1");
    assert_serde!(MessagePublisherId, "publisher_1");
}

/// Raw publisher claims remain unrestricted and lossless across both wire
/// codecs so report admission can audit or reject the exact peer value.
#[test]
fn raw_message_publisher_claim_round_trips_without_validation() {
    for claim in ["", "invalid publisher/☃", "control\u{1b}claim"] {
        let raw = RawMessagePublisherId::new(claim);
        let json = serde_json::to_string(&raw).expect("encode raw claim as JSON");
        assert_eq!(
            serde_json::from_str::<RawMessagePublisherId>(&json)
                .expect("decode raw claim from JSON")
                .as_str(),
            claim
        );
        let mut cbor = Vec::new();
        ciborium::into_writer(&raw, &mut cbor).expect("encode raw claim as CBOR");
        assert_eq!(
            ciborium::from_reader::<RawMessagePublisherId, _>(cbor.as_slice())
                .expect("decode raw claim from CBOR")
                .as_str(),
            claim
        );
    }
}

/// Outer-turn identity accepts only the exact derived `ot-{AgentPromptId}`
/// representation, including the prompt family's complete boundary behavior.
#[test]
fn outer_turn_identifier_enforces_exact_derived_grammar() {
    let max_prompt = "p".repeat(SESSION_SCOPED_ID_MAX_LEN);
    let maximum = format!("ot-{max_prompt}");
    assert_eq!(
        &*AgentOuterTurnId::parse(&maximum).expect("maximum derived outer-turn id"),
        maximum
    );
    for invalid in [
        "",
        "ot-",
        "turn-prompt",
        "ot-bad.prompt",
        "ot-bad/prompt",
        "ot-bad prompt",
        "ot-é",
    ] {
        assert!(AgentOuterTurnId::parse(invalid).is_err(), "{invalid:?}");
        let json = serde_json::to_string(invalid).expect("encode invalid outer-turn JSON");
        assert!(serde_json::from_str::<AgentOuterTurnId>(&json).is_err());
        let mut cbor = Vec::new();
        ciborium::into_writer(invalid, &mut cbor).expect("encode invalid outer-turn CBOR");
        assert!(ciborium::from_reader::<AgentOuterTurnId, _>(cbor.as_slice()).is_err());
    }
    assert!(
        AgentOuterTurnId::parse(format!("ot-{}", "p".repeat(SESSION_SCOPED_ID_MAX_LEN + 1)))
            .is_err()
    );
}

/// Invalid controlled identifiers fail at representative protocol and durable
/// event payload boundaries instead of entering routing or journal state.
#[test]
fn controlled_identifiers_fail_closed_at_protocol_and_journal_boundaries() {
    let invalid_hello = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "client_name": "bad.extension",
        "client_kind": "tool",
        "capabilities": []
    });
    assert!(serde_json::from_value::<Hello>(invalid_hello.clone()).is_err());
    let mut hello_cbor = Vec::new();
    ciborium::into_writer(&invalid_hello, &mut hello_cbor)
        .expect("encode malformed protocol payload");
    assert!(ciborium::from_reader::<Hello, _>(hello_cbor.as_slice()).is_err());

    let invalid_message = serde_json::json!({
        "message_id": "bad.message",
        "sender_id": "agent-sender",
        "recipient": {"kind": "user"},
        "message": "hello"
    });
    assert!(serde_json::from_value::<AgentMessageSent>(invalid_message.clone()).is_err());
    let mut message_cbor = Vec::new();
    ciborium::into_writer(&invalid_message, &mut message_cbor)
        .expect("encode malformed journal payload");
    assert!(ciborium::from_reader::<AgentMessageSent, _>(message_cbor.as_slice()).is_err());
}

/// Connection and extension identities cannot escape a path component because
/// their owning parsers reject separators, traversal punctuation, and controls.
#[test]
fn routing_identifier_path_components_are_safe_by_construction() {
    for invalid in [".", "..", "/", "\\", "a/b", r"a\b", "a\0b", "a\nb"] {
        assert!(ConnectionId::parse(invalid).is_err(), "{invalid:?}");
        assert!(ExtensionName::parse(invalid).is_err(), "{invalid:?}");
    }
}

/// The transient internal-prompt request must preserve absent/default
/// provenance and encode explicit timer provenance canonically.
#[test]
fn internal_prompt_activation_kind_is_optional_and_typed() {
    let base = ExtInternalPromptSubmitRequest {
        agent_id: AgentId::parse("agent-timer").expect("agent id"),
        text: "wake".into(),
        ctx_id: None,
        activation_kind: None,
    };
    let absent = serde_json::to_value(&base).expect("serialize absent provenance");
    assert!(absent.get("activation_kind").is_none());
    assert_eq!(
        serde_json::from_value::<ExtInternalPromptSubmitRequest>(absent)
            .expect("decode absent provenance")
            .activation_kind,
        None
    );
    let timer = serde_json::to_value(ExtInternalPromptSubmitRequest {
        activation_kind: Some(InternalPromptActivationKind::Timer),
        ..base
    })
    .expect("serialize timer provenance");
    assert_eq!(timer["activation_kind"], "timer");
}

/// Locks the persisted adjacent-tag shape for both activation authorities so a
/// serde attribute change cannot silently make existing agent journals
/// unreadable.
#[test]
fn outer_turn_activation_shapes_round_trip_in_json_and_cbor() {
    let cases = [
        (
            AgentOuterTurnActivation::Journal {
                occurrence: AgentHead::Node(NodeId::new(7)),
            },
            serde_json::json!({
                "kind": "journal",
                "details": {
                    "occurrence": {
                        "kind": "node",
                        "node_id": 7
                    }
                }
            }),
        ),
        (
            AgentOuterTurnActivation::External {
                correlation_id: AgentActivationCorrelationId::parse("accepted-7")
                    .expect("test identifier must be valid"),
            },
            serde_json::json!({
                "kind": "external",
                "details": {
                    "correlation_id": "accepted-7"
                }
            }),
        ),
    ];
    for (activation, expected) in cases {
        let json = serde_json::to_value(&activation).expect("serialize activation JSON");
        assert_eq!(json, expected);
        assert_eq!(
            serde_json::from_value::<AgentOuterTurnActivation>(json)
                .expect("deserialize activation JSON"),
            activation
        );

        let mut encoded = Vec::new();
        ciborium::into_writer(&activation, &mut encoded).expect("serialize activation CBOR");
        let shape: CborValue =
            ciborium::from_reader(encoded.as_slice()).expect("decode activation CBOR shape");
        let CborValue::Map(fields) = shape else {
            panic!("activation CBOR must be a map");
        };
        let field = |name: &str| {
            fields.iter().find_map(|(key, value)| {
                (key == &CborValue::Text(name.to_owned())).then_some(value)
            })
        };
        assert_eq!(
            field("kind"),
            Some(&json_to_cbor(&expected["kind"])),
            "CBOR kind"
        );
        assert_eq!(
            field("details"),
            Some(&json_to_cbor(&expected["details"])),
            "CBOR details"
        );
        assert_eq!(
            ciborium::from_reader::<AgentOuterTurnActivation, _>(encoded.as_slice())
                .expect("deserialize activation CBOR"),
            activation
        );
    }
}

/// Locks the nested activation envelope inside its real event, preventing a
/// variant-only round trip from missing top-level event encoding regressions.
#[test]
fn outer_turn_started_event_preserves_nested_activation_shape() {
    let event = Event::AgentOuterTurnStarted(AgentOuterTurnStarted {
        agent_id: AgentId::parse("agent-7").expect("agent id"),
        session_id: test_session_id("session-7"),
        outer_turn_id: AgentOuterTurnId::for_prompt(&test_agent_prompt_id("ap-agent-7-0")),
        agent_prompt_id: test_agent_prompt_id("ap-agent-7-0"),
        runtime_id: AccountingRuntimeId::parse("runtime-7").expect("test identifier must be valid"),
        activation: AgentOuterTurnActivation::Journal {
            occurrence: AgentHead::Node(NodeId::new(7)),
        },
    });
    let json = serde_json::to_value(&event).expect("serialize event JSON");
    assert_eq!(json["event"], "agent.outer_turn_started");
    assert_eq!(json["payload"]["activation"]["kind"], "journal");
    assert_eq!(
        json["payload"]["activation"]["details"]["occurrence"],
        serde_json::json!({"kind": "node", "node_id": 7})
    );
    assert_eq!(
        serde_json::from_value::<Event>(json).expect("deserialize event JSON"),
        event
    );

    let mut encoded = Vec::new();
    ciborium::into_writer(&event, &mut encoded).expect("serialize event CBOR");
    assert_eq!(
        ciborium::from_reader::<Event, _>(encoded.as_slice()).expect("deserialize event CBOR"),
        event
    );
}

/// User-entered references may carry one convenience sigil, while canonical
/// parsing, storage, and serialization remain unsigiled and strict.
#[test]
fn agent_id_reference_parsing_normalizes_one_optional_at_prefix() {
    let bare = AgentId::parse_reference("worker-1").expect("bare reference");
    let prefixed = AgentId::parse_reference("@worker-1").expect("prefixed reference");

    assert_eq!(bare, prefixed);
    assert_eq!(prefixed.as_str(), "worker-1");
    assert_eq!(
        serde_json::to_string(&prefixed).expect("serialize agent id"),
        "\"worker-1\""
    );
    assert!(AgentId::parse("@worker-1").is_err());
    assert!(serde_json::from_str::<AgentId>("\"@worker-1\"").is_err());
}

/// Reference parsing must not turn empty, multiply sigiled, malformed, or
/// overlong user input into a valid durable identity.
#[test]
fn agent_id_reference_parsing_preserves_identifier_validation() {
    for invalid in ["", "@", "@@worker", "@bad/id", "@bad id"] {
        assert!(
            AgentId::parse_reference(invalid).is_err(),
            "{invalid:?} must be rejected"
        );
    }

    let longest = "a".repeat(AGENT_ID_MAX_LEN);
    assert!(AgentId::parse_reference(format!("@{longest}")).is_ok());
    assert!(AgentId::parse_reference(format!("@{longest}a")).is_err());
}

/// Metadata mutation ids enforce their byte bound at construction and serde
/// boundaries so a committed correlation cannot be silently rejected later.
#[test]
fn metadata_mutation_id_rejects_empty_and_oversized_values() {
    assert!(AgentMetadataMutationId::parse("").is_err());
    let oversized = "x".repeat(AGENT_METADATA_MUTATION_ID_MAX_BYTES + 1);
    assert!(AgentMetadataMutationId::parse(oversized.clone()).is_err());
    assert!(
        serde_json::from_value::<AgentMetadataMutationId>(serde_json::json!(oversized)).is_err()
    );
}

/// Metadata mutation requests have distinct transient wire names while their
/// canonical successor facts retain durable defaults.
#[test]
fn metadata_requests_have_distinct_transient_wire_names() {
    let agent_id = AgentId::parse("metadata-agent").expect("agent id");
    let set = AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: AgentMetadataKey::new("ext_test_value"),
        value: CborValue::Text("value".to_owned()),
        mutation_id: None,
        inheritable: true,
    };
    let unset = AgentMetadataUnset {
        agent_id,
        key: AgentMetadataKey::new("ext_test_value"),
    };
    for (event, name) in [
        (
            Event::AgentMetadataSetRequest(set.clone()),
            EventName::AGENT_METADATA_SET_REQUEST,
        ),
        (
            Event::AgentMetadataUnsetRequest(unset.clone()),
            EventName::AGENT_METADATA_UNSET_REQUEST,
        ),
    ] {
        assert_eq!(event.name(), name);
        assert!(!event.defaults_to_persist());
        let encoded = serde_json::to_value(&event).expect("encode request");
        let expected_name = name.to_string();
        assert_eq!(
            encoded.get("event").and_then(serde_json::Value::as_str),
            Some(expected_name.as_str())
        );
        assert_eq!(
            serde_json::from_value::<Event>(encoded).expect("decode request"),
            event
        );
    }
    assert!(Event::AgentMetadataSet(set).defaults_to_persist());
    assert!(Event::AgentMetadataUnset(unset).defaults_to_persist());
}

/// Literal prompt provenance survives the UI wire boundary while legacy
/// requests without the marker retain ordinary command-processing behavior.
#[test]
fn ui_prompt_literal_marker_round_trips_and_defaults_false() {
    let request = UiPromptSubmitted {
        session_id: test_session_id("session"),
        text: ":skill example".to_owned(),
        literal: true,
        agent_id: AgentId::parse("worker").expect("agent id"),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        ctx_id: None,
    };
    let mut encoded = serde_json::to_value(&request).expect("encode literal prompt");
    assert_eq!(encoded.get("literal"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(
        serde_json::from_value::<UiPromptSubmitted>(encoded.clone())
            .expect("decode literal prompt"),
        request
    );

    encoded
        .as_object_mut()
        .expect("prompt object")
        .remove("literal");
    let legacy =
        serde_json::from_value::<UiPromptSubmitted>(encoded).expect("decode legacy prompt");
    assert!(!legacy.literal);
}

/// Prefix syntax rejects ambiguous separators and non-provider-safe bytes.
#[test]
fn tool_prefix_validation_is_segmented_ascii() {
    for valid in ["a", "Work2", "team_ops"] {
        assert!(ToolNamePrefix::parse(valid).is_ok(), "{valid}");
    }
    for invalid in ["", "_a", "a_", "a__b", "a-b", "a b", "é"] {
        assert!(ToolNamePrefix::parse(invalid).is_err(), "{invalid}");
    }
}

/// Composition is exactly additive and envelope checks require a complete
/// underscore-delimited prefix component.
#[test]
fn tool_prefix_composition_is_additive_and_envelope_is_exact() {
    let prefix = ToolNamePrefix::parse("work").expect("prefix");
    assert_eq!(
        prefix
            .compose_tool_name(&ToolName::new("work_send"))
            .expect("compose")
            .as_str(),
        "work_work_send"
    );
    assert!(prefix.contains_tool_name(&ToolName::new("work_send")));
    assert!(!prefix.contains_tool_name(&ToolName::new("workspace_send")));
}

/// Ensures typed image output survives the durable CBOR protocol with exact
/// call provenance and bytes while its `Debug` representation stays
/// metadata-only.
#[test]
fn typed_image_tool_result_cbor_roundtrip_and_safe_debug() {
    let event = Event::ToolResult(ToolResult {
        call_id: "call-image".into(),
        tool_name: ToolName::new("read_image"),
        tool_type: ToolType::Function,
        result: CborValue::Text("image/png image, 1x1".to_owned()),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: b"\x89PNG\r\n\x1a\nDATA".to_vec().into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    });

    let mut encoded = Vec::new();
    ciborium::into_writer(&event, &mut encoded).expect("encode image event");
    let encoded_value: CborValue =
        ciborium::from_reader(encoded.as_slice()).expect("decode image event as generic CBOR");
    fn contains_bytes(value: &CborValue, expected: &[u8]) -> bool {
        match value {
            CborValue::Bytes(bytes) => bytes == expected,
            CborValue::Array(values) => values.iter().any(|value| contains_bytes(value, expected)),
            CborValue::Map(entries) => entries.iter().any(|(key, value)| {
                contains_bytes(key, expected) || contains_bytes(value, expected)
            }),
            CborValue::Tag(_, value) => contains_bytes(value, expected),
            _ => false,
        }
    }
    assert!(
        contains_bytes(&encoded_value, b"\x89PNG\r\n\x1a\nDATA"),
        "image data must use a CBOR byte string rather than an integer array"
    );
    let decoded: Event = ciborium::from_reader(encoded.as_slice()).expect("decode image event");
    assert_eq!(decoded, event);
    let debug = format!("{event:?}");
    assert!(debug.contains("<12 bytes>"));
    assert!(!debug.contains("[137, 80, 78, 71"));
}

/// Ensures prompt/event projections share canonical immutable image bytes
/// in-process instead of deep-copying a potentially multi-megabyte buffer.
#[test]
fn typed_image_clone_shares_immutable_bytes() {
    let image = ImageContent {
        media_type: ImageMediaType::Png,
        data: b"\x89PNG\r\n\x1a\nDATA".to_vec().into(),
        width: 1,
        height: 1,
        detail: ImageDetail::High,
    };
    let cloned = image.clone();

    assert!(std::sync::Arc::ptr_eq(&image.data, &cloned.data));
}

/// Manual retry controls and their correlated provider result must retain exact
/// prompt identity and typed scheduler status across the wire codec.
#[test]
fn retry_prompt_events_round_trip() {
    let request = Event::UiRetryPrompt(UiRetryPrompt {
        request_id: RetryPromptRequestId::parse("retry-1").expect("valid retry request id"),
        session_id: test_session_id("session-1"),
        target_agent_id: Some(AgentId::parse("agent-1").expect("valid agent id")),
        agent_prompt_id: Some(test_agent_prompt_id("prompt-1")),
    });
    let mut encoded = Vec::new();
    ciborium::into_writer(&request, &mut encoded).expect("encode retry request");
    assert_eq!(
        ciborium::from_reader::<Event, _>(encoded.as_slice()).expect("decode retry request"),
        request
    );

    let result = Event::ProviderRetryPromptResultReported(ProviderRetryPromptResult {
        request_id: RetryPromptRequestId::parse("retry-1").expect("valid retry request id"),
        agent_prompt_id: test_agent_prompt_id("prompt-1"),
        status: RetryPromptStatus::Accepted,
    });
    let mut encoded = Vec::new();
    ciborium::into_writer(&result, &mut encoded).expect("encode retry result");
    assert_eq!(
        ciborium::from_reader::<Event, _>(encoded.as_slice()).expect("decode retry result"),
        result
    );

    let ui_result = Event::UiRetryPromptResult(UiRetryPromptResult {
        request_id: RetryPromptRequestId::parse("retry-1").expect("valid retry request id"),
        target_agent_id: Some(AgentId::parse("agent-1").expect("valid agent id")),
        target_label: "worker".into(),
        status: Some(RetryPromptStatus::Accepted),
        message: "Retrying agent worker now.".into(),
    });
    assert_eq!(ui_result.name(), EventName::UI_RETRY_PROMPT_RESULT);
    let mut encoded = Vec::new();
    ciborium::into_writer(&ui_result, &mut encoded).expect("encode UI retry result");
    assert_eq!(
        ciborium::from_reader::<Event, _>(encoded.as_slice()).expect("decode UI retry result"),
        ui_result
    );
}

/// Locks each retry-control stage into its intended directional wrapper so a
/// future caller cannot accidentally send a provider result as a UI request or
/// bypass the harness-owned requester delivery.
#[test]
fn retry_prompt_events_use_emit_and_deliver_directional_wrappers() {
    let request = HarnessInputMessage::emit(Event::UiRetryPrompt(UiRetryPrompt {
        request_id: RetryPromptRequestId::parse("retry-direction").expect("valid retry request id"),
        session_id: test_session_id("session-1"),
        target_agent_id: Some(agent_id("agent-1")),
        agent_prompt_id: None,
    }));
    let provider_result = HarnessInputMessage::emit_transient(
        Event::ProviderRetryPromptResultReported(ProviderRetryPromptResult {
            request_id: RetryPromptRequestId::parse("retry-direction")
                .expect("valid retry request id"),
            agent_prompt_id: test_agent_prompt_id("prompt-1"),
            status: RetryPromptStatus::NotParked,
        }),
    );
    let ui_result =
        HarnessOutputMessage::deliver(Event::UiRetryPromptResult(UiRetryPromptResult {
            request_id: RetryPromptRequestId::parse("retry-direction")
                .expect("valid retry request id"),
            target_agent_id: Some(agent_id("agent-1")),
            target_label: "worker".into(),
            status: Some(RetryPromptStatus::NotParked),
            message: "No delayed provider retry is waiting for agent worker.".into(),
        }));

    for input in [request, provider_result] {
        let bytes = encode_harness_input_to_vec(&input).expect("encode retry input");
        assert_eq!(
            decode_harness_input_from_slice(&bytes).expect("decode retry input"),
            input
        );
        assert!(decode_harness_output_from_slice(&bytes).is_err());
    }
    let bytes = encode_harness_output_to_vec(&ui_result).expect("encode retry delivery");
    assert_eq!(
        decode_harness_output_from_slice(&bytes).expect("decode retry delivery"),
        ui_result
    );
    assert!(decode_harness_input_from_slice(&bytes).is_err());
}

/// Ensures manual-compaction request ids remain bounded and safe for notices,
/// persistence keys, and provider-independent correlation.
#[test]
fn compaction_request_id_validation() {
    assert!(CompactionRequestId::parse("cr-agent_1-42").is_ok());
    assert!(CompactionRequestId::parse("").is_err());
    assert!(CompactionRequestId::parse("cr/unsafe").is_err());
    assert!(CompactionRequestId::parse("x".repeat(MAX_COMPACTION_REQUEST_ID_LEN + 1)).is_err());
}

/// Ensures the two durable pre-start facts retain every immutable authority and
/// correlation field across the CBOR wire format.
#[test]
fn manual_compaction_request_events_round_trip() {
    let requested = Event::AgentManualCompactionRequested(AgentManualCompactionRequested {
        request_id: CompactionRequestId::parse("cr-wire").expect("request id"),
        caller_agent_id: AgentId::parse("caller").expect("caller"),
        target_agent_id: AgentId::parse("target").expect("target"),
        initiating_agent_prompt_id: test_agent_prompt_id("ap-origin"),
        initiating_tool_call_id: "call-origin".into(),
        initiating_tool_name: ManualCompactionTool::AgentCompact,
        visible_tool_name: ToolName::new("compact_other"),
        requested_target_head: AgentHead::Root,
        target_generation: 7,
        model: "provider/model".into(),
        resume_inference: false,
    });
    let bytes = encode_message_to_vec(&requested).expect("encode request");
    assert_eq!(
        decode_message_from_slice::<Event>(&bytes).expect("decode request"),
        requested
    );

    let failed = Event::AgentManualCompactionRequestFailed(AgentManualCompactionRequestFailed {
        request_id: CompactionRequestId::parse("cr-wire").expect("request id"),
        target_agent_id: AgentId::parse("target").expect("target"),
        reason: ManualCompactionRequestFailureReason::RouteFailed,
    });
    let bytes = encode_message_to_vec(&failed).expect("encode failure");
    assert_eq!(
        decode_message_from_slice::<Event>(&bytes).expect("decode failure"),
        failed
    );
}

fn agent_id(value: &str) -> AgentId {
    AgentId::parse(value).expect("test agent id")
}

/// Ensures `ui.prompt_draft` remains readable from older peers that emitted
/// only session/text liveness pings before drafts carried an explicit viewed
/// agent target.
#[test]
fn ui_prompt_draft_target_agent_id_defaults_to_none() {
    let value = serde_json::json!({
        "session_id": "s1",
        "text": "draft"
    });

    let draft: UiPromptDraft = serde_json::from_value(value).expect("legacy prompt draft");

    assert_eq!(draft.session_id, test_session_id("s1"));
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text, "draft");
}

fn user_text_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

/// Ensures the optional Responses assistant-message replay sidecar is additive
/// for older persisted message items and round-trips when present.
#[test]
fn message_item_responses_raw_json_is_optional_and_round_trips() {
    let legacy = serde_json::json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "hello" }],
        "phase": "commentary"
    });
    let decoded: MessageItem = serde_json::from_value(legacy).expect("legacy message item");
    assert_eq!(decoded.responses_raw_json, None);

    let raw = r#"{"type":"message","id":"msg_raw","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]}"#;
    let message = MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: "hello".to_owned(),
        }],
        phase: Some(MessagePhase::Commentary),
        responses_raw_json: Some(raw.to_owned()),
    };
    let encoded = serde_json::to_value(&message).expect("encode message");
    let decoded: MessageItem = serde_json::from_value(encoded).expect("round-trip message");

    assert_eq!(decoded.responses_raw_json.as_deref(), Some(raw));
}

/// Ensures the optional tool-call replay sidecars are additive for old
/// persisted context items while still round-tripping when a provider parser
/// records them for replay/cache identity.
#[test]
fn tool_call_item_replay_sidecars_are_optional_and_round_trip() {
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    let call = ToolCallItem {
        call_id: "call-raw".into(),
        name: ToolName::new("shell"),
        tool_type: ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("z".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("a".to_owned()),
                CborValue::Array(vec![
                    CborValue::Integer(2.into()),
                    CborValue::Integer(3.into()),
                ]),
            ),
        ]),
        raw_arguments_json: Some(raw_arguments.to_owned()),
        responses_envelope: Some(ResponsesToolCallEnvelope {
            item_id: Some("fc_provider_item".to_owned()),
            status: Some("completed".to_owned()),
            extra_fields: Some(CborValue::Map(vec![(
                CborValue::Text("provider_future".to_owned()),
                CborValue::Text("kept".to_owned()),
            )])),
        }),
    };
    let mut legacy_value = serde_json::to_value(&call).expect("serialize legacy fixture");
    let legacy_object = legacy_value
        .as_object_mut()
        .expect("tool call serializes as object");
    legacy_object.remove("raw_arguments_json");
    legacy_object.remove("responses_envelope");
    let legacy_call: ToolCallItem =
        serde_json::from_value(legacy_value).expect("legacy tool call item");
    assert_eq!(legacy_call.raw_arguments_json, None);
    assert_eq!(legacy_call.responses_envelope, None);

    let round_trip: ToolCallItem =
        serde_json::from_value(serde_json::to_value(&call).expect("serialize tool call item"))
            .expect("deserialize tool call item");
    assert_eq!(
        round_trip.raw_arguments_json.as_deref(),
        Some(raw_arguments)
    );
    assert_eq!(round_trip.responses_envelope, call.responses_envelope);
}

/// Ensures opaque provider items keep their optional raw JSON replay sidecar
/// across the current serialized representation.
#[test]
fn opaque_provider_item_raw_json_is_optional_and_round_trips() {
    let raw_json = r#"{"type":"compaction","z":1.2300,"a":1e+03}"#;
    let value = CborValue::Map(vec![
        (
            CborValue::Text("type".to_owned()),
            CborValue::Text("compaction".to_owned()),
        ),
        (CborValue::Text("z".to_owned()), CborValue::Float(1.23)),
    ]);
    let item = OpaqueProviderItem::with_raw_json(value.clone(), raw_json);

    let round_trip: OpaqueProviderItem =
        serde_json::from_value(serde_json::to_value(&item).expect("serialize opaque item"))
            .expect("deserialize opaque item");
    assert_eq!(round_trip.value, value);
    assert_eq!(round_trip.raw_json.as_deref(), Some(raw_json));

    let serialized = serde_json::to_value(&item).expect("serialize opaque item");
    assert_eq!(serialized["tau_opaque_provider_item_version"], 0);
}

fn action_schema_fixture() -> ActionSchema {
    ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![ActionCommand {
            name: ":email".to_owned(),
            description: "Review email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![ActionCommand {
                name: "out".to_owned(),
                description: "Outgoing approvals".to_owned(),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "list".to_owned(),
                    description: "List queued outgoing email".to_owned(),
                    action_id: Some("email.out.list".to_owned()),
                    args: Vec::new(),
                    children: Vec::new(),
                }],
            }],
        }],
    }
}

fn representative_events() -> Vec<Event> {
    let mut events = vec![
        Event::ToolRegistrationDeclared(ToolRegistrationDeclared {
            tool: ToolSpec {
                name: ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo a payload".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        }),
        Event::ToolRegister(ToolRegister {
            publisher_extension_id: crate::ExtensionName::parse("tool-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            publisher_instance_id: ExtensionInstanceId::new(7),
            tool: echo_tool_spec(),
            tool_group: None,
            prompt_fragment: None,
        }),
        Event::ToolRequest(ToolRequest {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            arguments: CborValue::Text("hello".to_owned()),
            agent_id: agent_id("agent-1"),
            originator: PromptOriginator::User,
        }),
        Event::ToolUnregistrationDeclared(ToolUnregistrationDeclared {
            tool_name: ToolName::new("old_echo"),
        }),
        Event::ToolUnregister(ToolUnregister {
            publisher_extension_id: crate::ExtensionName::parse("tool-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            publisher_instance_id: ExtensionInstanceId::new(7),
            tool_name: ToolName::new("old_echo"),
        }),
        Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            agent_id: agent_id("agent-1"),
            originator: PromptOriginator::User,
        }),
        Event::ToolRejected(ToolRejected {
            call_id: "call-rejected".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "no provider".to_owned(),
            originator: PromptOriginator::User,
        }),
        Event::ToolResultReported(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("reported hello".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("hello".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolResultDisplay(ToolResultDisplay {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            kind: ToolResultKind::Final,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolErrorReported(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "reported failure".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "no live provider".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolBackgroundResult(ToolBackgroundResult {
            call_id: "call-bg".into(),
            tool_name: ToolName::new("slow_echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolBackgroundResultDisplay(ToolBackgroundResultDisplay {
            call_id: "call-bg".into(),
            tool_name: ToolName::new("slow_echo"),
            tool_type: ToolType::Function,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolBackgroundError(ToolBackgroundError {
            call_id: "call-bg-err".into(),
            tool_name: ToolName::new("slow_echo"),
            tool_type: ToolType::Function,
            message: "failed later".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgressReported(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("provider running".to_owned()),
            progress: None,
            display: None,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: Some(ProgressUpdate {
                current: Some(1),
                total: Some(10),
            }),
            display: None,
        }),
        Event::ToolCancelRequest(ToolCancelRequest {
            target_call_id: "call-1".into(),
        }),
        Event::ToolCancelledReported(ToolCancelled {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            tool_type: ToolType::Function,
        }),
        Event::ToolCancelled(ToolCancelled {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            tool_type: ToolType::Function,
        }),
        Event::ToolDelegateProgress(DelegateProgress {
            call_id: "delegate-call".into(),
            task_name: "review".to_owned(),
            agent_id: Some(agent_id("delegate_1")),
            role: Some("reviewer".to_owned()),
            ctx_percent: Some(10),
            ctx_input_tokens: Some(100),
            ctx_window: Some(1000),
            tools_in_flight: 0,
            tools_total: 1,
            display: None,
        }),
        Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: test_extension_name("std-email"),
            instance_id: 7.into(),
            schema: action_schema_fixture(),
        }),
        Event::ActionInvoke(ActionInvoke {
            invocation_id: ActionInvocationId::parse("act-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            extension_name: test_extension_name("std-email"),
            instance_id: 7.into(),
            action_id: "email.out.list".to_owned(),
            raw_line: ":email out list".to_owned(),
            argv: Vec::new(),
            arguments: CborValue::Map(Vec::new()),
        }),
        Event::ActionResult(ActionResult {
            invocation_id: ActionInvocationId::parse("act-1")
                .expect("test identifier must satisfy its grammar"),
            action_id: "email.out.list".to_owned(),
            output: ActionOutput::Text {
                text: "no queued mail".to_owned(),
            },
        }),
        Event::ActionError(ActionError {
            invocation_id: ActionInvocationId::parse("act-2")
                .expect("test identifier must satisfy its grammar"),
            action_id: "email.out.list".to_owned(),
            message: "approval queue unavailable".to_owned(),
            details: None,
        }),
        Event::MessageDelivered(MessageDelivered {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            message_id: MessageFactId::new("m1"),
            sender: MessageParty {
                stable_id: "u1".to_owned(),
                display_name: Some("Alice".to_owned()),
                sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
            },
            conversation: Some(MessageConversation {
                stable_id: "c1".to_owned(),
                display_name: Some("General".to_owned()),
                alias: Some("general".to_owned()),
            }),
            text: "hello".to_owned(),
            extension_data: MessageExtensionData::default(),
        }),
        Event::MessageEdited(MessageEdited {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            target: MessageFactRef {
                publisher_extension_id: RawMessagePublisherId::new("bridge-main"),
                message_id: MessageFactId::new("m1"),
            },
            actor: Some(MessageParty {
                stable_id: "u2".to_owned(),
                display_name: None,
                sender_auth: Some(MessageSenderAuth::VerifiedConversationAuthorized),
            }),
            conversation: None,
            text: "edited".to_owned(),
            extension_data: MessageExtensionData::default(),
        }),
        Event::MessageDeleted(MessageDeleted {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            target: MessageFactRef {
                publisher_extension_id: RawMessagePublisherId::new("other-bridge"),
                message_id: MessageFactId::new("future"),
            },
            actor: Some(MessageParty {
                stable_id: "u3".to_owned(),
                display_name: None,
                sender_auth: Some(MessageSenderAuth::TrustedMembership),
            }),
            conversation: None,
            extension_data: MessageExtensionData::default(),
        }),
        Event::MessageReactionAdded(MessageReactionAdded {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            target: MessageFactRef {
                publisher_extension_id: RawMessagePublisherId::new("bridge-main"),
                message_id: MessageFactId::new("m1"),
            },
            actor: None,
            conversation: None,
            reaction: "👍".to_owned(),
            extension_data: MessageExtensionData::default(),
        }),
        Event::MessageReactionRemoved(MessageReactionRemoved {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            target: MessageFactRef {
                publisher_extension_id: RawMessagePublisherId::new("bridge-main"),
                message_id: MessageFactId::new("m1"),
            },
            actor: None,
            conversation: None,
            reaction: "👍".to_owned(),
            extension_data: MessageExtensionData::default(),
        }),
        Event::MessageSent(MessageSent {
            publisher_extension_id: crate::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            agent_id: MessageAgentTarget::new("agent"),
            message_id: MessageFactId::new("m2"),
            recipient: None,
            conversation: None,
            text: "reply".to_owned(),
            extension_data: MessageExtensionData::new(CborValue::Text("opaque".to_owned()))
                .expect("bounded extension data"),
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "hello".to_owned(),
            agent_id: agent_id("agent"),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::AgentMessageSent(AgentMessageSent {
            message_id: AgentMessageId::parse("msg-1")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_abcd1234"),
            recipient: AgentMessageRecipient::User,
            kind: AgentMessageKind::Message,
            message: "hello".to_owned(),
        }),
        Event::AgentMessageReceived(AgentMessageReceived {
            message_id: AgentMessageId::parse("msg-2")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_abcd1234"),
            sender_session_id: None,
            recipient_id: agent_id("reviewer_efgh5678"),
            kind: AgentMessageKind::Message,
            watch_turn_state: None,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            message: "hello back".to_owned(),
        }),
        Event::AgentWatchesUpdated(AgentWatchesUpdated {
            session_id: test_session_id("session_123"),
            watcher_id: agent_id("engineer_parent"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: AgentWatchUpdateCause::AgentWatchEnable,
        }),
        Event::AgentStatsUpdated(AgentStatsUpdated {
            session_id: test_session_id("session_123"),
            agent_id: agent_id("engineer_child"),
            navigation_mode: AgentNavigationMode::Active,
            runtime_state: AgentRuntimeState::Running,
            tools: AgentToolStats {
                in_flight: 1,
                started_total: 1,
            },
            context: AgentContextStats::default(),
            estimated_api_cost: Default::default(),
        }),
        Event::SessionStarted(SessionStarted {
            session_id: test_session_id("s1"),
            reason: SessionStartReason::Initial,
        }),
        Event::SessionAgentLoaded(SessionAgentLoaded {
            agent_initialization_id: AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_abcd1234"),
            ephemeral: false,
        }),
        Event::SessionShutdown(SessionShutdown {
            session_id: test_session_id("s1"),
        }),
        Event::SessionAgentUnloaded(SessionAgentUnloaded {
            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_abcd1234"),
        }),
        Event::SessionReplayComplete(SessionReplayComplete {
            session_id: test_session_id("s1"),
            error: None,
        }),
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("engineer_abcd1234"),
            text: "hello".to_owned(),
            message_class: PromptMessageClass::User,
            internal_kind: None,
            originator: PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: agent_id("engineer_abcd1234"),
            text: "queued".to_owned(),
            message_class: PromptMessageClass::User,
        }),
        Event::AgentPromptRecalled(AgentPromptRecalled {
            agent_id: agent_id("engineer_abcd1234"),
            text: "queued".to_owned(),
        }),
        Event::AgentPromptSteered(AgentPromptSteered {
            inference_activation: false,
            submission_source: PromptSubmissionSource::HarnessInternal,
            agent_id: agent_id("engineer_abcd1234"),
            text: "steer".to_owned(),
            message_class: PromptMessageClass::User,
            internal_kind: None,
            ctx_id: None,
        }),
        Event::AgentCompactionTriggered(AgentCompactionTriggered {
            agent_id: agent_id("engineer_abcd1234"),
            originator: PromptOriginator::User,
            resume_inference: false,
        }),
        Event::AgentCompacted(AgentCompacted {
            compact_prompt_id: None,
            model: None,
            operation: None,
            agent_id: agent_id("engineer_abcd1234"),
            transaction_id: None,
            cut: None,
            suffix_end: None,
            replacement_window: vec![user_text_item("summary")],
        }),
        Event::AgentStandaloneCompactionStarted(AgentStandaloneCompactionStarted {
            compact_prompt_id: test_agent_prompt_id("ap-legacy-default"),
            operation: PromptOperation::StandaloneCompaction,
            agent_id: agent_id("engineer_abcd1234"),
            transaction_id: CompactionTransactionId::parse("ct-1").expect("transaction id"),
            cut: AgentHead::Root,
            resume_through: Some(AgentHead::Node(NodeId::new(1))),
            model: ModelId::from("provider/model"),
            originator: PromptOriginator::User,
            supersedes: None,
            trigger: StandaloneCompactionTrigger::Manual,
        }),
        Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
            agent_id: agent_id("engineer_abcd1234"),
            transaction_id: CompactionTransactionId::parse("ct-1").expect("transaction id"),
            cut: AgentHead::Root,
            reason: StandaloneCompactionFailureReason::InvalidWindow,
            resume_through: Some(AgentHead::Node(NodeId::new(1))),
        }),
        Event::AgentInferenceDispatchStarted(AgentInferenceDispatchStarted {
            agent_id: agent_id("engineer_abcd1234"),
            transaction_id: Some(CompactionTransactionId::parse("ct-1").expect("transaction id")),
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            through: AgentHead::Node(NodeId::new(1)),
            model: None,
            operation: None,
            activation_cut: None,
        }),
        Event::AgentPromptCreated(AgentPromptCreated {
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            agent_id: agent_id("engineer_abcd1234"),
            session_id: test_session_id("session_123"),
            system_prompt: "You are helpful.".to_owned(),
            context: PromptContext {
                blocks: vec![ContextBlock::UserInput(UserInputBlock {
                    items: vec![user_text_item("hello")],
                })],
            },
            tools: vec![ToolDefinition {
                name: ToolName::new("read"),
                model_visible_name: None,
                description: Some("Read a file".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
            }],
            tools_ref: None,
            model: "test/model".parse().expect("model id"),
            model_params: ModelParams::default(),
            tool_choice: ToolChoice::default(),
            originator: PromptOriginator::User,
            ctx_id: None,
            compaction: None,
            share_user_cache_key: false,
            operation: PromptOperation::Inference,
        }),
        Event::AgentPromptStarted(AgentPromptStarted {
            model_params: Some(ModelParams::default()),
            outer_turn_id: None,

            agent_prompt_id: test_agent_prompt_id("sp-1"),
            agent_id: agent_id("engineer_abcd1234"),
            session_id: test_session_id("session_123"),
            model: "test/model".parse().expect("model id"),
            operation: PromptOperation::Inference,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::AgentPromptTerminated(AgentPromptTerminated {
            agent_id: agent_id("engineer_abcd1234"),
            agent_prompt_id: test_agent_prompt_id("sp-stale"),
            reason: AgentPromptTerminationReason::Stale,
            originator: PromptOriginator::User,
        }),
        Event::AgentPromptPrewarmRequested(AgentPromptPrewarmRequested {
            agent_id: agent_id("engineer_abcd1234"),
            session_id: test_session_id("s1"),
            system_prompt: "You are helpful.".to_owned(),
            context: PromptContext { blocks: Vec::new() },
            tools: Vec::new(),
            model: Some("openai/gpt-4.1".parse().expect("model id")),
            model_params: ModelParams::default(),
            tool_choice: ToolChoice::Auto,
            originator: PromptOriginator::User,
            share_user_cache_key: false,
        }),
        Event::AgentUserMessageInjected(AgentUserMessageInjected {
            inference_activation: false,
            agent_id: agent_id("engineer_abcd1234"),
            text: "injected".to_owned(),
            message_class: PromptMessageClass::Internal,
        }),
        Event::AgentHeadMoved(AgentHeadMoved {
            agent_id: agent_id("engineer_abcd1234"),
            head: AgentHead::Root,
        }),
        Event::AgentStarted(AgentStarted {
            creator: Some(AgentCreator::default()),

            agent_id: agent_id("engineer_abcd1234"),
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: Some("Main".to_owned()),
            metadata: Vec::new(),
            ephemeral: false,
        }),
        Event::AgentDisplayNameSet(AgentDisplayNameSet {
            agent_id: agent_id("engineer_abcd1234"),
            display_name: "Main".to_owned(),
        }),
        Event::AgentMetadataSet(AgentMetadataSet {
            agent_id: agent_id("engineer_abcd1234"),
            key: "cwd".into(),
            value: CborValue::Text("/tmp".to_owned()),
            inheritable: true,
            mutation_id: None,
        }),
        Event::AgentMetadataUnset(AgentMetadataUnset {
            agent_id: agent_id("engineer_abcd1234"),
            key: "cwd".into(),
        }),
        Event::AgentMetadataSetRequest(AgentMetadataSet {
            agent_id: agent_id("engineer_abcd1234"),
            key: "cwd".into(),
            value: CborValue::Text("/tmp".to_owned()),
            inheritable: true,
            mutation_id: None,
        }),
        Event::AgentMetadataUnsetRequest(AgentMetadataUnset {
            agent_id: agent_id("engineer_abcd1234"),
            key: "cwd".into(),
        }),
        Event::AgentReplayComplete(AgentReplayComplete {
            agent_id: agent_id("engineer_abcd1234"),
            session_id: Some(test_session_id("s1")),
            error: None,
        }),
        Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            originator: PromptOriginator::User,
        }),
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            agent_id: agent_id("engineer_abcd1234"),
            deltas: vec![ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "Hi".to_owned(),
                phase: None,
            }],
            compaction: None,
            status: None,
            response_stats: None,
            originator: PromptOriginator::User,
        }),
        Event::ProviderResponseFinished(ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("sp-1"),
            agent_id: agent_id("engineer_abcd1234"),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "Hi there".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: ContextRecoveryDisposition::None,
            usage: None,
            originator: PromptOriginator::User,

            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
        Event::ProviderCacheMissDiagnostic(ProviderCacheMissDiagnostic {
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            model: "openai/gpt-4.1".parse().expect("model id"),
            originator: PromptOriginator::User,
            tool_choice: ToolChoice::Auto,
            ws_pool_delta: None,
            input_tokens: 1000,
            cached_tokens: 100,
            previous_input_tokens: 900,
            cacheable_input_tokens: 800,
            corrected_cache_efficiency: 0.125,
        }),
        Event::ExtensionStarting(ExtensionStarting {
            instance_id: 1.into(),
            extension_name: test_extension_name("shell"),
            pid: Some(1234),
        }),
        Event::ExtensionReady(ExtensionReady {
            instance_id: 1.into(),
            extension_name: test_extension_name("shell"),
            pid: Some(1234),
        }),
        Event::ExtensionExited(ExtensionExited {
            instance_id: 1.into(),
            extension_name: test_extension_name("shell"),
            pid: Some(1234),
            exit_code: Some(0),
            signal: None,
        }),
        Event::ExtensionRestarting(ExtensionRestarting {
            instance_id: 1.into(),
            extension_name: test_extension_name("shell"),
            pid: Some(1234),
            attempt: 2,
            reason: Some("hot reload".to_owned()),
        }),
        Event::ExtensionSessionDiscoverySnapshotDeclared(
            ExtensionSessionDiscoverySnapshotDeclared {
                session_id: test_session_id("s1"),
                skills: vec![DiscoverySkillCandidate {
                    name: "brave-search".into(),
                    description: "Web search via Brave API".to_owned(),
                    file_path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
                    add_to_prompt: true,
                    user_invocable: true,
                    disable_model_invocation: false,
                    argument_hint: Some("<query>".to_owned()),
                    sampled_modified: Some(DiscoveryModifiedMicros::new(123_000_000)),
                }],
                agents_files: vec![DiscoveryAgentsFile {
                    file_path: "/home/user/src/project/AGENTS.md".into(),
                    content: "# Project instructions\n- Run tests".to_owned(),
                }],
            },
        ),
        Event::ExtensionAgentDiscoverySnapshotDeclared(ExtensionAgentDiscoverySnapshotDeclared {
            session_id: test_session_id("s1"),
            agent_id: agent_id("agent-1"),
            agent_initialization_id: AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
            skills: vec![DiscoverySkillCandidate {
                name: "local-search".into(),
                description: "Search locally".to_owned(),
                file_path: "/project/.agents/skills/local-search/SKILL.md".into(),
                add_to_prompt: false,
                user_invocable: true,
                disable_model_invocation: true,
                argument_hint: None,
                sampled_modified: Some(DiscoveryModifiedMicros::new(-1)),
            }],
            agents_files: vec![DiscoveryAgentsFile {
                file_path: "/project/AGENTS.md".into(),
                content: "Run local checks.".to_owned(),
            }],
        }),
        Event::ExtensionContextProviderRegister(ExtensionContextProviderRegister {}),
        Event::ExtensionSessionContextProviderRegister(ExtensionSessionContextProviderRegister {}),
        Event::ExtensionContextReady(ExtensionContextReady {
            agent_initialization_id: AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id("agent-1"),
        }),
        Event::ExtensionSessionContextReady(ExtensionSessionContextReady {
            session_id: test_session_id("s1"),
        }),
        Event::ExtAgentContextPublish(ExtAgentContextPublish {
            session_id: test_session_id("test-session"),
            agent_initialization_id: AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            agent_id: agent_id("agent-1"),
            key: "cwd".into(),
            value: AgentContextValue(serde_json::json!("/tmp/project")),
        }),
        Event::ExtPromptFragmentPublish(ExtPromptFragmentPublish {
            fragment: PromptFragment::new(
                "style-guide",
                PromptPriority::new(100),
                PromptContent::new("Be concise."),
            ),
        }),
        Event::ExtInternalPromptSubmitRequest(ExtInternalPromptSubmitRequest {
            agent_id: agent_id("agent-1"),
            text: "internal extension prompt".to_owned(),
            ctx_id: Some("ctx-1".to_owned()),
            activation_kind: None,
        }),
        Event::StartAgentRequest(StartAgentRequest {
            query_id: "query-1".to_owned(),
            instruction: "check this".to_owned(),
            role: Some("reviewer".to_owned()),
            input_stats: ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: Some("review".to_owned()),
            parent_agent: Some(agent_id("agent-1")),
        }),
        Event::StartAgentAccepted(StartAgentAccepted {
            query_id: "query-1".to_owned(),
            agent_id: agent_id("delegate_1"),
        }),
        Event::StartAgentResult(StartAgentResult {
            query_id: "query-1".to_owned(),
            text: "looks good".to_owned(),
            error: None,
        }),
        Event::AgentInitializationContextSet(AgentInitializationContextSet {
            session_id: test_session_id("s1"),
            agent_id: agent_id("agent-1"),
            agent_initialization_id: AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
            agents_message: Some("project instructions".to_owned()),
            effective_skills: vec![
                DiscoveryEffectiveSkill {
                    name: "brave-search".into(),
                    description: "Web search via Brave API".to_owned(),
                    source: DiscoveryEffectiveSkillSource::File {
                        path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
                    },
                    add_to_prompt: true,
                    user_invocable: true,
                    disable_model_invocation: false,
                    argument_hint: Some("<query>".to_owned()),
                },
                DiscoveryEffectiveSkill {
                    name: "tau-self-knowledge".into(),
                    description: "Explain Tau".to_owned(),
                    source: DiscoveryEffectiveSkillSource::BuiltIn,
                    add_to_prompt: true,
                    user_invocable: true,
                    disable_model_invocation: false,
                    argument_hint: None,
                },
            ],
            agents_files: vec![DiscoveryAgentsFileSummary {
                file_path: "/home/user/src/project/AGENTS.md".into(),
                lines: 2,
                bytes: 32,
            }],
        }),
        Event::ExtensionEvent(
            CustomEvent::try_new(
                "demo.progress".parse().expect("event name"),
                Some(test_session_id("s1")),
                CborValue::Text("working".to_owned()),
            )
            .expect("valid custom event"),
        ),
        Event::ProviderModelsDeclared(ProviderModelsDeclared { models: Vec::new() }),
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            publisher_extension_id: crate::ExtensionName::parse("provider")
                .expect("test extension name must satisfy the identifier grammar"),
            models: vec![ProviderModelInfo {
                id: "openai/gpt-4.1".parse().expect("model id"),
                display_name: Some("GPT-4.1".to_owned()),
                tags: Vec::new(),
                supported_tool_types: vec![],
                input_modalities: Vec::new(),
                tool_result_modalities: Vec::new(),
                supports_parallel_tool_calls: true,
                default_affinity: 0,
                context_window: 128_000,
                efforts: vec![Effort::Off, Effort::Low, Effort::Medium, Effort::High],
                verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: false,
                standalone_compaction_threshold: None,
                est_uncached_input_cost_1m_usd: Default::default(),
                est_cached_input_cost_1m_usd: Default::default(),
                est_output_cost_1m_usd: Default::default(),
            }],
        }),
        Event::HarnessAgentContextInitialized(HarnessAgentContextInitialized {
            session_id: test_session_id("s1"),
            agent_id: agent_id("agent-1"),
            agent_initialization_id: AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
            listed_skills: vec![DiscoveryEffectiveSkill {
                name: "tau-self-knowledge".into(),
                description: "Explain Tau".to_owned(),
                source: DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            }],
            agents_files: vec![DiscoveryAgentsFileSummary {
                file_path: "/project/AGENTS.md".into(),
                lines: 1,
                bytes: 17,
            }],
        }),
        Event::HarnessSessionSkillsAvailable(HarnessSessionSkillsAvailable {
            session_id: test_session_id("s1"),
            skills: vec![DiscoveryEffectiveSkill {
                name: "brave-search".into(),
                description: "Web search via Brave API".to_owned(),
                source: DiscoveryEffectiveSkillSource::File {
                    path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
                },
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: Some("<query>".to_owned()),
            }],
        }),
        Event::ProviderQuotaReplaceReported(ProviderQuotaReplace {
            provider: ProviderName::new("chatgpt"),
            profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 1,
            establishes_new_epoch: true,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        }),
        Event::ProviderQuotaPatchReported(ProviderQuotaPatch {
            provider: ProviderName::new("chatgpt"),
            profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 2,
            windows: Vec::new(),
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        }),
        Event::ProviderQuotaClearReported(ProviderQuotaClear {
            provider: ProviderName::new("chatgpt"),
            profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 3,
        }),
        Event::ProviderToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("provider-visible completion".to_owned()),
            provider_content: Vec::new(),
            kind: ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ProviderToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "provider-visible failure".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::HarnessNotice(HarnessNotice::new(
            notice_kind::HARNESS_NOTICE,
            "ready",
            NoticeLevel::Info,
        )),
        Event::HarnessSessionDir(HarnessSessionDir {
            session_id: test_session_id("s1"),
            path: "/tmp/tau/session".into(),
            status: SessionDirStatus::New,
        }),
        Event::HarnessUiDir(HarnessUiDir {
            path: "/tmp/tau/ui".into(),
        }),
        Event::HarnessModelsAvailable(HarnessModelsAvailable {
            models: vec!["openai/gpt-4.1".parse().expect("model id")],
        }),
        Event::HarnessRolesAvailable(HarnessRolesAvailable {
            roles: vec![HarnessRoleInfo {
                name: "engineer".to_owned(),
                description: "Engineer".to_owned(),
                role_description: Some("Writes code".to_owned()),
                details: Some(HarnessRoleDetails {
                    model: Some("openai/gpt-4.1".parse().expect("model id")),
                    ..Default::default()
                }),
            }],
            groups: vec![HarnessRoleGroup {
                name: "default".to_owned(),
                roles: vec!["engineer".to_owned()],
            }],
            custom_prompts: vec![HarnessCustomPrompt {
                id: "summarize".to_owned(),
                text: "Summarize this".to_owned(),
            }],
        }),
        Event::HarnessRoleSelected(HarnessRoleSelected {
            role: "engineer".to_owned(),
            model: Some("openai/gpt-4.1".parse().expect("model id")),
            context_window: Some(128_000),
            baseline_params: Some(ModelParams::default()),
            model_params: ModelParams::default(),
        }),
        Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
            input_tokens: Some(100),
            cached_tokens: Some(20),
            percent_used: Some(1),
        }),
        Event::HarnessProviderQuotaChanged(HarnessProviderQuotaChanged {
            provider: ProviderName::new("chatgpt"),
            profile_epoch: ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 1,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        }),
        Event::HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged {
            agent_id: agent_id("agent-1"),
            input_tokens: Some(100),
            cached_tokens: Some(20),
            context_window: Some(128_000),
            percent_used: Some(1),
        }),
        Event::AgentState(AgentStateChanged {
            agent_id: agent_id("agent-1"),
            state: AgentRuntimeState::Running,
        }),
        Event::HarnessEffortsAvailable(HarnessEffortsAvailable {
            levels: vec![Effort::Off, Effort::Low],
        }),
        Event::HarnessVerbositiesAvailable(HarnessVerbositiesAvailable {
            levels: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
        }),
        Event::HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable {
            levels: vec![
                ThinkingSummary::Off,
                ThinkingSummary::Auto,
                ThinkingSummary::Concise,
                ThinkingSummary::Detailed,
            ],
        }),
        Event::UiRoleSelect(UiRoleSelect {
            role: "engineer".to_owned(),
        }),
        Event::UiAgentModelSelect(UiAgentModelSelect {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
            model: "openai/gpt-4.1".parse().expect("model id"),
        }),
        Event::UiRoleUpdate(UiRoleUpdate {
            role: "engineer".to_owned(),
            action: UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
            },
        }),
        Event::UiShellCommand(UiShellCommand {
            session_id: test_session_id("s1"),
            command_id: ShellCommandId::parse("shell-1")
                .expect("test identifier must satisfy its grammar"),
            command: "pwd".to_owned(),
            include_in_context: true,
            target_agent_id: Some(agent_id("agent-1")),
        }),
        Event::UiSwitchSession(UiSwitchSession {
            new_session_id: test_session_id("s2"),
            reason: SessionStartReason::New,
        }),
        Event::UiCreateAgent(UiCreateAgent {
            literal: false,
            session_id: test_session_id("s1"),
            role: "engineer".to_owned(),
            model_override: Some("openai/gpt-4.1".parse().expect("model id")),
            metadata: vec![AgentInitialMetadata {
                key: "cwd".into(),
                value: CborValue::Text("/tmp".to_owned()),
                inheritable: true,
            }],
            initial_prompt: Some("hello".to_owned()),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: Some("ctx-create".to_owned()),
            parent_agent: Some(agent_id("parent_1")),
            ephemeral: false,
        }),
        Event::UiNavigateTree(UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
            target: UiTreeNavigationTarget::Root,
        }),
        Event::UiCompactRequest(UiCompactRequest {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
        }),
        Event::UiPromptDraft(UiPromptDraft {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
            text: "draft".to_owned(),
        }),
        Event::UiFocusChanged(UiFocusChanged {
            session_id: test_session_id("s1"),
            focused: true,
        }),
        Event::UiCancelPrompt(UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
            agent_prompt_id: Some(test_agent_prompt_id("sp-1")),
        }),
        Event::UiRecallQueuedPrompt(UiRecallQueuedPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
        }),
        Event::UiSetAgentDisplayName(UiSetAgentDisplayName {
            session_id: test_session_id("s1"),
            agent_id: agent_id("agent-1"),
            display_name: "Main".to_owned(),
        }),
        Event::Osc1337SetUserVar(Osc1337SetUserVar {
            name: "tau_status".to_owned(),
            value: "ready".to_owned(),
        }),
        Event::TermBell(TermBell {}),
        Event::ShellCommandProgress(ShellCommandProgress {
            command_id: ShellCommandId::parse("shell-1")
                .expect("test identifier must satisfy its grammar"),
            stream: ShellStream::Stdout,
            chunk: "/tmp\n".to_owned(),
            target_agent_id: Some(agent_id("agent-1")),
        }),
        Event::ShellCommandFinished(ShellCommandFinished {
            command_id: ShellCommandId::parse("shell-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "pwd".to_owned(),
            include_in_context: true,
            target_agent_id: Some(agent_id("agent-1")),
            output: "/tmp\n".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        }),
        Event::ShellCommandProgressReported(ShellCommandProgress {
            command_id: ShellCommandId::parse("shell-report-1")
                .expect("test identifier must satisfy its grammar"),
            stream: ShellStream::Stderr,
            chunk: "working\n".to_owned(),
            target_agent_id: Some(agent_id("agent-1")),
        }),
        Event::ShellCommandFinishedReported(ShellCommandFinished {
            command_id: ShellCommandId::parse("shell-report-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "pwd".to_owned(),
            include_in_context: true,
            target_agent_id: Some(agent_id("agent-1")),
            output: "/tmp\n".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        }),
    ];
    let reports = events
        .iter()
        .filter_map(|event| match event.clone() {
            Event::MessageDelivered(value) => Some(Event::MessageDeliveredReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::MessageEdited(value) => Some(Event::MessageEditedReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::MessageDeleted(value) => Some(Event::MessageDeletedReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::MessageReactionAdded(value) => Some(Event::MessageReactionAddedReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::MessageReactionRemoved(value) => Some(Event::MessageReactionRemovedReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::MessageSent(value) => Some(Event::MessageSentReported(
                value.with_publisher(RawMessagePublisherId::new("raw-claim")),
            )),
            Event::ProviderPromptSubmitted(value) => {
                Some(Event::ProviderPromptSubmittedReported(value))
            }
            Event::ProviderResponseUpdated(value) => {
                Some(Event::ProviderResponseUpdatedReported(value))
            }
            Event::ProviderResponseFinished(value) => {
                Some(Event::ProviderResponseFinishedReported(value))
            }
            Event::ProviderCacheMissDiagnostic(value) => {
                Some(Event::ProviderCacheMissDiagnosticReported(value))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    events.extend(reports);
    events.push(Event::ProviderRetryPromptResultReported(
        ProviderRetryPromptResult {
            request_id: RetryPromptRequestId::parse("retry-representative").expect("retry id"),
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            status: RetryPromptStatus::Accepted,
        },
    ));
    events
}

fn sample_session_started() -> Event {
    Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    })
}

fn representative_input_messages() -> Vec<HarnessInputMessage> {
    vec![
        HarnessInputMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: test_extension_name("provider"),
            client_kind: ClientKind::Provider,
            capabilities: Default::default(),
        }),
        HarnessInputMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
        }),
        HarnessInputMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority::new(0),
        }),
        HarnessInputMessage::Ready(Ready {
            message: Some("ready".to_owned()),
        }),
        HarnessInputMessage::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        HarnessInputMessage::ConfigError(ConfigError {
            message: "bad config".to_owned(),
        }),
        HarnessInputMessage::ExtensionNoticeRequest(ExtensionNoticeRequest {
            message: "provider connected".to_owned(),
            level: NoticeLevel::Info,
        }),
        HarnessInputMessage::Emit(Emit {
            event: Box::new(Event::ExtensionEvent(
                CustomEvent::try_new(
                    "demo.persist_progress".parse().expect("event name"),
                    Some(test_session_id("s1")),
                    CborValue::Text("working".to_owned()),
                )
                .expect("valid custom event"),
            )),
            persist: false,
        }),
        HarnessInputMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        }),
        HarnessInputMessage::GetAgentPromptCreated(GetAgentPromptCreated {
            request_id: "prompt-1".to_owned(),
            session_id: test_session_id("s1"),
            agent_prompt_id: test_agent_prompt_id("sp-1"),
        }),
        HarnessInputMessage::GetRenderedSystemPrompt(GetRenderedSystemPrompt {
            request_id: "render-system-prompt-1".to_owned(),
            role: "engineer".to_owned(),
        }),
        HarnessInputMessage::GetRenderedPrompt(GetRenderedPrompt {
            request_id: "render-prompt-1".to_owned(),
            role: "engineer".to_owned(),
            enable_agents_md: true,
        }),
        HarnessInputMessage::GetRenderedToolDefinitions(GetRenderedToolDefinitions {
            request_id: "render-tools-1".to_owned(),
            role: "engineer".to_owned(),
        }),
        HarnessInputMessage::GetCurrentSession(GetCurrentSession {
            request_id: "current-session-1".to_owned(),
        }),
        HarnessInputMessage::GetSessionAgentList(GetSessionAgentList {
            request_id: "agent-list-1".to_owned(),
            session_id: test_session_id("s1"),
            scope: SessionAgentListScope::History,
        }),
        HarnessInputMessage::UiDetachRequest(UiDetachRequest {}),
        HarnessInputMessage::UiTreeRequest(UiTreeRequest {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
        }),
        HarnessInputMessage::ExtensionDataRequest(ExtensionDataRequest {
            request_id: "ext-data-1".to_owned(),
            scope: ExtensionDataScope::Session,
            op: ExtensionDataRequestOp::ReadFile {
                path: ExtensionDataPath::new("notes/state.cbor"),
            },
        }),
        HarnessInputMessage::ExternalAgentMessage(ExternalAgentMessageRequest {
            request_id: "external-1".to_owned(),
            message_id: AgentMessageId::parse("msg-external-1")
                .expect("test identifier must satisfy its grammar"),
            capability: "capability-1".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: agent_id("sender_agent"),
            recipient_session_id: test_session_id("recipient-session"),
            recipient: ExternalAgentMessageRecipient::Exact(agent_id("recipient_agent")),
            kind: AgentMessageKind::Message,
            message: "hello external".to_owned(),
        }),
        HarnessInputMessage::ExternalAgentMessageAuth(ExternalAgentMessageAuthRequest {
            request_id: "external-auth-1".to_owned(),
            message_id: AgentMessageId::parse("msg-external-1")
                .expect("test identifier must satisfy its grammar"),
            capability: "capability-1".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: agent_id("sender_agent"),
            recipient_session_id: test_session_id("recipient-session"),
            recipient: ExternalAgentMessageRecipient::Exact(agent_id("recipient_agent")),
            kind: AgentMessageKind::Message,
            message: "hello external".to_owned(),
        }),
    ]
}

fn representative_output_messages() -> Vec<HarnessOutputMessage> {
    vec![
        HarnessOutputMessage::Configure(Configure {
            instance_name: crate::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            tool_prefix: None,
            config: CborValue::Null,
            state_dir: Some(std::path::PathBuf::from("/tmp/tau/state/ext/demo")),
            secrets: std::collections::BTreeMap::new(),
        }),
        HarnessOutputMessage::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        HarnessOutputMessage::Deliver(EventDelivery::live(
            UnixMicros::new(1_700_000_000_000_000),
            sample_session_started(),
        )),
        HarnessOutputMessage::Deliver(EventDelivery::replay(
            UnixMicros::new(1_700_000_000_000_000),
            sample_session_started(),
        )),
        HarnessOutputMessage::Deliver(EventDelivery::direct(Event::ExtensionEvent(
            CustomEvent::try_new(
                "demo.snapshot".parse().expect("event name"),
                Some(test_session_id("s1")),
                CborValue::Text("snapshot".to_owned()),
            )
            .expect("valid custom event"),
        ))),
        HarnessOutputMessage::InterceptRequest(InterceptRequest {
            event: Box::new(sample_session_started()),
            persist: true,
        }),
        HarnessOutputMessage::AgentPromptCreatedResult(Box::new(AgentPromptCreatedResult {
            request_id: "prompt-1".to_owned(),
            prompt: None,
        })),
        HarnessOutputMessage::RenderedSystemPromptResult(Box::new(RenderedSystemPromptResult {
            request_id: "render-system-prompt-1".to_owned(),
            prompt: Some("You are helpful.".to_owned()),
            error: None,
        })),
        HarnessOutputMessage::RenderedPromptResult(Box::new(RenderedPromptResult {
            request_id: "render-prompt-1".to_owned(),
            prompt: Some("<message role=\"system\">\nYou are helpful.\n</message>\n".to_owned()),
            error: None,
        })),
        HarnessOutputMessage::RenderedToolDefinitionsResult(Box::new(
            RenderedToolDefinitionsResult {
                request_id: "render-tools-1".to_owned(),
                tools: Some(vec![ToolDefinition {
                    name: ToolName::new("read"),
                    model_visible_name: None,
                    description: Some("Read a file".to_owned()),
                    tool_type: ToolType::Function,
                    parameters: Some(serde_json::json!({"type": "object"})),
                    format: None,
                }]),
                error: None,
            },
        )),
        HarnessOutputMessage::CurrentSessionResult(CurrentSessionResult {
            request_id: "current-session-1".to_owned(),
            session_id: test_session_id("s1"),
            project_root: std::path::PathBuf::from("/work/project"),
        }),
        HarnessOutputMessage::SessionAgentListResult(Box::new(SessionAgentListResult {
            request_id: "agent-list-1".to_owned(),
            session_id: test_session_id("s1"),
            result: SessionAgentListResultPayload::Ok {
                agents: vec![SessionAgentListEntry {
                    agent_id: agent_id("agent-1"),
                    lifecycle: SessionAgentLifecycle::Live {
                        runtime_state: AgentRuntimeState::Idle,
                        navigation_mode: AgentNavigationMode::Active,
                    },
                    persistence: SessionAgentPersistence::Durable,
                    facts: SessionAgentFacts::Available {
                        started_at: Some(UnixMicros::new(1_700_000_000_000_000)),
                        parent_agent: None,
                        role: "engineer".to_owned(),
                        display_name: Some("Agent one".to_owned()),
                    },
                    work_status: Some(SessionAgentWorkStatus {
                        phase: AgentWorkStatusPhase::Working,
                        title: Some("testing roster status".to_owned()),
                    }),
                }],
            },
        })),
        HarnessOutputMessage::ExtensionDataResult(Box::new(ExtensionDataResult {
            request_id: "ext-data-1".to_owned(),
            result: ExtensionDataResultPayload::Ok {
                value: ExtensionDataValue::ListFiles {
                    entries: vec![ExtensionDataEntry {
                        path: ExtensionDataPath::new("notes/state.cbor"),
                        is_dir: false,
                        len: Some(3),
                    }],
                },
            },
        })),
        HarnessOutputMessage::ExternalAgentMessageResult(ExternalAgentMessageResult {
            request_id: "external-1".to_owned(),
            error: None,
            recipient_id: Some(agent_id("recipient_agent")),
            started: false,
        }),
        HarnessOutputMessage::ExternalAgentMessageAuthResult(ExternalAgentMessageAuthResult {
            request_id: "external-auth-1".to_owned(),
            authorized: true,
            error: None,
        }),
    ]
}

/// Ensures parsed event names preserve category/call structure and display back
/// to the dotted wire name.
#[test]
fn event_name_round_trips_from_string() {
    for event in representative_events() {
        let name = event.name();
        let serialized = name.to_string();
        assert_eq!(serialized.parse::<EventName>(), Ok(name));
    }
}

/// All six message facts remain distinct durable events, preserve universal and
/// opaque fields through both codecs, and require explicit opaque data on wire.
#[test]
fn message_fact_events_have_distinct_required_v11_wire_shapes() {
    let facts = representative_events()
        .into_iter()
        .filter(|event| event.message_agent_target().is_some())
        .collect::<Vec<_>>();
    assert_eq!(facts.len(), 6);
    for fact in facts {
        assert!(fact.defaults_to_persist());
        let json = serde_json::to_value(&fact).expect("serialize fact to JSON");
        assert!(
            json["payload"].get("extension_data").is_some(),
            "opaque data is required for {}",
            fact.name()
        );
        let decoded: Event = serde_json::from_value(json.clone()).expect("decode fact from JSON");
        assert_eq!(decoded, fact);
        if matches!(fact, Event::MessageDelivered(_)) {
            let mut missing_optional = json.clone();
            let payload = missing_optional
                .get_mut("payload")
                .and_then(serde_json::Value::as_object_mut)
                .expect("delivered payload");
            payload
                .get_mut("sender")
                .and_then(serde_json::Value::as_object_mut)
                .expect("sender")
                .remove("sender_auth");
            payload
                .get_mut("conversation")
                .and_then(serde_json::Value::as_object_mut)
                .expect("conversation")
                .remove("alias");
            let decoded: Event =
                serde_json::from_value(missing_optional).expect("optional prompt metadata");
            let Event::MessageDelivered(decoded) = decoded else {
                panic!("delivered fact");
            };
            assert_eq!(decoded.sender.sender_auth, None);
            assert_eq!(
                decoded
                    .conversation
                    .and_then(|conversation| conversation.alias),
                None
            );
        }
        let encoded = encode_message_to_vec(&HarnessInputMessage::emit(fact.clone()))
            .expect("encode fact frame");
        assert_eq!(
            decode_harness_input_from_slice(&encoded).expect("decode fact frame"),
            HarnessInputMessage::emit(fact)
        );

        let mut missing = json;
        missing
            .get_mut("payload")
            .and_then(serde_json::Value::as_object_mut)
            .expect("fact payload")
            .remove("extension_data");
        assert!(serde_json::from_value::<Event>(missing).is_err());
    }
}

/// Message bridges publish transient report names while the harness alone
/// converts those reports into the existing canonical fact names.
#[test]
fn message_reports_are_transient_and_convert_to_canonical_facts() {
    fn reported(fact: Event, claim: &str) -> Event {
        let raw = || RawMessagePublisherId::new(claim);
        match fact {
            Event::MessageDelivered(value) => {
                Event::MessageDeliveredReported(value.with_publisher(raw()))
            }
            Event::MessageEdited(value) => {
                Event::MessageEditedReported(value.with_publisher(raw()))
            }
            Event::MessageDeleted(value) => {
                Event::MessageDeletedReported(value.with_publisher(raw()))
            }
            Event::MessageReactionAdded(value) => {
                Event::MessageReactionAddedReported(value.with_publisher(raw()))
            }
            Event::MessageReactionRemoved(value) => {
                Event::MessageReactionRemovedReported(value.with_publisher(raw()))
            }
            Event::MessageSent(value) => Event::MessageSentReported(value.with_publisher(raw())),
            _ => unreachable!("message fixture is canonical"),
        }
    }

    let canonical = representative_events()
        .into_iter()
        .filter(|event| event.message_agent_target().is_some())
        .collect::<Vec<_>>();
    assert_eq!(canonical.len(), 6);
    for fact in canonical {
        let report = reported(fact.clone(), "bridge-main");
        assert!(report.is_message_report());
        assert!(!report.defaults_to_persist());
        assert!(report.name().to_string().ends_with("_reported"));
        assert_eq!(
            report.into_stamped_canonical_message_fact(
                MessagePublisherId::parse("bridge-main").expect("canonical publisher"),
            ),
            Some(fact.clone())
        );

        for invalid_claim in [
            "other-bridge",
            "bad publisher",
            "control\u{1f}",
            "c1\u{80}",
            "unicode-☃",
        ] {
            let invalid = reported(fact.clone(), invalid_claim);
            let json = serde_json::to_vec(&invalid).expect("encode raw report as JSON");
            let decoded_json: Event =
                serde_json::from_slice(&json).expect("decode raw report from JSON");
            assert_eq!(decoded_json, invalid);

            let mut cbor = Vec::new();
            ciborium::into_writer(&invalid, &mut cbor).expect("encode raw report as CBOR");
            let decoded_cbor: Event =
                ciborium::from_reader(cbor.as_slice()).expect("decode raw report from CBOR");
            assert_eq!(decoded_cbor, invalid);

            let publisher = MessagePublisherId::parse("bridge-main").expect("canonical publisher");
            assert_eq!(invalid.into_stamped_canonical_message_fact(publisher), None);
        }
    }
}

/// Ensures every representative first-party event keeps its serde tag,
/// `EventName` constant, `Event::name()` dispatch, and default durability in
/// sync. Keep serialized event tags, parsed names, and `Event::name()`
/// synchronized.
#[test]
fn first_party_event_wire_tags_match_event_names_and_persistence() {
    let events = representative_events();
    let mut seen = std::collections::BTreeSet::new();
    for event in events {
        if !(matches!(event, Event::ExtensionEvent(_))) {
            let name = event.name();
            let wire = serde_json::to_value(&event).expect("serialize event");
            let tag = wire
                .get("event")
                .and_then(serde_json::Value::as_str)
                .expect("event tag");

            assert_eq!(tag, name.to_string(), "wire tag and Event::name diverged");
            assert_eq!(tag.parse::<EventName>(), Ok(name.clone()));
            assert_eq!(
                event.defaults_to_persist(),
                expected_default_persist(&event),
                "unexpected default persistence setting for {tag}"
            );
            assert!(seen.insert(tag.to_owned()), "duplicate sample for {tag}");
        }
    }

    assert_eq!(seen, expected_first_party_event_names());
}

/// Ensures every discovery event preserves its complete nested wire
/// payload through JSON and CBOR, including both effective-skill source kinds.
#[test]
fn discovery_snapshot_events_round_trip_complete_wire_payloads() {
    let events = representative_events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                    | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                    | Event::AgentInitializationContextSet(_)
                    | Event::HarnessAgentContextInitialized(_)
                    | Event::HarnessSessionSkillsAvailable(_)
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 5);

    for event in events {
        let json = serde_json::to_vec(&event).expect("encode discovery event as JSON");
        assert_eq!(
            serde_json::from_slice::<Event>(&json).expect("decode discovery event from JSON"),
            event
        );

        let mut cbor = Vec::new();
        ciborium::into_writer(&event, &mut cbor).expect("encode discovery event as CBOR");
        assert_eq!(
            ciborium::from_reader::<Event, _>(cbor.as_slice())
                .expect("decode discovery event from CBOR"),
            event
        );
    }
}

/// Ensures sampled modification times use an explicit signed-microsecond wire
/// value and optional empty replacement fields remain representable.
#[test]
fn discovery_snapshot_timestamp_and_empty_replacement_wire_shapes_are_stable() {
    for micros in [-1, 0, 1] {
        let candidate = DiscoverySkillCandidate {
            name: "sample".into(),
            description: "sample".to_owned(),
            file_path: "/sample/SKILL.md".into(),
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
            sampled_modified: Some(DiscoveryModifiedMicros::new(micros)),
        };
        let json = serde_json::to_value(&candidate).expect("serialize timestamp candidate");
        assert_eq!(json["sampled_modified"], micros);
        assert_eq!(
            serde_json::from_value::<DiscoverySkillCandidate>(json).expect("deserialize candidate"),
            candidate
        );
    }

    let absent = DiscoverySkillCandidate {
        name: "sample".into(),
        description: "sample".to_owned(),
        file_path: "/sample/SKILL.md".into(),
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
        sampled_modified: None,
    };
    let absent_json = serde_json::to_value(&absent).expect("serialize absent timestamp");
    assert!(absent_json.get("sampled_modified").is_none());

    let empty = Event::AgentInitializationContextSet(AgentInitializationContextSet {
        session_id: test_session_id("s1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: AgentInitializationId::parse("init-empty")
            .expect("test identifier must be valid"),
        agents_message: None,
        effective_skills: Vec::new(),
        agents_files: Vec::new(),
    });
    let json = serde_json::to_vec(&empty).expect("serialize empty replacement as JSON");
    assert_eq!(
        serde_json::from_slice::<Event>(&json).expect("deserialize empty replacement from JSON"),
        empty
    );
    let mut cbor = Vec::new();
    ciborium::into_writer(&empty, &mut cbor).expect("serialize empty replacement as CBOR");
    assert_eq!(
        ciborium::from_reader::<Event, _>(cbor.as_slice())
            .expect("deserialize empty replacement from CBOR"),
        empty
    );
}

fn expected_default_persist(event: &Event) -> bool {
    !event.is_message_report()
        && !matches!(
            event,
            Event::ToolRegistrationDeclared(_)
                | Event::ToolUnregistrationDeclared(_)
                | Event::ToolRegister(_)
                | Event::ToolUnregister(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
                | Event::ToolCancelled(_)
                | Event::ToolResultDisplay(_)
                | Event::ToolBackgroundResultDisplay(_)
                | Event::ProviderModelsDeclared(_)
                | Event::ProviderModelsUpdated(_)
                | Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ProviderResponseUpdated(_)
                | Event::ProviderPromptSubmitted(_)
                | Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::HarnessProviderQuotaChanged(_)
                | Event::AgentWatchesUpdated(_)
                | Event::AgentStatsUpdated(_)
                | Event::HarnessAgentContextInitialized(_)
                | Event::HarnessSessionSkillsAvailable(_)
                | Event::AgentReplayComplete(_)
                | Event::SessionReplayComplete(_)
                | Event::ToolProgressReported(_)
                | Event::ToolProgress(_)
                | Event::ToolDelegateProgress(_)
                | Event::ToolError(_)
                | Event::ActionSchemaPublished(_)
                | Event::ActionInvoke(_)
                | Event::ActionResult(_)
                | Event::ActionError(_)
                | Event::ExtPromptFragmentPublish(_)
                | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionContextReady(_)
                | Event::ExtensionContextProviderRegister(_)
                | Event::ExtensionContextReady(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtInternalPromptSubmitRequest(_)
                | Event::StartAgentRequest(_)
                | Event::AgentMetadataSetRequest(_)
                | Event::AgentMetadataUnsetRequest(_)
                | Event::ShellCommandProgressReported(_)
                | Event::ShellCommandProgress(_)
                | Event::ShellCommandFinishedReported(_)
                | Event::UiPromptSubmitted(_)
                | Event::AgentPromptQueued(_)
                | Event::AgentPromptRecalled(_)
                | Event::AgentPromptCreated(_)
                | Event::AgentPromptStarted(_)
                | Event::AgentPromptTerminated(_)
                | Event::AgentPromptPrewarmRequested(_)
                | Event::AgentState(_)
                | Event::UiCompactRequest(_)
                | Event::UiCreateAgent(_)
                | Event::UiPromptDraft(_)
                | Event::UiFocusChanged(_)
                | Event::UiSetAgentDisplayName(_)
        )
}

fn expected_first_party_event_names() -> std::collections::BTreeSet<String> {
    [
        "action.error",
        "action.invoke",
        "action.result",
        "action.schema_published",
        "agent.compaction_triggered",
        "agent.compacted",
        "agent.display_name_set",
        "agent.head_moved",
        "agent.inference_dispatch_started",
        "agent.initialization_context_set",
        "agent.message_received",
        "agent.message_sent",
        "agent.metadata_set",
        "agent.metadata_set_request",
        "agent.metadata_unset",
        "agent.metadata_unset_request",
        "agent.prompt_created",
        "agent.prompt_prewarm_requested",
        "agent.prompt_queued",
        "agent.prompt_recalled",
        "agent.prompt_started",
        "agent.prompt_steered",
        "agent.prompt_submitted",
        "agent.prompt_terminated",
        "agent.replay_complete",
        "agent.start_accepted",
        "agent.start_request",
        "agent.start_result",
        "agent.started",
        "agent.standalone_compaction_failed",
        "agent.standalone_compaction_started",
        "agent.state",
        "agent.stats_updated",
        "agent.user_message_injected",
        "agent.watches_updated",
        "extension.agent_context_publish",
        "extension.agent_discovery_snapshot_declared",
        "extension.context_provider_register",
        "extension.context_ready",
        "extension.prompt_fragment_publish",
        "extension.internal_prompt_submit_request",
        "extension.ready",
        "extension.restarting",
        "extension.session_context_provider_register",
        "extension.session_context_ready",
        "extension.session_discovery_snapshot_declared",
        "extension.starting",
        "extension.exited",
        "harness.agent_context_usage_changed",
        "harness.agent_context_initialized",
        "harness.context_usage_changed",
        "harness.efforts_available",
        "harness.models_available",
        "harness.notice",
        "harness.provider_quota_changed",
        "harness.role_selected",
        "harness.roles_available",
        "harness.session_dir",
        "harness.session_skills_available",
        "harness.thinking_summaries_available",
        "harness.ui_dir",
        "harness.verbosities_available",
        "message.deleted",
        "message.deleted_reported",
        "message.delivered",
        "message.delivered_reported",
        "message.edited",
        "message.edited_reported",
        "message.reaction_added",
        "message.reaction_added_reported",
        "message.reaction_removed",
        "message.reaction_removed_reported",
        "message.sent",
        "message.sent_reported",
        "provider.cache_miss_diagnostic",
        "provider.cache_miss_diagnostic_reported",
        "provider.models_declared",
        "provider.models_updated",
        "provider.prompt_submitted",
        "provider.prompt_submitted_reported",
        "provider.quota_clear_reported",
        "provider.quota_patch_reported",
        "provider.quota_replace_reported",
        "provider.response_finished",
        "provider.response_finished_reported",
        "provider.response_updated",
        "provider.response_updated_reported",
        "provider.retry_prompt_result_reported",
        "provider.tool_error",
        "provider.tool_result",
        "session.agent_loaded",
        "session.agent_unloaded",
        "session.replay_complete",
        "session.shutdown",
        "session.started",
        "shell.command_finished",
        "shell.command_finished_reported",
        "shell.command_progress",
        "shell.command_progress_reported",
        "term.bell",
        "term.osc1337_set_user_var",
        "tool.background_error",
        "tool.background_result",
        "tool.background_result_display",
        "tool.cancel_request",
        "tool.cancelled",
        "tool.cancelled_reported",
        "tool.delegate_progress",
        "tool.error",
        "tool.error_reported",
        "tool.progress",
        "tool.progress_reported",
        "tool.registration_declared",
        "tool.register",
        "tool.rejected",
        "tool.request",
        "tool.result",
        "tool.result_display",
        "tool.result_reported",
        "tool.started",
        "tool.unregistration_declared",
        "tool.unregister",
        "ui.agent_model_select",
        "ui.cancel_prompt",
        "ui.compact_request",
        "ui.create_agent",
        "ui.focus_changed",
        "ui.navigate_tree",
        "ui.prompt_draft",
        "ui.prompt_submitted",
        "ui.recall_queued_prompt",
        "ui.role_select",
        "ui.role_update",
        "ui.set_agent_display_name",
        "ui.shell_command",
        "ui.switch_session",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Ensures parsed event names reject malformed segment structure so custom
/// events cannot enter routing with malformed dotted names.
#[test]
fn event_name_rejects_empty_segments() {
    for name in [".progress", "demo.", ".", "demo.extra.progress"] {
        assert!(name.parse::<EventName>().is_err());
    }

    assert!("demo.progress".parse::<EventName>().is_ok());
}

/// Ensures agent-message event variants report stable event names and
/// persistence defaults.
#[test]
fn agent_message_events_have_names_and_persistence_defaults() {
    let sent = Event::AgentMessageSent(AgentMessageSent {
        message_id: AgentMessageId::parse("msg-1")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("engineer_abcd1234"),
        recipient: AgentMessageRecipient::User,
        kind: AgentMessageKind::Message,
        message: "hello".to_owned(),
    });
    assert_eq!(sent.name(), EventName::AGENT_MESSAGE_SENT);
    assert_eq!(sent.name().to_string(), "agent.message_sent");
    assert!(sent.defaults_to_persist());

    let received = Event::AgentMessageReceived(AgentMessageReceived {
        message_id: AgentMessageId::parse("msg-2")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("engineer_abcd1234"),
        sender_session_id: None,
        recipient_id: agent_id("reviewer_efgh5678"),
        kind: AgentMessageKind::Message,
        watch_turn_state: None,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        message: "hello back".to_owned(),
    });
    assert_eq!(received.name(), EventName::AGENT_MESSAGE_RECEIVED);
    assert_eq!(received.name().to_string(), "agent.message_received");
    assert!(received.defaults_to_persist());
}

/// Ensures legacy agent-message payloads omit the default message kind but
/// preserve non-default watch notifications.
#[test]
fn agent_message_kind_defaults_and_serializes_only_when_non_default() {
    let legacy: AgentMessageReceived = serde_json::from_value(serde_json::json!({
        "message_id": "msg-legacy",
        "sender_id": "engineer_abcd1234",
        "recipient_id": "reviewer_efgh5678",
        "message": "hello"
    }))
    .expect("legacy message without kind decodes");
    assert_eq!(legacy.kind, AgentMessageKind::Message);

    let explicit_message = AgentMessageReceived {
        message_id: AgentMessageId::parse("msg-message")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("engineer_abcd1234"),
        sender_session_id: None,
        recipient_id: agent_id("reviewer_efgh5678"),
        kind: AgentMessageKind::Message,
        watch_turn_state: None,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        message: "hello".to_owned(),
    };
    let message_json = serde_json::to_value(&explicit_message).expect("serialize message");
    assert_eq!(message_json.get("kind"), None);

    let watch_response = AgentMessageReceived {
        kind: AgentMessageKind::WatchResponse,
        ..explicit_message
    };
    let watch_json = serde_json::to_value(&watch_response).expect("serialize watch response");
    assert_eq!(watch_json["kind"], serde_json::json!("watch_response"));

    let watch_prompt = AgentMessageReceived {
        kind: AgentMessageKind::WatchPrompt,
        ..watch_response
    };
    let prompt_json = serde_json::to_value(&watch_prompt).expect("serialize watch prompt");
    assert_eq!(prompt_json["kind"], serde_json::json!("watch_prompt"));

    let watch_turn = AgentMessageReceived {
        kind: AgentMessageKind::WatchTurnState,
        watch_turn_state: Some(AgentWatchTurnStateNotification {
            session_id: test_session_id("session-1"),
            subscription_id: "watch-subscription-1".to_owned(),
            state: AgentRuntimeState::Running,
            initial: true,
            turn_generation: 7,
        }),
        ..watch_prompt
    };
    let turn_json = serde_json::to_value(&watch_turn).expect("serialize watch turn");
    assert_eq!(turn_json["kind"], serde_json::json!("watch_turn_state"));
    assert_eq!(turn_json["watch_turn_state"]["turn_generation"], 7);
    let decoded: AgentMessageReceived =
        serde_json::from_value(turn_json).expect("decode watch turn");
    assert_eq!(decoded, watch_turn);
}

/// Ensures representative harness input/output messages round-trip through the
/// CBOR codec.
#[test]
fn representative_directional_messages_round_trip_through_cbor() {
    for message in representative_input_messages() {
        let encoded = encode_harness_input_to_vec(&message).expect("input should encode");
        let decoded = decode_harness_input_from_slice(&encoded).expect("input should decode");
        assert_eq!(decoded, message);
    }

    for message in representative_output_messages() {
        let encoded = encode_harness_output_to_vec(&message).expect("output should encode");
        let decoded = decode_harness_output_from_slice(&encoded).expect("output should decode");
        assert_eq!(decoded, message);
    }
}

/// Ensures semantic watch DTOs retain their approved closed wire shapes.
#[test]
fn semantic_watch_notifications_round_trip_with_closed_phases() {
    let status = AgentWatchWorkStatusNotification {
        session_id: test_session_id("session-1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 4,
        phase: AgentWorkStatusPhase::Working,
        title: Some("trace restore".to_owned()),
        initial: false,
    };
    let value = serde_json::to_value(&status).expect("serialize work status");
    assert_eq!(value["phase"], "working");
    assert!(
        serde_json::from_value::<AgentWatchWorkStatusNotification>(serde_json::json!({
            "session_id": "session-1",
            "subscription_id": "watch-1",
            "status_epoch": 4,
            "phase": "invented",
            "title": "trace restore",
            "initial": false
        }))
        .is_err()
    );

    let wait = AgentWatchLongWaitNotification {
        session_id: test_session_id("session-1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 4,
        threshold_minutes: 30,
    };
    let decoded: AgentWatchLongWaitNotification =
        serde_json::from_value(serde_json::to_value(&wait).expect("serialize long wait"))
            .expect("decode long wait");
    assert_eq!(decoded, wait);
}

/// Visible metadata classification covers spoofing-prone default ignorables,
/// variation selectors, tags, and Unicode noncharacters.
#[test]
fn visible_metadata_classifier_covers_default_ignorables_and_noncharacters() {
    for character in [
        '\u{00ad}',
        '\u{034f}',
        '\u{061c}',
        '\u{115f}',
        '\u{180e}',
        '\u{200d}',
        '\u{202e}',
        '\u{2066}',
        '\u{3164}',
        '\u{fe0f}',
        '\u{ffa0}',
        '\u{fff0}',
        '\u{fff8}',
        '\u{1bca0}',
        '\u{1d173}',
        '\u{e0100}',
        '\u{fdd0}',
        '\u{10ffff}',
    ] {
        assert!(
            requires_visible_escape(character),
            "U+{:04X}",
            character as u32
        );
        assert!(visible_escape_metadata(&character.to_string()).starts_with("\\u{"));
    }
    assert!(!requires_visible_escape('🦀'));
}

/// Canonical activation markers must default false for legacy payloads and
/// round-trip true without changing model-visible text.
#[test]
fn canonical_inference_activation_defaults_and_round_trips() {
    let active_submitted = AgentPromptSubmitted {
        inference_activation: true,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        text: "hello".to_owned(),
        message_class: PromptMessageClass::User,
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    };
    let mut legacy = serde_json::to_value(&active_submitted).expect("encode legacy base");
    legacy
        .as_object_mut()
        .expect("object")
        .remove("inference_activation");
    let decoded: AgentPromptSubmitted =
        serde_json::from_value(legacy).expect("legacy prompt decodes");
    assert!(!decoded.inference_activation);

    let encoded = serde_json::to_value(&active_submitted).expect("encode");
    assert_eq!(encoded["inference_activation"], true);
    assert_eq!(
        serde_json::from_value::<AgentPromptSubmitted>(encoded).expect("decode"),
        active_submitted
    );
    let mut passive_submitted = active_submitted.clone();
    passive_submitted.inference_activation = false;
    let encoded = serde_json::to_value(&passive_submitted).expect("encode false");
    assert!(encoded.get("inference_activation").is_none());
    assert!(
        !serde_json::from_value::<AgentPromptSubmitted>(encoded)
            .expect("decode false")
            .inference_activation
    );

    let active_injected = AgentUserMessageInjected {
        inference_activation: true,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        text: "injected".to_owned(),
        message_class: PromptMessageClass::Internal,
    };
    let mut legacy = serde_json::to_value(&active_injected).expect("encode legacy base");
    legacy
        .as_object_mut()
        .expect("object")
        .remove("inference_activation");
    let decoded: AgentUserMessageInjected =
        serde_json::from_value(legacy).expect("legacy injection decodes");
    assert!(!decoded.inference_activation);

    let encoded = serde_json::to_value(&active_injected).expect("encode");
    assert_eq!(encoded["inference_activation"], true);
    assert_eq!(
        serde_json::from_value::<AgentUserMessageInjected>(encoded).expect("decode"),
        active_injected
    );
    let mut passive_injected = active_injected.clone();
    passive_injected.inference_activation = false;
    let encoded = serde_json::to_value(&passive_injected).expect("encode false");
    assert!(encoded.get("inference_activation").is_none());
    assert!(
        !serde_json::from_value::<AgentUserMessageInjected>(encoded)
            .expect("decode false")
            .inference_activation
    );

    let active_steered = AgentPromptSteered {
        inference_activation: true,
        submission_source: PromptSubmissionSource::HarnessInternal,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        text: "steered".to_owned(),
        message_class: PromptMessageClass::User,
        internal_kind: None,
        ctx_id: Some("ctx-1".to_owned()),
    };
    let mut legacy = serde_json::to_value(&active_steered).expect("encode legacy base");
    legacy
        .as_object_mut()
        .expect("object")
        .remove("inference_activation");
    let decoded: AgentPromptSteered =
        serde_json::from_value(legacy).expect("legacy steering decodes");
    assert!(!decoded.inference_activation);

    let encoded = serde_json::to_value(&active_steered).expect("encode");
    assert_eq!(encoded["inference_activation"], true);
    assert_eq!(
        serde_json::from_value::<AgentPromptSteered>(encoded).expect("decode"),
        active_steered
    );
    let mut passive_steered = active_steered.clone();
    passive_steered.inference_activation = false;
    let encoded = serde_json::to_value(&passive_steered).expect("encode false");
    assert!(encoded.get("inference_activation").is_none());
    assert!(
        !serde_json::from_value::<AgentPromptSteered>(encoded)
            .expect("decode false")
            .inference_activation
    );
}

/// Ensures extension-data path wrappers keep the existing string wire shape
/// while giving Rust callers semantic path fields.
#[test]
fn extension_data_paths_use_string_wire_shape() {
    let op = ExtensionDataRequestOp::RenameFile {
        from: ExtensionDataPath::new("old/name"),
        to: ExtensionDataPath::new("new/name"),
    };

    let value = serde_json::to_value(&op).expect("operation should serialize");

    assert_eq!(
        value,
        serde_json::json!({
            "op": "rename_file",
            "from": "old/name",
            "to": "new/name"
        })
    );

    let decoded: ExtensionDataRequestOp =
        serde_json::from_value(value).expect("operation should deserialize");
    assert_eq!(decoded, op);

    let entry = ExtensionDataEntry {
        path: ExtensionDataPath::new("listed/name"),
        is_dir: false,
        len: Some(4),
    };
    let value = serde_json::to_value(&entry).expect("entry should serialize");
    assert_eq!(value["path"], "listed/name");
    let decoded: ExtensionDataEntry =
        serde_json::from_value(value).expect("entry should deserialize");
    assert_eq!(decoded, entry);
}

/// Ensures single-slice decoders reject extra bytes instead of accepting a
/// valid message prefix and ignoring trailing garbage.
#[test]
fn decode_message_from_slice_rejects_trailing_bytes() {
    let message = HarnessInputMessage::Ready(Ready { message: None });
    let mut encoded = encode_harness_input_to_vec(&message).expect("message should encode");
    encoded.extend_from_slice(&[0xff, 0x00]);

    let error = decode_harness_input_from_slice(&encoded).expect_err("trailing bytes should fail");

    assert!(
        error.to_string().contains("trailing bytes"),
        "unexpected error: {error}"
    );
}

/// Ensures framed readers can decode multiple back-to-back protocol messages
/// from one stream.
#[test]
fn multiple_directional_messages_can_share_one_stream() {
    let messages = representative_output_messages();
    let mut writer = HarnessOutputWriter::new(Vec::new());
    for message in &messages {
        writer
            .write_message(message)
            .expect("output message should encode");
    }
    writer.flush().expect("stream should flush");

    let bytes = writer.into_inner();
    let mut reader = HarnessOutputReader::new(std::io::Cursor::new(bytes));
    let mut decoded = Vec::new();
    for _ in 0..messages.len() {
        decoded.push(
            reader
                .read_message()
                .expect("read should succeed")
                .expect("message should arrive"),
        );
    }

    assert_eq!(decoded, messages);
}

/// Ensures streaming codec byte accounting reuses the real encode/decode work
/// and attributes each back-to-back item independently.
#[test]
fn directional_message_codec_reports_actual_item_sizes() {
    let messages = representative_output_messages();
    let mut writer = HarnessOutputWriter::new(Vec::new());
    let encoded_sizes = messages
        .iter()
        .map(|message| {
            writer
                .write_message_with_size(message)
                .expect("output message should encode")
        })
        .collect::<Vec<_>>();
    let bytes = writer.into_inner();
    assert_eq!(
        encoded_sizes.iter().map(|bytes| bytes.get()).sum::<u64>(),
        bytes.len() as u64
    );

    let mut reader = HarnessOutputReader::new(std::io::Cursor::new(bytes));
    let decoded = messages
        .iter()
        .map(|_| {
            reader
                .read_message_with_size()
                .expect("read should succeed")
                .expect("message should arrive")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        decoded
            .iter()
            .map(|decoded| &decoded.message)
            .collect::<Vec<_>>(),
        messages.iter().collect::<Vec<_>>()
    );
    assert_eq!(
        decoded
            .iter()
            .map(|decoded| decoded.encoded_bytes)
            .collect::<Vec<_>>(),
        encoded_sizes
    );
    assert!(
        reader
            .read_message_with_size()
            .expect("clean EOF should succeed")
            .is_none()
    );
}

/// Ensures extension-defined events cannot spoof first-party event categories
/// that routing and policy code treat as typed protocol events.
#[test]
fn custom_event_rejects_reserved_event_names() {
    let value = serde_json::json!({
        "event": "extension.event",
        "payload": {
            "name": "harness.notice",
            "payload": "spoofed"
        }
    });

    let error = serde_json::from_value::<Event>(value).expect_err("reserved name should fail");

    assert!(
        error.to_string().contains("extension-owned category"),
        "unexpected error: {error}"
    );
}

/// Ensures custom event validation treats manually constructed `Other` values
/// with reserved wire text as reserved categories.
#[test]
fn custom_event_rejects_reserved_category_spelled_as_other() {
    let name = EventName::new(
        EventCategory::Other("harness".to_owned()),
        "info".to_owned(),
    );

    assert!(!CustomEvent::name_is_allowed(&name));

    let error = CustomEvent::try_new(name.clone(), None, CborValue::Null)
        .expect_err("reserved custom event name should fail");
    assert_eq!(error.name(), &name);
    assert_eq!(error.into_name(), name);
}
/// Ensures the panicking EventName constructor enforces dynamic segment
/// validation for public callers.
#[test]
#[should_panic(expected = "invalid event name segment")]
fn event_name_new_panics_on_invalid_segments() {
    // This call is intentionally best-effort; preserve the existing discarded
    // result. ast-grep-ignore: let-underscore-call
    let _ = EventName::new(EventCategory::Other("demo.extra".to_owned()), "progress");
}

/// Ensures dynamic event-name construction rejects invalid segment text before
/// custom events can enter routing or serialization.
#[test]
fn custom_event_rejects_direct_empty_segments() {
    assert!(EventName::try_new(EventCategory::Other(String::new()), "progress").is_none());
    assert!(EventName::try_new(EventCategory::Other("demo".to_owned()), String::new()).is_none());
    assert!(
        EventName::try_new(
            EventCategory::Other("harness.notice".to_owned()),
            "progress"
        )
        .is_none()
    );
    assert!(
        EventName::try_new(EventCategory::Other("demo".to_owned()), "extra.progress").is_none()
    );
}

/// Ensures extension-owned custom event categories still round-trip and route
/// by their payload name.
#[test]
fn custom_event_allows_extension_owned_event_names() {
    let event = Event::ExtensionEvent(
        CustomEvent::try_new(
            "demo.progress".parse().expect("custom event name"),
            None,
            CborValue::Text("working".to_owned()),
        )
        .expect("valid custom event"),
    );

    let encoded = serde_json::to_value(&event).expect("serialize custom event");
    let decoded: Event = serde_json::from_value(encoded).expect("decode custom event");

    assert_eq!(decoded.name(), "demo.progress".parse().expect("event name"));
    assert_eq!(decoded, event);
}

/// Ensures peer-to-harness emits and harness-to-peer deliveries keep distinct
/// wire tags.
#[test]
fn input_emit_and_output_deliver_are_distinct_wire_messages() {
    let event = sample_session_started();
    let input = HarnessInputMessage::emit_with_persist(event.clone(), false);
    let output =
        HarnessOutputMessage::deliver_live(UnixMicros::new(1_700_000_000_000_000), event.clone());

    let input_json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(input_json["message"], "emit");
    assert_eq!(input_json["payload"]["event"]["event"], "session.started");
    assert_eq!(input_json["payload"]["persist"], false);
    assert!(input_json["payload"].get("transient").is_none());

    let output_json = serde_json::to_value(&output).expect("serialize output");
    assert_eq!(output_json["message"], "deliver");
    assert_eq!(output_json["payload"]["event"]["event"], "session.started");
    assert_eq!(
        output_json["payload"]["recorded_at"],
        serde_json::json!(1_700_000_000_000_000_u64)
    );
    // Live deliveries omit the replay marker entirely; only replayed
    // history pays for the extra field on the wire.
    assert!(output_json["payload"].get("replay").is_none());
    assert!(output_json["payload"].get("seq").is_none());
    assert!(output_json["payload"].get("persist").is_none());
    assert!(output_json["payload"].get("transient").is_none());

    let input_bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert!(decode_harness_output_from_slice(&input_bytes).is_err());

    let output_bytes = encode_harness_output_to_vec(&output).expect("encode output");
    assert!(decode_harness_input_from_slice(&output_bytes).is_err());
}

/// Both persistence polarities are explicit on the new wire schema, and an
/// envelope containing only the removed `transient` field cannot decode.
#[test]
fn emit_wire_requires_explicit_positive_persistence_metadata() {
    let durable = HarnessInputMessage::emit(sample_session_started());
    let live_only = HarnessInputMessage::emit_transient(sample_session_started());

    for (message, expected) in [(durable, true), (live_only, false)] {
        let json = serde_json::to_value(&message).expect("serialize emit");
        assert_eq!(json["payload"]["persist"], expected);
        assert!(json["payload"].get("transient").is_none());
        assert_eq!(
            serde_json::from_value::<HarnessInputMessage>(json).expect("decode emit"),
            message
        );
    }

    let removed_schema = serde_json::json!({
        "message": "emit",
        "payload": {
            "event": serde_json::to_value(sample_session_started()).expect("serialize event"),
            "transient": false
        }
    });
    assert!(
        serde_json::from_value::<HarnessInputMessage>(removed_schema).is_err(),
        "the removed field must not supply the required positive persistence bit"
    );
}

/// Interception carries both explicit persistence polarities and cannot decode
/// an envelope that supplies only the removed negative field.
#[test]
fn intercept_request_wire_requires_explicit_positive_persistence_metadata() {
    for persist in [true, false] {
        let message = HarnessOutputMessage::InterceptRequest(InterceptRequest {
            event: Box::new(sample_session_started()),
            persist,
        });
        let json = serde_json::to_value(&message).expect("serialize intercept request");
        assert_eq!(json["payload"]["persist"], persist);
        assert!(json["payload"].get("transient").is_none());
        assert_eq!(
            serde_json::from_value::<HarnessOutputMessage>(json).expect("decode intercept request"),
            message
        );
    }

    let removed_schema = serde_json::json!({
        "message": "intercept_request",
        "payload": {
            "event": serde_json::to_value(sample_session_started()).expect("serialize event"),
            "transient": false
        }
    });
    assert!(
        serde_json::from_value::<HarnessOutputMessage>(removed_schema).is_err(),
        "the removed field must not supply required interceptor persistence metadata"
    );
}

/// Routine extension notices use a flat dedicated request with no peer-owned
/// event metadata.
#[test]
fn extension_notice_request_uses_dedicated_input_message() {
    let input = HarnessInputMessage::ExtensionNoticeRequest(ExtensionNoticeRequest {
        message: "bridge reconnecting".to_owned(),
        level: NoticeLevel::Warning,
    });
    let json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(
        json,
        serde_json::json!({
            "message": "extension_notice_request",
            "payload": {
                "message": "bridge reconnecting",
                "level": "warning"
            }
        })
    );

    let bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert_eq!(
        decode_harness_input_from_slice(&bytes).expect("decode input"),
        input
    );
    assert!(decode_harness_output_from_slice(&bytes).is_err());
    assert!(decode_message_from_slice::<Event>(&bytes).is_err());
}

/// UI debug stats use a flat dedicated input message and cannot decode as an
/// event or harness output.
#[test]
fn ui_debug_event_stats_request_uses_dedicated_input_message() {
    let input = HarnessInputMessage::UiDebugEventStatsRequest(UiDebugEventStatsRequest {
        extension_name: test_extension_name("std-shell"),
    });
    let json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(json["message"], "ui_debug_event_stats_request");
    assert_eq!(json["payload"]["extension_name"], "std-shell");

    let bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert_eq!(
        decode_harness_input_from_slice(&bytes).expect("decode input"),
        input
    );
    assert!(decode_harness_output_from_slice(&bytes).is_err());
    assert!(decode_message_from_slice::<Event>(&bytes).is_err());
}

/// UI detach uses a flat dedicated input message and cannot decode as an event
/// or harness output.
#[test]
fn ui_detach_request_uses_dedicated_input_message() {
    let input = HarnessInputMessage::UiDetachRequest(UiDetachRequest {});
    let json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(json["message"], "ui_detach_request");
    assert_eq!(json["payload"], serde_json::json!({}));

    let bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert_eq!(
        decode_harness_input_from_slice(&bytes).expect("decode input"),
        input
    );
    assert!(decode_harness_output_from_slice(&bytes).is_err());
    assert!(decode_message_from_slice::<Event>(&bytes).is_err());
}

/// The superseded dotted detach event has no compatibility decoder.
#[test]
fn removed_ui_detach_request_event_has_no_decoder() {
    let removed = serde_json::json!({
        "event": "ui.detach_request",
        "payload": {}
    });
    assert!(serde_json::from_value::<Event>(removed).is_err());
}

/// UI tree inspection uses a flat dedicated input message and cannot decode as
/// an event or harness output.
#[test]
fn ui_tree_request_uses_dedicated_input_message() {
    let input = HarnessInputMessage::UiTreeRequest(UiTreeRequest {
        session_id: test_session_id("s1"),
        target_agent_id: Some(agent_id("agent-1")),
    });
    let json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(
        json,
        serde_json::json!({
            "message": "ui_tree_request",
            "payload": {
                "session_id": "s1",
                "target_agent_id": "agent-1"
            }
        })
    );
    assert_eq!(
        serde_json::to_value(HarnessInputMessage::UiTreeRequest(UiTreeRequest {
            session_id: test_session_id("s1"),
            target_agent_id: None,
        }))
        .expect("serialize request without target"),
        serde_json::json!({
            "message": "ui_tree_request",
            "payload": {
                "session_id": "s1"
            }
        })
    );

    let bytes = encode_harness_input_to_vec(&input).expect("encode input");
    assert_eq!(
        decode_harness_input_from_slice(&bytes).expect("decode input"),
        input
    );
    assert!(decode_harness_output_from_slice(&bytes).is_err());
    assert!(decode_message_from_slice::<Event>(&bytes).is_err());
}

/// The superseded dotted tree-request event has no compatibility decoder.
#[test]
fn removed_ui_tree_request_event_has_no_decoder() {
    let removed = serde_json::json!({
        "event": "ui.tree_request",
        "payload": {
            "session_id": "s1",
            "target_agent_id": "agent-1"
        }
    });
    assert!(serde_json::from_value::<Event>(removed).is_err());
}

/// Ensures raw events are not accepted where directional protocol messages are
/// required.
#[test]
fn bare_event_is_not_a_protocol_item_in_either_direction() {
    let bytes = encode_message_to_vec(&sample_session_started()).expect("encode bare event");
    assert!(decode_harness_input_from_slice(&bytes).is_err());
    assert!(decode_harness_output_from_slice(&bytes).is_err());
}

/// Ensures configuration requires stable publisher provenance while retaining
/// the independently optional extension state directory.
#[test]
fn configure_requires_instance_name_and_keeps_state_dir_optional() {
    assert!(
        serde_json::from_value::<Configure>(serde_json::json!({
            "config": null
        }))
        .is_err()
    );
    let parsed: Configure = serde_json::from_value(serde_json::json!({
        "config": null,
        "instance_name": "demo"
    }))
    .expect("configure decodes");

    assert_eq!(parsed.config, CborValue::Null);
    assert_eq!(parsed.instance_name.as_str(), "demo");
    assert_eq!(parsed.state_dir, None);
    assert!(parsed.secrets.is_empty());

    let with_state = Configure {
        instance_name: crate::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        tool_prefix: None,
        config: CborValue::Null,
        state_dir: Some(std::path::PathBuf::from("/tmp/tau/state/ext/demo")),
        secrets: std::collections::BTreeMap::new(),
    };
    let json = serde_json::to_value(&with_state).expect("serialize configure");
    assert_eq!(
        json["state_dir"],
        serde_json::json!("/tmp/tau/state/ext/demo")
    );
    let decoded: Configure = serde_json::from_value(json).expect("decode configure");
    assert_eq!(decoded, with_state);

    let without_state = serde_json::to_value(Configure {
        instance_name: crate::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        tool_prefix: None,
        config: CborValue::Null,
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
    .expect("serialize configure without state dir");
    assert!(without_state.get("state_dir").is_none());
}

/// Ensures configure secrets round-trip while Debug output redacts secret
/// material.
#[test]
fn configure_secrets_round_trip_and_debug_redacts_values() {
    // Secret values travel only to explicitly configured extensions and must not
    // leak through derived protocol debug output.
    let mut secrets = std::collections::BTreeMap::new();
    secrets.insert("mail_password".to_owned(), SecretValue::new("super-secret"));
    let configure = Configure {
        instance_name: crate::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        tool_prefix: None,
        config: CborValue::Null,
        state_dir: None,
        secrets,
    };

    let debug = format!("{configure:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("super-secret"));

    let json = serde_json::to_value(&configure).expect("serialize configure");
    assert_eq!(
        json["secrets"]["mail_password"],
        serde_json::json!("super-secret")
    );
    let decoded: Configure = serde_json::from_value(json).expect("decode configure");
    assert_eq!(
        decoded.secrets["mail_password"].expose_secret(),
        "super-secret"
    );
}

/// Ensures directional protocol messages use the expected flat tagged wire
/// representation.
#[test]
fn directional_message_wire_form_uses_flat_message_tag() {
    let input = HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: test_extension_name("provider"),
        client_kind: ClientKind::Provider,
        capabilities: Default::default(),
    });
    let input_json = serde_json::to_value(&input).expect("serialize input");
    assert_eq!(input_json["message"], "hello");
    assert!(input_json.get("payload").is_some());

    let output = HarnessOutputMessage::Disconnect(Disconnect {
        reason: Some("shutdown".to_owned()),
    });
    let output_json = serde_json::to_value(&output).expect("serialize output");
    assert_eq!(output_json["message"], "disconnect");
    assert!(output_json.get("payload").is_some());
}

/// Ensures events serialize with dotted event names as the wire tag.
#[test]
fn event_wire_form_uses_dotted_event_tag() {
    let event = Event::ToolStarted(ToolStarted {
        call_id: "call-1".into(),
        tool_name: ToolName::new("echo"),
        arguments: CborValue::Text("hi".to_owned()),
        agent_id: agent_id("agent-1"),
        originator: PromptOriginator::User,
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "tool.started");
    assert!(json.get("payload").is_some());
}

/// Internal extension prompt requests are a narrow control request, not a
/// durable transcript fact. This locks in the current wire name and field set.
#[test]
fn extension_internal_prompt_submit_request_wire_form() {
    let event = Event::ExtInternalPromptSubmitRequest(ExtInternalPromptSubmitRequest {
        agent_id: agent_id("agent-1"),
        text: "timer fired".to_owned(),
        ctx_id: Some("timer:wake:1".to_owned()),
        activation_kind: None,
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "extension.internal_prompt_submit_request");
    assert_eq!(json["payload"]["agent_id"], "agent-1");
    assert_eq!(json["payload"]["ctx_id"], "timer:wake:1");
    assert!(json["payload"].get("message_class").is_none());
    assert_eq!(
        event.name(),
        EventName::EXTENSION_INTERNAL_PROMPT_SUBMIT_REQUEST
    );
}

/// The removed extension user-message prompt request must not be a protocol
/// event.
#[test]
fn removed_extension_prompt_submit_request_has_no_decoder() {
    let removed = serde_json::json!({
        "event": "extension.prompt_submit_request",
        "payload": {
            "agent_id": "agent-1",
            "text": "removed user message",
            "message_class": "user"
        }
    });
    assert!(serde_json::from_value::<Event>(removed).is_err());
}

/// Ensures model ids split only on the first slash so provider model names may
/// contain slashes.
#[test]
fn model_id_parses_provider_and_slashy_model_name() {
    // OpenRouter and similar providers use native model ids such as
    // `anthropic/claude-sonnet-4`. The first slash separates Tau's provider
    // namespace; remaining slashes belong to the provider-native model id.
    let model: ModelId = "openrouter/anthropic/claude-sonnet-4"
        .parse()
        .expect("model id");

    assert_eq!(model.provider.as_str(), "openrouter");
    assert_eq!(model.model.as_str(), "anthropic/claude-sonnet-4");
    assert_eq!(model.to_string(), "openrouter/anthropic/claude-sonnet-4");
}

/// Ensures provider model declaration and canonical state use distinct provider
/// event names.
#[test]
fn provider_model_event_names_match_wire_family() {
    // Both names are routed and intercepted independently. Keep the peer-authored
    // declaration distinct from the harness-authored current-state projection.
    let cases = [
        (
            Event::ProviderModelsDeclared(ProviderModelsDeclared { models: Vec::new() }),
            "provider.models_declared",
        ),
        (
            Event::ProviderModelsUpdated(ProviderModelsUpdated {
                publisher_extension_id: crate::ExtensionName::parse("provider")
                    .expect("test extension name must satisfy the identifier grammar"),
                models: Vec::new(),
            }),
            "provider.models_updated",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["event"], expected);
        assert!(!event.defaults_to_persist());
        let mut cbor = Vec::new();
        ciborium::into_writer(&event, &mut cbor).expect("encode cbor");
        assert_eq!(
            ciborium::from_reader::<Event, _>(cbor.as_slice()).expect("decode cbor"),
            event
        );
    }
}

/// Tool declarations and canonical state retain distinct wire names, transient
/// defaults, and canonical configured-instance provenance.
#[test]
fn tool_lifecycle_event_names_and_provenance_match_wire_family() {
    let declaration = ToolRegistrationDeclared {
        tool: echo_tool_spec(),
        tool_group: None,
        prompt_fragment: None,
    };
    let cases = [
        (
            Event::ToolRegistrationDeclared(declaration.clone()),
            "tool.registration_declared",
        ),
        (
            Event::ToolUnregistrationDeclared(ToolUnregistrationDeclared {
                tool_name: ToolName::new("echo"),
            }),
            "tool.unregistration_declared",
        ),
        (
            Event::ToolRegister(ToolRegister {
                publisher_extension_id: crate::ExtensionName::parse("tool-extension")
                    .expect("test extension name must satisfy the identifier grammar"),
                publisher_instance_id: ExtensionInstanceId::new(9),
                tool: declaration.tool,
                tool_group: None,
                prompt_fragment: None,
            }),
            "tool.register",
        ),
        (
            Event::ToolUnregister(ToolUnregister {
                publisher_extension_id: crate::ExtensionName::parse("tool-extension")
                    .expect("test extension name must satisfy the identifier grammar"),
                publisher_instance_id: ExtensionInstanceId::new(9),
                tool_name: ToolName::new("echo"),
            }),
            "tool.unregister",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["event"], expected);
        assert!(!event.defaults_to_persist());
        let mut cbor = Vec::new();
        ciborium::into_writer(&event, &mut cbor).expect("encode cbor");
        assert_eq!(
            ciborium::from_reader::<Event, _>(cbor.as_slice()).expect("decode cbor"),
            event
        );
    }
}

/// Tool progress submission and canonical observation share a payload while
/// retaining distinct transient wire names.
#[test]
fn tool_progress_report_and_canonical_fact_have_distinct_wire_names() {
    let progress = ToolProgress {
        call_id: "progress-call".into(),
        tool_name: ToolName::new("owned_tool"),
        message: Some("running".to_owned()),
        progress: Some(ProgressUpdate {
            current: Some(1),
            total: Some(2),
        }),
        display: None,
    };
    for (event, expected) in [
        (
            Event::ToolProgressReported(progress.clone()),
            "tool.progress_reported",
        ),
        (Event::ToolProgress(progress.clone()), "tool.progress"),
    ] {
        assert_eq!(event.name().to_string(), expected);
        assert!(!event.defaults_to_persist());
        let json = serde_json::to_value(&event).expect("serialize progress event");
        assert_eq!(json["event"], expected);
        assert_eq!(
            serde_json::from_value::<Event>(json).expect("decode progress event"),
            event
        );
    }
}

/// Terminal tool reports and canonical facts retain distinct wire names while
/// reusing the exact payload DTOs.
#[test]
fn terminal_tool_reports_and_canonical_facts_have_distinct_wire_names() {
    let result = ToolResult {
        call_id: "result-call".into(),
        tool_name: ToolName::new("owned_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    };
    let error = ToolError {
        call_id: "error-call".into(),
        tool_name: ToolName::new("owned_tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    };
    let cancelled = ToolCancelled {
        call_id: "cancelled-call".into(),
        tool_name: ToolName::new("owned_tool"),
        tool_type: ToolType::Function,
    };
    let result_display = ToolResultDisplay::from(&result);
    for (event, expected) in [
        (
            Event::ToolResultReported(result.clone()),
            "tool.result_reported",
        ),
        (Event::ToolResult(result), "tool.result"),
        (
            Event::ToolResultDisplay(result_display),
            "tool.result_display",
        ),
        (
            Event::ToolErrorReported(error.clone()),
            "tool.error_reported",
        ),
        (Event::ToolError(error), "tool.error"),
        (
            Event::ToolCancelledReported(cancelled.clone()),
            "tool.cancelled_reported",
        ),
        (Event::ToolCancelled(cancelled), "tool.cancelled"),
    ] {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize terminal event");
        assert_eq!(json["event"], expected);
        assert_eq!(
            serde_json::from_value::<Event>(json).expect("decode terminal event"),
            event
        );
    }
}

/// Ensures execution lifecycle events retain the provider wire-family event
/// names.
#[test]
fn execution_events_use_provider_wire_family() {
    // Provider extensions own execution status; agent transcript events use
    // `agent.*`, but provider execution progress remains in the `provider.*`
    // family so subscribers can route it separately.
    let cases = [
        (
            Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
                agent_prompt_id: test_agent_prompt_id("sp-1"),
                originator: PromptOriginator::User,
            }),
            "provider.prompt_submitted",
        ),
        (
            Event::ProviderResponseUpdated(ProviderResponseUpdated {
                agent_prompt_id: test_agent_prompt_id("sp-1"),
                agent_id: agent_id("engineer_abcd1234"),
                deltas: Vec::new(),
                compaction: None,
                status: None,
                response_stats: None,
                originator: PromptOriginator::User,
            }),
            "provider.response_updated",
        ),
        (
            Event::ProviderResponseFinished(ProviderResponseFinished {
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: test_agent_prompt_id("sp-1"),
                agent_id: agent_id("engineer_abcd1234"),
                stop_reason: ProviderStopReason::EndTurn,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: ContextRecoveryDisposition::None,
                originator: PromptOriginator::User,
                output_items: Vec::new(),
                usage: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            "provider.response_finished",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["event"], expected);
    }
}

/// Provider peers use distinct transient report wires; only the harness emits
/// canonical execution facts and directed retry outcomes.
#[test]
fn provider_execution_reports_use_distinct_transient_wires() {
    let prompt_id = test_agent_prompt_id("sp-1");
    let agent_id = agent_id("engineer_abcd1234");
    let reports = [
        (
            Event::ProviderPromptSubmittedReported(ProviderPromptSubmitted {
                agent_prompt_id: prompt_id.clone(),
                originator: PromptOriginator::User,
            }),
            EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED,
        ),
        (
            Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
                agent_prompt_id: prompt_id.clone(),
                agent_id: agent_id.clone(),
                deltas: Vec::new(),
                compaction: None,
                status: None,
                response_stats: None,
                originator: PromptOriginator::User,
            }),
            EventName::PROVIDER_RESPONSE_UPDATED_REPORTED,
        ),
        (
            Event::ProviderResponseFinishedReported(ProviderResponseFinished {
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: prompt_id.clone(),
                agent_id,
                stop_reason: ProviderStopReason::EndTurn,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: ContextRecoveryDisposition::None,
                originator: PromptOriginator::User,
                output_items: Vec::new(),
                usage: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            EventName::PROVIDER_RESPONSE_FINISHED_REPORTED,
        ),
        (
            Event::ProviderRetryPromptResultReported(ProviderRetryPromptResult {
                request_id: RetryPromptRequestId::parse("retry-1").expect("retry id"),
                agent_prompt_id: prompt_id.clone(),
                status: RetryPromptStatus::Accepted,
            }),
            EventName::PROVIDER_RETRY_PROMPT_RESULT_REPORTED,
        ),
        (
            Event::ProviderCacheMissDiagnosticReported(ProviderCacheMissDiagnostic {
                agent_prompt_id: prompt_id,
                model: "provider/model".into(),
                originator: PromptOriginator::User,
                tool_choice: ToolChoice::default(),
                ws_pool_delta: None,
                input_tokens: 1,
                cached_tokens: 0,
                previous_input_tokens: 1,
                cacheable_input_tokens: 1,
                corrected_cache_efficiency: 0.0,
            }),
            EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC_REPORTED,
        ),
    ];
    for (event, expected_name) in reports {
        assert_eq!(event.name(), expected_name);
        assert!(!event.defaults_to_persist());
        let json = serde_json::to_value(&event).expect("encode report");
        assert_eq!(json["event"], expected_name.to_string());
        assert_eq!(
            serde_json::from_value::<Event>(json).expect("decode report"),
            event
        );
    }

    let legacy = serde_json::json!({
        "event": "provider.retry_prompt_result",
        "payload": {
            "request_id": "retry-1",
            "agent_prompt_id": "sp-1",
            "status": "accepted"
        }
    });
    assert!(serde_json::from_value::<Event>(legacy).is_err());
}

/// Ensures provider response updates require the new delta-routing fields
/// rather than accepting legacy text/thinking snapshots.
#[test]
fn provider_response_updated_requires_delta_routing_fields() {
    // The provider streaming payload is delta-based. Legacy text/thinking
    // snapshots must fail instead of silently decoding as empty delta updates.
    let value = serde_json::json!({
        "agent_prompt_id": "sp-1",
        "agent_id": "engineer_abcd1234",
        "text": "legacy assistant text",
        "thinking": "legacy reasoning text"
    });

    let error = serde_json::from_value::<ProviderResponseUpdated>(value)
        .expect_err("legacy streaming payload should not decode");
    assert!(
        error.to_string().contains("text"),
        "unexpected error: {error}"
    );
}

/// Provider response stats preserve zero-valued first-output timing, omit and
/// default its absence, and round-trip provider-owned live observations.
#[test]
fn provider_response_updated_response_stats_round_trip() {
    let update = ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-1"),
        agent_id: agent_id("engineer_abcd1234"),
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: Some(ProviderResponseStats {
            current: ProviderResponseStatsSample {
                response_bytes_received: 12_345,
                elapsed_micros: 2_000_000,
            },
            previous: ProviderResponseStatsSample {
                response_bytes_received: 4096,
                elapsed_micros: 1_000_000,
            },
            first_semantic_output_elapsed_micros: Some(0),
        }),
        originator: PromptOriginator::User,
    };

    let value = serde_json::to_value(&update).expect("serialize response stats update");
    assert_eq!(
        value["response_stats"]["current"]["response_bytes_received"],
        12_345
    );
    assert_eq!(
        value["response_stats"]["first_semantic_output_elapsed_micros"],
        0
    );
    let decoded: ProviderResponseUpdated =
        serde_json::from_value(value.clone()).expect("decode response stats update");
    assert_eq!(decoded, update);

    let mut absent = value;
    absent["response_stats"]
        .as_object_mut()
        .expect("response stats object")
        .remove("first_semantic_output_elapsed_micros");
    let decoded_absent: ProviderResponseUpdated =
        serde_json::from_value(absent).expect("decode absent first-output timing");
    assert_eq!(
        decoded_absent
            .response_stats
            .expect("response stats")
            .first_semantic_output_elapsed_micros,
        None
    );
    assert_eq!(
        ProviderResponseStats::default().first_semantic_output_elapsed_micros,
        None
    );
    let absent_value =
        serde_json::to_value(ProviderResponseStats::default()).expect("serialize default stats");
    assert!(
        absent_value
            .get("first_semantic_output_elapsed_micros")
            .is_none()
    );
}

/// Ensures generic watched-agent watch snapshots keep optional change metadata
/// optional while preserving the authoritative full watched set.
#[test]
fn agent_watches_updated_serde_round_trip() {
    let update = AgentWatchesUpdated {
        session_id: test_session_id("session_123"),
        watcher_id: agent_id("engineer_parent"),
        watched_agent_ids: vec![agent_id("engineer_child")],
        changed_agent_id: None,
        cause: AgentWatchUpdateCause::SessionSnapshot,
    };
    let value = serde_json::to_value(Event::AgentWatchesUpdated(update.clone()))
        .expect("serialize watches");
    assert_eq!(value["event"], "agent.watches_updated");
    assert!(value["payload"].get("changed_agent_id").is_none());
    let round_trip = serde_json::from_value::<Event>(value).expect("decode watches");
    assert_eq!(round_trip, Event::AgentWatchesUpdated(update));
}

/// Ensures generic agent stats snapshots support partial/unknown context usage
/// while carrying runtime state and complete tool counters.
#[test]
fn agent_stats_updated_serde_round_trip() {
    let update = AgentStatsUpdated {
        session_id: test_session_id("session_123"),
        agent_id: agent_id("engineer_child"),
        navigation_mode: AgentNavigationMode::Active,
        runtime_state: AgentRuntimeState::Running,
        tools: AgentToolStats {
            in_flight: 1,
            started_total: 3,
        },
        context: AgentContextStats {
            input_tokens: Some(42_000),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(21),
        },
        estimated_api_cost: Default::default(),
    };
    let value =
        serde_json::to_value(Event::AgentStatsUpdated(update.clone())).expect("serialize stats");
    assert_eq!(value["event"], "agent.stats_updated");
    assert!(value["payload"]["context"].get("cached_tokens").is_none());
    let round_trip = serde_json::from_value::<Event>(value).expect("decode stats");
    assert_eq!(round_trip, Event::AgentStatsUpdated(update));
}

/// Ensures the provider repetition stop reason has a stable protocol spelling.
#[test]
fn provider_stop_reason_repetition_detected_uses_snake_case_wire_value() {
    let json = serde_json::to_value(ProviderStopReason::RepetitionDetected)
        .expect("serialize stop reason");
    assert_eq!(json, serde_json::json!("repetition_detected"));
    let decoded: ProviderStopReason =
        serde_json::from_value(json).expect("deserialize stop reason");
    assert_eq!(decoded, ProviderStopReason::RepetitionDetected);
}

/// Ensures harness role info remains backward compatible when role descriptions
/// are omitted.
#[test]
fn harness_role_info_role_description_is_optional_and_round_trips() {
    // Older harnesses only send `description`; the new free-form role metadata
    // must default cleanly while preserving the technical description field.
    let legacy: HarnessRoleInfo = serde_json::from_value(serde_json::json!({
        "name": "engineer",
        "description": "model=openai/gpt-4.1, effort=high"
    }))
    .expect("decode legacy role info");
    assert_eq!(legacy.name, "engineer");
    assert_eq!(legacy.role_description, None);

    let with_description = HarnessRoleInfo {
        name: "deep".to_owned(),
        description: "model=openai/gpt-4.1, effort=xhigh".to_owned(),
        role_description: Some("Deep investigation mode".to_owned()),
        details: Some(HarnessRoleDetails {
            model: Some("openai/gpt-4.1".into()),
            params: ModelParams {
                effort: Effort::High,
                verbosity: Verbosity::Medium,
                thinking_summary: ThinkingSummary::Auto,
                service_tier: Some(ServiceTier::Fast),
            },
            tools: Some(vec![ToolName::new("read")]),
            enable_tool_groups: vec![ToolGroupName::new("pim")],
            disable_tool_groups: vec![ToolGroupName::new("shell")],
            enable_tools: vec![ToolName::new("web_search")],
            disable_tools: vec![ToolName::new("shell")],
        }),
    };
    let json = serde_json::to_value(&with_description).expect("serialize role info");
    assert_eq!(json["role_description"], "Deep investigation mode");
    assert_eq!(json["details"]["model"], "openai/gpt-4.1");
    assert_eq!(json["details"]["params"]["effort"], "high");
    assert_eq!(json["details"]["enable_tools"][0], "web_search");
    let decoded: HarnessRoleInfo = serde_json::from_value(json).expect("decode role info");
    assert_eq!(decoded, with_description);

    let without_description = serde_json::to_value(HarnessRoleInfo {
        role_description: None,
        ..with_description
    })
    .expect("serialize role info without metadata");
    assert!(without_description.get("role_description").is_none());
}

/// Ensures provider model metadata rejects missing context-window limits
/// required by scheduling/UI code.
#[test]
fn provider_model_info_requires_context_window() {
    // The harness uses provider snapshots as the only source of model UI
    // metadata, so context windows must be present instead of defaulted.
    let value = serde_json::json!({
        "id": "openai/gpt-4.1",
        "efforts": ["off"],
        "verbosities": ["medium"],
        "thinking_summaries": ["off"]
    });

    let error = serde_json::from_value::<ProviderModelInfo>(value)
        .expect_err("context_window should be required");
    assert!(
        error.to_string().contains("context_window"),
        "unexpected error: {error}"
    );
}

/// Context-limit telemetry must preserve its closed tags and optional evidence
/// across both supported durable wire encodings without introducing content.
#[test]
fn context_limit_telemetry_json_and_cbor_round_trip() {
    let mut telemetry = ContextLimitTelemetry {
        model: "openai/gpt-test".parse().expect("model"),
        operation: PromptOperation::StandaloneCompaction,
        projected_input_tokens: Some(127_000),
        transcript_delta_bytes: Some(8192),
        advertised_context_window: Some(128_000),
        provider_input_tokens: Some(127_000),
        projection_reserve_tokens: 4096,
        compaction_threshold: Some(115_200),
        compaction_policy: ContextLimitCompactionPolicy::Threshold,
        recovery_eligible: false,
        action: ContextLimitAction::Terminal,
        observation: ContextLimitObservation::RejectedBelowAdvertisedLimit,
    };
    let json = serde_json::to_value(&telemetry).expect("json");
    assert_eq!(json["observation"], "rejected_below_advertised_limit");
    assert_eq!(json["provider_input_tokens"], 127_000);
    assert_eq!(
        serde_json::from_value::<ContextLimitTelemetry>(json).expect("json decode"),
        telemetry
    );
    let mut cbor = Vec::new();
    ciborium::into_writer(&telemetry, &mut cbor).expect("cbor");
    assert_eq!(
        ciborium::from_reader::<ContextLimitTelemetry, _>(cbor.as_slice()).expect("cbor decode"),
        telemetry
    );

    telemetry.transcript_delta_bytes = None;
    let json = serde_json::to_value(&telemetry).expect("JSON without transcript delta");
    assert!(json.get("transcript_delta_bytes").is_none());
    assert_eq!(
        serde_json::from_value::<ContextLimitTelemetry>(json).expect("missing JSON field decodes"),
        telemetry
    );
    let mut cbor = Vec::new();
    ciborium::into_writer(&telemetry, &mut cbor).expect("CBOR without transcript delta");
    assert_eq!(
        ciborium::from_reader::<ContextLimitTelemetry, _>(cbor.as_slice())
            .expect("missing CBOR field decodes"),
        telemetry
    );
}

/// Ensures JSON-to-CBOR conversion preserves unsigned integer precision above
/// the exact range of IEEE-754 floats.
#[test]
fn json_to_cbor_preserves_large_unsigned_integers() {
    let max = serde_json::json!(u64::MAX);
    assert_eq!(
        json_to_cbor(&max),
        CborValue::Integer(ciborium::value::Integer::from(u64::MAX))
    );

    let above_precise_float = serde_json::json!(9_007_199_254_740_993_u64);
    assert_eq!(
        json_to_cbor(&above_precise_float),
        CborValue::Integer(ciborium::value::Integer::from(9_007_199_254_740_993_u64))
    );
}

/// Ensures valid tool identifiers are accepted by the ToolName validator.
#[test]
fn tool_name_accepts_valid_names() {
    assert!(ToolName::try_new("read").is_some());
    assert!(ToolName::try_new("shell").is_some());
    assert!(ToolName::try_new("my_tool_2").is_some());
    assert!(ToolName::try_new("Echo").is_some());
}

/// Ensures ToolName rejects empty names and names with unsupported separators
/// or whitespace.
#[test]
fn tool_name_rejects_invalid_names() {
    assert!(ToolName::try_new("").is_none());
    assert!(ToolName::try_new("fs.read").is_none());
    assert!(ToolName::try_new("my tool").is_none());
    assert!(ToolName::try_new("a-b").is_none());
    assert!(ToolName::try_new("tool/name").is_none());
}

/// Ensures the panicking ToolName constructor fails fast on invalid
/// identifiers.
#[test]
#[should_panic(expected = "invalid tool name")]
fn tool_name_new_panics_on_invalid() {
    // This call is intentionally best-effort; preserve the existing discarded
    // result. ast-grep-ignore: let-underscore-call
    let _ = ToolName::new("bad.name");
}

/// Ensures ToolName enforces its maximum byte length.
#[test]
fn tool_name_rejects_overlong_input() {
    // ASCII alphanumerics that exceed the cap must be rejected even
    // though they pass the character-class check.
    let long = "a".repeat(ToolName::MAX_LEN + 1);
    assert!(ToolName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolName::MAX_LEN);
    assert!(ToolName::try_new(at_cap).is_some());
}

/// Ensures model/tool policy tags accept lowercase namespaced values and reject
/// nondeterministic spellings such as uppercase or whitespace.
#[test]
fn model_and_tool_tags_validate_namespaced_lowercase_values() {
    assert!(ModelTag::try_new("shell:chatgpt").is_some());
    assert!(ToolTag::try_new("shell:edit:apply_patch").is_some());
    assert!(ToolTag::try_new("tools:custom-text").is_some());
    assert!(ModelTag::try_new("Shell:ChatGPT").is_none());
    assert!(ToolTag::try_new("shell edit").is_none());
    assert!(ToolTag::try_new("a".repeat(ToolTag::MAX_LEN + 1)).is_none());
}

/// Ensures tool group names enforce the same valid identifier shape as tool
/// names.
#[test]
fn tool_group_name_accepts_valid_names() {
    assert!(ToolGroupName::try_new("mail").is_some());
    assert!(ToolGroupName::try_new("project_tools_2").is_some());
    assert!(ToolGroupName::try_new("Ops").is_some());
}

/// Ensures invalid tool group names cannot enter the protocol through fallible
/// construction.
#[test]
fn tool_group_name_rejects_invalid_names() {
    assert!(ToolGroupName::try_new("").is_none());
    assert!(ToolGroupName::try_new("mail.send").is_none());
    assert!(ToolGroupName::try_new("mail send").is_none());
    assert!(ToolGroupName::try_new("mail-send").is_none());
    assert!(ToolGroupName::try_new("mail/send").is_none());
}

/// Ensures overlong tool group names are rejected before they can be
/// serialized.
#[test]
fn tool_group_name_rejects_overlong_input() {
    let long = "a".repeat(ToolGroupName::MAX_LEN + 1);
    assert!(ToolGroupName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolGroupName::MAX_LEN);
    assert!(ToolGroupName::try_new(at_cap).is_some());
}

/// Ensures event-delivery helpers preserve replay/live state and expose the
/// wrapped event.
#[test]
fn event_delivery_helpers_expose_replay_marker_and_inner_event() {
    // The replay marker is the contract side-effecting consumers rely on to
    // skip historical frames; live and direct deliveries must not carry it.
    let inner = sample_session_started();
    let message =
        HarnessOutputMessage::deliver_live(UnixMicros::new(1_700_000_000_000_000), inner.clone());

    let delivery = message.as_delivery().expect("delivery payload");
    assert!(!delivery.is_replay());
    assert_eq!(delivery.event(), &inner);
    assert_eq!(message.clone().into_delivered_event(), Some(inner.clone()));

    let replayed = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1_700_000_000_000_000),
        sample_session_started(),
    );
    assert!(replayed.as_delivery().expect("delivery").is_replay());

    let direct = HarnessOutputMessage::deliver(sample_session_started());
    assert!(!direct.as_delivery().expect("delivery").is_replay());

    let non_delivery = HarnessOutputMessage::Disconnect(Disconnect { reason: None });
    assert_eq!(non_delivery.as_delivery(), None);
    assert_eq!(non_delivery.into_delivered_event(), None);
}

/// Ensures positive default-persistence classification separates live-only and
/// durable event families.
#[test]
fn event_defaults_to_persist_separates_live_only_and_durable_kinds() {
    // The set named by `defaults_to_persist` is the contract the harness relies
    // on when a component publishes without explicit persistence metadata.
    // Lock it down here so any future edit to the matcher is intentional.
    let transient = [
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: test_agent_prompt_id("sp-1"),
            agent_id: agent_id("engineer_abcd1234"),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: None,
            display: None,
        }),
        Event::ToolProgressReported(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("provider running".to_owned()),
            progress: None,
            display: None,
        }),
        Event::ShellCommandProgressReported(ShellCommandProgress {
            command_id: ShellCommandId::parse("shell-progress")
                .expect("test identifier must satisfy its grammar"),
            stream: ShellStream::Stdout,
            chunk: "provider running".to_owned(),
            target_agent_id: Some(agent_id("worker")),
        }),
        Event::ShellCommandFinishedReported(ShellCommandFinished {
            command_id: ShellCommandId::parse("shell-finished")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id("worker")),
            output: "/tmp\n".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("read"),
            tool_type: ToolType::Function,
            message: "failed".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ActionSchemaPublished(ActionSchemaPublished {
            extension_name: test_extension_name("std-email"),
            instance_id: 7.into(),
            schema: action_schema_fixture(),
        }),
        Event::ActionInvoke(ActionInvoke {
            invocation_id: ActionInvocationId::parse("act-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            extension_name: test_extension_name("std-email"),
            instance_id: 7.into(),
            action_id: "email.out.list".to_owned(),
            raw_line: ":email out list".to_owned(),
            argv: Vec::new(),
            arguments: CborValue::Map(Vec::new()),
        }),
        Event::ActionResult(ActionResult {
            invocation_id: ActionInvocationId::parse("act-1")
                .expect("test identifier must satisfy its grammar"),
            action_id: "email.out.list".to_owned(),
            output: ActionOutput::Text {
                text: "ok".to_owned(),
            },
        }),
        Event::ActionError(ActionError {
            invocation_id: ActionInvocationId::parse("act-2")
                .expect("test identifier must satisfy its grammar"),
            action_id: "email.out.list".to_owned(),
            message: "nope".to_owned(),
            details: None,
        }),
        Event::ExtPromptFragmentPublish(ExtPromptFragmentPublish {
            fragment: PromptFragment::new(
                "extension.instructions",
                PromptPriority::new(10),
                "runtime instructions",
            ),
        }),
        Event::UiPromptDraft(UiPromptDraft {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id("agent-1")),
            text: "draft".to_owned(),
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "hi".to_owned(),
            agent_id: agent_id("agent"),
            message_class: PromptMessageClass::User,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: agent_id("worker"),
            text: "queued".to_owned(),
            message_class: PromptMessageClass::User,
        }),
        Event::AgentPromptRecalled(AgentPromptRecalled {
            agent_id: agent_id("worker"),
            text: "queued".to_owned(),
        }),
        Event::AgentPromptTerminated(AgentPromptTerminated {
            agent_id: agent_id("worker"),
            agent_prompt_id: test_agent_prompt_id("sp-stale"),
            reason: AgentPromptTerminationReason::Stale,
            originator: PromptOriginator::User,
        }),
    ];
    for event in &transient {
        assert!(
            !event.defaults_to_persist(),
            "{} should default to transient",
            event.name()
        );
    }

    let durable = [
        Event::SessionStarted(SessionStarted {
            session_id: test_session_id("s1"),
            reason: SessionStartReason::Initial,
        }),
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("worker"),
            text: "hi".to_owned(),
            message_class: PromptMessageClass::User,
            internal_kind: None,
            originator: PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        Event::SessionAgentLoaded(SessionAgentLoaded {
            agent_initialization_id: AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id("worker"),
            ephemeral: false,
        }),
        Event::AgentMetadataSet(AgentMetadataSet {
            agent_id: agent_id("worker"),
            key: AgentMetadataKey::new("ext_core-shell_cwd"),
            value: CborValue::Text("/tmp".to_owned()),
            inheritable: true,
            mutation_id: None,
        }),
        Event::AgentMetadataUnset(AgentMetadataUnset {
            agent_id: agent_id("worker"),
            key: AgentMetadataKey::new("ext_core-shell_cwd"),
        }),
    ];
    for event in &durable {
        assert!(
            event.defaults_to_persist(),
            "{} should be durable",
            event.name()
        );
    }
}

/// Ensures legacy tool-result events without an explicit kind deserialize as
/// final results.
#[test]
fn tool_result_kind_defaults_to_final_for_legacy_events() {
    let result: ToolResult = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "tool_name": "read",
        "tool_type": "function",
        "result": "ok",
        "originator": { "kind": "user" }
    }))
    .expect("legacy tool result decodes");
    assert_eq!(result.kind, ToolResultKind::Final);
}

/// Ensures prompt messages remain backward compatible by defaulting omitted
/// class to user.
#[test]
fn prompt_message_class_defaults_to_user_when_omitted() {
    let prompt: UiPromptSubmitted = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "text": "legacy",
        "agent_id": "agent",
        "originator": { "kind": "user" }
    }))
    .expect("ui prompt decodes");
    assert_eq!(prompt.message_class, PromptMessageClass::User);
    assert!(!prompt.message_class.is_internal());

    let submitted: AgentPromptSubmitted = serde_json::from_value(serde_json::json!({
        "agent_id": "worker",
        "text": "submitted"
    }))
    .expect("agent prompt decodes");
    assert_eq!(submitted.message_class, PromptMessageClass::User);
    assert_eq!(submitted.originator, PromptOriginator::User);
    assert_eq!(submitted.internal_kind, None);

    let queued: AgentPromptQueued = serde_json::from_value(serde_json::json!({
        "agent_id": "worker",
        "text": "queued"
    }))
    .expect("queued prompt decodes");
    assert_eq!(queued.message_class, PromptMessageClass::User);

    let internal = serde_json::to_value(AgentPromptSteered {
        inference_activation: false,
        submission_source: PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("worker"),
        text: "[tau-internal] Tool call `bg` is complete.".into(),
        message_class: PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    })
    .expect("serialize steered prompt");
    assert_eq!(internal["message_class"], serde_json::json!("internal"));
    assert!(internal.get("internal_kind").is_none());
}

/// Steered prompt provenance is required so replay never guesses which provider
/// presentation applies to a queued prompt.
#[test]
fn steered_prompt_requires_submission_source() {
    let missing_source = serde_json::json!({
        "agent_id": "worker",
        "text": "steered"
    });
    assert!(
        serde_json::from_value::<AgentPromptSteered>(missing_source).is_err(),
        "old steered records without typed provenance must not decode"
    );

    let steered = AgentPromptSteered {
        inference_activation: true,
        submission_source: PromptSubmissionSource::HumanUi,
        agent_id: agent_id("worker"),
        text: "steered".to_owned(),
        message_class: PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    };
    let encoded = serde_json::to_value(&steered).expect("serialize steered prompt");
    assert_eq!(encoded["submission_source"], serde_json::json!("human_ui"));
    assert_eq!(
        serde_json::from_value::<AgentPromptSteered>(encoded).expect("decode steered prompt"),
        steered
    );
}

/// Context-size alerts keep a typed optional tag on both durable prompt shapes
/// while untagged legacy payloads retain their absent-field compatibility.
#[test]
fn context_size_alert_internal_kind_round_trips_on_durable_prompts() {
    let submitted = AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("worker"),
        text: "compact soon".to_owned(),
        message_class: PromptMessageClass::Internal,
        internal_kind: Some(InternalPromptKind::ContextSizeAlert),
        originator: PromptOriginator::User,
        submission_source: PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    };
    let submitted_json = serde_json::to_value(&submitted).expect("serialize submitted alert");
    assert_eq!(
        submitted_json["internal_kind"],
        serde_json::json!("context_size_alert")
    );
    assert_eq!(
        serde_json::from_value::<AgentPromptSubmitted>(submitted_json)
            .expect("deserialize submitted alert")
            .internal_kind,
        Some(InternalPromptKind::ContextSizeAlert)
    );

    let steered = AgentPromptSteered {
        inference_activation: true,
        submission_source: PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("worker"),
        text: "compact after tools".to_owned(),
        message_class: PromptMessageClass::Internal,
        internal_kind: Some(InternalPromptKind::ContextSizeAlert),
        ctx_id: None,
    };
    let steered_json = serde_json::to_value(&steered).expect("serialize steered alert");
    assert_eq!(
        steered_json["internal_kind"],
        serde_json::json!("context_size_alert")
    );
    assert_eq!(
        serde_json::from_value::<AgentPromptSteered>(steered_json)
            .expect("deserialize steered alert")
            .internal_kind,
        Some(InternalPromptKind::ContextSizeAlert)
    );
}

/// Ephemeral-agent markers must be backwards-compatible with peers that do not
/// yet send the field, and compact on the wire for the durable default.
#[test]
fn ephemeral_agent_fields_default_false_and_skip_serializing() {
    let create: UiCreateAgent = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "role": "engineer"
    }))
    .expect("legacy create-agent");
    assert!(!create.ephemeral);

    let started = AgentStarted {
        creator: Some(AgentCreator::default()),

        agent_id: AgentId::parse("agent-1").expect("agent id"),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    };
    let json = serde_json::to_value(&started).expect("serialize started");
    assert!(json.get("ephemeral").is_none());

    let loaded = SessionAgentLoaded {
        agent_initialization_id: AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: test_session_id("s1"),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        ephemeral: true,
    };
    let json = serde_json::to_value(&loaded).expect("serialize loaded");
    assert_eq!(json["ephemeral"], serde_json::json!(true));
}

/// Tool specs default to enabled and omit default-valued fields for compact
/// extension registration payloads.
#[test]
fn tool_spec_defaults_and_background_support() {
    let parsed: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "echo",
        "description": "Echo a payload",
        "tool_type": "function"
    }))
    .expect("deserialize tool spec");
    assert!(parsed.enabled_by_default);

    let serialized = serde_json::to_value(&parsed).expect("serialize tool spec");
    assert!(serialized.get("enabled_by_default").is_none());
    assert!(serialized.get("background_support").is_none());
    assert_eq!(parsed.background_support, None);
    assert_eq!(
        BackgroundSupport::default_effective(),
        BackgroundSupport::MinForegroundSeconds(2)
    );

    let backgrounded: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "agent_start",
        "tool_type": "function",
        "background_support": "instant"
    }))
    .expect("deserialize background support");
    assert_eq!(
        backgrounded.background_support,
        Some(BackgroundSupport::Instant)
    );

    let disabled = ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    };
    let serialized = serde_json::to_value(&disabled).expect("serialize disabled tool spec");
    assert_eq!(
        serialized["enabled_by_default"],
        serde_json::Value::Bool(false)
    );
}

/// Prompt fragment primitives are transparent on the wire so config and
/// extension JSON can stay simple: priorities are numbers and prompt contents
/// are plain strings.
#[test]
fn prompt_fragment_primitives_serde_as_simple_values() {
    let priority: PromptPriority =
        serde_json::from_value(serde_json::json!(42)).expect("deserialize prompt priority");
    let content: PromptContent =
        serde_json::from_value(serde_json::json!("Use care")).expect("deserialize prompt content");

    assert_eq!(priority.get(), 42);
    assert_eq!(content.as_str(), "Use care");
    assert_eq!(
        serde_json::to_value(priority).expect("serialize prompt priority"),
        serde_json::json!(42)
    );
    assert_eq!(
        serde_json::to_value(content).expect("serialize prompt content"),
        serde_json::json!("Use care")
    );
}

fn echo_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

/// `tool.registration_declared` remains compatible with extensions that omit
/// prompt fragments, while newer extensions can attach one ordered prompt
/// fragment.
#[test]
fn tool_registration_declaration_prompt_is_optional_and_round_trips_when_present() {
    let without_prompt: ToolRegistrationDeclared = serde_json::from_value(serde_json::json!({
        "tool": {
            "name": "echo",
            "description": "Echo a payload",
            "tool_type": "function"
        }
    }))
    .expect("deserialize tool registration declaration without prompt");
    assert_eq!(without_prompt.prompt_fragment, None);

    let with_prompt = ToolRegistrationDeclared {
        tool: echo_tool_spec(),
        tool_group: None,
        prompt_fragment: Some(PromptFragment::new(
            "echo.instructions",
            PromptPriority::new(7),
            "Prefer the echo tool for echo requests.",
        )),
    };
    let json = serde_json::to_value(&with_prompt).expect("serialize tool declaration with prompt");
    assert_eq!(json["prompt_fragment"]["priority"], serde_json::json!(7));
    assert_eq!(
        json["prompt_fragment"]["template"],
        serde_json::json!("Prefer the echo tool for echo requests.")
    );
    let decoded: ToolRegistrationDeclared =
        serde_json::from_value(json).expect("decode prompt fragment");
    assert_eq!(decoded, with_prompt);
}

/// `StartAgentRequest` leaves role selection to the harness when omitted.
#[test]
fn start_agent_request_role_is_optional() {
    let parsed: StartAgentRequest = serde_json::from_value(serde_json::json!({
        "query_id": "q1",
        "instruction": "summarize"
    }))
    .expect("deserialize start-agent request");
    assert_eq!(parsed.role, None);
}

/// Legacy `DelegateProgress` metadata remains readable for old persisted
/// protocol samples. First-party sub-agent UI now uses generic watch/stat
/// events, but the legacy agent-id wire shape still validates through the
/// protocol newtype.
#[test]
fn delegate_progress_optional_metadata_and_agent_id_wire_contract() {
    let parsed: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress without optional metadata");
    assert_eq!(parsed.role, None);
    assert_eq!(parsed.agent_id, None);

    let with_metadata: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "agent_id": "agent-1",
        "role": "rush",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress with role and valid agent id");
    assert_eq!(with_metadata.role.as_deref(), Some("rush"));
    assert_eq!(with_metadata.agent_id.as_deref(), Some("agent-1"));

    let round_tripped = serde_json::to_value(&with_metadata).expect("serialize progress");
    assert_eq!(round_tripped["agent_id"], "agent-1");

    serde_json::from_value::<DelegateProgress>(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "agent_id": "bad.name",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect_err("invalid agent id should fail to deserialize");
}

/// `Verbosity::next_in` mirrors `Effort::next_in`. Even though the CLI
/// doesn't bind a cycle key for verbosity today, the helper is part of
/// the public API and the protocol tests should pin the same wrap /
/// skip / empty-allowed-set behaviour effort relies on.
#[test]
fn verbosity_next_in_skips_disallowed_levels_and_wraps() {
    use Verbosity::*;
    let canonical = [Low, Medium, High];

    assert_eq!(Low.next_in(&canonical), Medium);
    assert_eq!(High.next_in(&canonical), Low);

    let only_low_high = [Low, High];
    assert_eq!(Low.next_in(&only_low_high), High);
    assert_eq!(High.next_in(&only_low_high), Low);

    let pinned = [Medium];
    assert_eq!(Low.next_in(&pinned), Medium);
    assert_eq!(Medium.next_in(&pinned), Medium);

    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// `ThinkingSummary` parses from / displays through the canonical
/// wire forms used by commands and harness role config.
#[test]
fn thinking_summary_round_trips_through_display_and_from_str() {
    use ThinkingSummary::*;
    for level in [Off, Auto, Concise, Detailed] {
        let s = level.to_string();
        assert_eq!(s.parse::<ThinkingSummary>().ok(), Some(level));
    }
    assert!("bogus".parse::<ThinkingSummary>().is_err());
}

/// `ModelParams` serializes its bundled knobs as a flat object that
/// drops fields at their default value. Lets `harness.yaml`
/// snapshots stay tiny and avoids surprising callers that introspect
/// the wire shape.
#[test]
fn model_params_serializes_skipping_defaults() {
    let json = serde_json::to_value(ModelParams::default()).expect("serialize");
    assert_eq!(json, serde_json::json!({}));

    let json = serde_json::to_value(ModelParams {
        effort: Effort::High,
        verbosity: Verbosity::Low,
        thinking_summary: ThinkingSummary::Concise,
        service_tier: Some(ServiceTier::Fast),
    })
    .expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "effort": "high",
            "thinking_summary": "concise",
            "service_tier": "fast",
        })
    );
}

/// `Effort::next_in` must skip levels that aren't in the harness's
/// allowed set so cycling callers don't trap when (say) `xhigh` is
/// missing for the current model. Locking the behaviour with explicit
/// cases so a future refactor of the cycle helper can't silently
/// regress the UX.
#[test]
fn effort_next_in_skips_disallowed_levels_and_wraps() {
    use Effort::*;
    let canonical = [Off, Minimal, Low, Medium, High];
    let with_xhigh = [Off, Minimal, Low, Medium, High, XHigh];
    let with_max = [Off, Minimal, Low, Medium, High, XHigh, Max];

    // Without xhigh or max, High wraps back to Off.
    assert_eq!(High.next_in(&canonical), Off);
    // With xhigh but not max, XHigh wraps to Off.
    assert_eq!(High.next_in(&with_xhigh), XHigh);
    assert_eq!(XHigh.next_in(&with_xhigh), Off);
    // GPT-5.6 can advance through Max before wrapping.
    assert_eq!(XHigh.next_in(&with_max), Max);
    assert_eq!(Max.next_in(&with_max), Off);

    // Sparse allowed set (provider with no reasoning effort) — Off
    // is the only legal level, so any input lands there.
    let only_off = [Off];
    assert_eq!(High.next_in(&only_off), Off);
    assert_eq!(Off.next_in(&only_off), Off);

    // Empty allowed set falls through to plain `next()` so callers
    // that haven't received `HarnessEffortsAvailable` yet still make
    // progress.
    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// Ensures the GPT-5.6 maximum effort has stable config, display, and atomic
/// representations across protocol boundaries.
#[test]
fn max_effort_round_trips_all_protocol_representations() {
    assert_eq!(
        serde_json::to_string(&Effort::Max).expect("serialize"),
        r#""max""#
    );
    assert_eq!(
        serde_json::from_str::<Effort>(r#""max""#).expect("deserialize"),
        Effort::Max
    );
    assert_eq!("max".parse::<Effort>().expect("parse"), Effort::Max);
    assert_eq!(Effort::Max.to_string(), "max");
    assert_eq!(Effort::Max.as_u8(), 6);
    assert_eq!(Effort::from_u8(6), Some(Effort::Max));
}

/// Provider-facing tool responses must use the uniform header/body shape so
/// individual providers do not each invent their own CBOR rendering.
#[test]
fn tool_response_renders_headers_blank_line_and_body() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("/tmp/file".to_owned()),
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("line-numbered content".to_owned()),
            CborValue::Text("1 hello\n2 world".to_owned()),
        ),
    ]));

    assert_eq!(
        response.render(),
        "path: /tmp/file\ntotal_lines: 2\n\n1 hello\n2 world"
    );
}

/// Ensures ToolResponse renders conventional output fields as body text without
/// an extra label.
#[test]
fn tool_response_renders_output_field_as_body_without_label() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            CborValue::Integer(0.into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text("out stdout\nerr stderr".to_owned()),
        ),
    ]));

    assert_eq!(response.render(), "status: 0\n\nout stdout\nerr stderr");
}

/// Ensures ToolResponse rendering does not expose raw CBOR debug structure for
/// output fields.
#[test]
fn tool_response_output_field_hides_raw_data_from_rendered_body() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("format".to_owned()),
            CborValue::Text("name flags".to_owned()),
        ),
        (
            CborValue::Text("data".to_owned()),
            CborValue::Map(vec![
                (
                    CborValue::Text("format".to_owned()),
                    CborValue::Text("name flags".to_owned()),
                ),
                (
                    CborValue::Text("folders".to_owned()),
                    CborValue::Array(vec![CborValue::Text("INBOX selectable".to_owned())]),
                ),
            ]),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text("INBOX selectable".to_owned()),
        ),
    ]));

    assert_eq!(response.render(), "format: name flags\n\nINBOX selectable");
}

/// Ensures plain text tool responses render as body content without synthetic
/// headers.
#[test]
fn tool_response_leaves_plain_text_as_body_only() {
    let response = ToolResponse::from_cbor(&CborValue::Text("done".to_owned()));

    assert_eq!(response.render(), "done");
}

/// Ensures rendered array/map records remain visually separated for provider
/// readability.
#[test]
fn tool_response_separates_array_map_records_with_blank_lines() {
    let response = ToolResponse::from_cbor(&CborValue::Array(vec![
        CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("first".to_owned()),
        )]),
        CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("second".to_owned()),
        )]),
    ]));

    assert_eq!(response.render(), "name: first\n\nname: second");
}

/// Ensures arrays of scalar tool output values render compactly instead of as
/// noisy records.
#[test]
fn tool_response_keeps_scalar_arrays_compact() {
    let response = ToolResponse::from_cbor(&CborValue::Array(vec![
        CborValue::Text("name".to_owned()),
        CborValue::Text("description".to_owned()),
    ]));

    assert_eq!(response.render(), "name\ndescription");
}

/// Provider-visible rendering must not let header keys or values inject extra
/// records or raw terminal controls into model input.
#[test]
fn tool_response_escapes_header_controls() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("bad\nkey".to_owned()),
        CborValue::Text("value\r\u{1b}\0\t\u{85}".to_owned()),
    )]));

    assert_eq!(
        response.render(),
        "bad\\nkey: value\\r\\x1b\\0\\t\\u{85}\n\n"
    );
}

/// Directly constructed ToolResponse headers still pass through render-time
/// sanitization so header values cannot inject lines or raw DEL controls.
#[test]
fn tool_response_escapes_direct_header_value_controls() {
    let response = ToolResponse {
        raw: CborValue::Null,
        headers: vec![ToolResponseHeader {
            key: "status".to_owned(),
            value: "ok\nforged: yes\u{7f}".to_owned(),
        }],
        body: String::new(),
    };

    assert_eq!(response.render(), "status: ok\\nforged: yes\\u{7f}\n\n");
}

/// Body sanitization is last-resort provider safety: it preserves legitimate
/// line-feed record separators but escapes other raw controls.
#[test]
fn tool_response_preserves_body_lfs_but_escapes_controls() {
    let response = ToolResponse::from_cbor(&CborValue::Text(
        "line 1\nline 2\r\u{1b}\0\t\u{85}".to_owned(),
    ));

    assert_eq!(response.render(), "line 1\nline 2\\r\\x1b\\0\\t\\u{85}");
}

/// Unicode line and paragraph separators are not ASCII LF record separators, so
/// they must be escaped in both headers and bodies before model-visible output.
#[test]
fn tool_response_escapes_unicode_line_separators() {
    let header_response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("key\u{2028}next".to_owned()),
        CborValue::Text("value\u{2029}next".to_owned()),
    )]));
    let body_response = ToolResponse::from_cbor(&CborValue::Text(
        "line\u{2028}not-record\u{2029}end".to_owned(),
    ));

    assert_eq!(
        header_response.render(),
        "key\\u{2028}next: value\\u{2029}next\n\n"
    );
    assert_eq!(
        body_response.render(),
        "line\\u{2028}not-record\\u{2029}end"
    );
}

/// Metadata labels that are pushed into a multiline body still need single-line
/// escaping so a malicious key cannot forge additional labels.
#[test]
fn tool_response_escapes_multiline_body_labels() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![(
        CborValue::Text("label\nforged".to_owned()),
        CborValue::Text("first\nsecond".to_owned()),
    )]));

    assert_eq!(response.render(), "label\\nforged:\nfirst\nsecond");
}

/// Binary fallback rendering stays bounded and does not leak raw bytes into the
/// provider-visible transcript.
#[test]
fn tool_response_renders_bytes_as_bounded_placeholder() {
    let response = ToolResponse::from_cbor(&CborValue::Bytes(vec![0; 1024]));

    assert_eq!(response.render(), "<1024 bytes>");
}

/// Ensures a clear close name is suggested, while poor and tied matches are
/// suppressed so callers do not present misleading recovery hints.
#[test]
fn nearest_name_suggestion_is_conservative_and_tie_safe() {
    assert_eq!(
        nearest_name_suggestion("rea", ["read", "edit"].into_iter()),
        Some("read".to_owned())
    );
    assert_eq!(
        nearest_name_suggestion("xyz", ["read", "edit"].into_iter()),
        None
    );
    assert_eq!(
        nearest_name_suggestion("cat", ["bat", "cut"].into_iter()),
        None
    );
}

/// Ensures suggestion work is bounded by observed candidate count, not only by
/// accepted short candidates after filtering.
#[test]
fn nearest_name_suggestion_suppresses_oversized_candidate_sets() {
    let candidates = (0..=MAX_SUGGESTION_CANDIDATES)
        .map(|idx| format!("{}-{idx}", "x".repeat(MAX_SUGGESTION_NAME_CHARS + 10)))
        .collect::<Vec<_>>();
    let suggestion = nearest_name_suggestion("read", candidates.iter().map(String::as_str));

    assert_eq!(suggestion, None);
}

/// Ensures overlong requested names and candidates are handled without scanning
/// or scoring unbounded strings.
#[test]
fn nearest_name_suggestion_suppresses_overlong_names() {
    let overlong = "r".repeat(MAX_SUGGESTION_NAME_CHARS + 1);
    assert_eq!(
        nearest_name_suggestion(&overlong, ["read"].into_iter()),
        None
    );
    assert_eq!(
        nearest_name_suggestion("reed", ["read", overlong.as_str()].into_iter()),
        Some("read".to_owned())
    );
}

/// Ensures tool examples are protocol-optional so older tool registrations
/// deserialize without changing shape.
#[test]
fn tool_spec_examples_default_to_empty() {
    let value = serde_json::json!({
        "name": "read",
        "tool_type": "function"
    });

    let spec: ToolSpec = serde_json::from_value(value).expect("legacy tool spec");

    assert!(spec.examples.is_empty());
}

/// Ensures example metadata can round-trip when providers opt into it.
#[test]
fn tool_spec_examples_round_trip() {
    let spec = ToolSpec {
        name: ToolName::new("edit"),
        model_visible_name: None,
        description: None,
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "replace".to_owned(),
            title: Some("Replace".to_owned()),
            arguments: CborValue::Map(vec![(
                CborValue::Text("operation".to_owned()),
                CborValue::Text("replace".to_owned()),
            )]),
            note: Some("Use exact field names.".to_owned()),
            subcommand: Some(ToolExampleSelector {
                path: vec!["operation".to_owned()],
                value: CborValue::Text("replace".to_owned()),
            }),
        }],
    };

    let encoded = serde_json::to_value(&spec).expect("serialize");
    let decoded: ToolSpec = serde_json::from_value(encoded).expect("deserialize");

    assert_eq!(decoded, spec);
}
/// Provider tool-type metadata remains backward compatible when omitted while
/// preserving explicit Function+Custom publication.
#[test]
fn provider_model_supported_tool_types_json_roundtrip() {
    let mut value = serde_json::json!({
        "id": "openai/model",
        "context_window": 1000,
        "efforts": [],
        "verbosities": [],
        "thinking_summaries": [],
        "supports_compaction": false,
        "supports_standalone_compaction": false
    });
    let legacy: ProviderModelInfo =
        serde_json::from_value(value.clone()).expect("legacy model metadata");
    assert!(legacy.supported_tool_types.is_empty());
    assert!(legacy.input_modalities.is_empty());
    assert!(legacy.tool_result_modalities.is_empty());
    assert!(legacy.supports_parallel_tool_calls);

    value["supported_tool_types"] = serde_json::json!(["function", "custom"]);
    value["input_modalities"] = serde_json::json!(["text", "image"]);
    value["tool_result_modalities"] = serde_json::json!(["text", "image"]);
    value["supports_parallel_tool_calls"] = serde_json::json!(false);
    let explicit: ProviderModelInfo =
        serde_json::from_value(value).expect("explicit model metadata");
    assert_eq!(
        explicit.supported_tool_types,
        [ToolType::Function, ToolType::Custom]
    );
    assert!(!explicit.supports_parallel_tool_calls);
    let encoded = serde_json::to_value(explicit).expect("serialize model metadata");
    assert_eq!(
        encoded["supported_tool_types"],
        serde_json::json!(["function", "custom"])
    );
    assert_eq!(
        encoded["input_modalities"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(
        encoded["tool_result_modalities"],
        serde_json::json!(["text", "image"])
    );
    assert_eq!(encoded["supports_parallel_tool_calls"], false);
}
/// Terminal provider failure categories have stable snake-case wire values,
/// while old response frames without the additive field remain decodable.
#[test]
fn provider_failure_kind_wire_contract_is_backward_compatible() {
    assert_eq!(
        serde_json::to_value(ProviderFailureKind::ContextWindowExceeded).expect("serialize"),
        serde_json::json!("context_window_exceeded")
    );
    assert_eq!(
        serde_json::to_value(ProviderFailureKind::RequestRejected).expect("serialize"),
        serde_json::json!("request_rejected")
    );

    let mut value = serde_json::to_value(ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-wire"),
        agent_id: agent_id("engineer_abcd1234"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::Error,
        error: Some("bounded detail".to_owned()),
        failure_kind: Some(ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: ContextRecoveryDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("serialize response");
    assert_eq!(
        value.get("failure_kind"),
        Some(&serde_json::json!("context_window_exceeded"))
    );
    value
        .as_object_mut()
        .expect("response object")
        .remove("failure_kind");
    let legacy: ProviderResponseFinished =
        serde_json::from_value(value.clone()).expect("decode legacy response");
    assert_eq!(legacy.failure_kind, None);
    assert!(value.get("failure_kind").is_none());
    let mut none_response = legacy;
    none_response.failure_kind = None;
    let none_value = serde_json::to_value(none_response).expect("serialize response without kind");
    assert!(
        none_value.get("failure_kind").is_none(),
        "None must preserve the legacy omitted wire shape"
    );
}

/// Provider-watch wire states must round-trip with one tagged phase shape and
/// reject option-soup payloads that could contradict the selected phase.
#[test]
fn agent_watch_provider_state_wire_contract_enforces_phase_invariants() {
    for (category, label) in [
        (AgentWatchProviderCategory::Transport, "transport"),
        (AgentWatchProviderCategory::Overload, "overload"),
        (AgentWatchProviderCategory::Throttle, "throttle"),
        (AgentWatchProviderCategory::UsageWindow, "usage_window"),
        (AgentWatchProviderCategory::Account, "account"),
        (AgentWatchProviderCategory::Auth, "auth"),
        (AgentWatchProviderCategory::Unknown, "unknown"),
        (AgentWatchProviderCategory::ContextWindow, "context_window"),
        (AgentWatchProviderCategory::Compaction, "compaction"),
    ] {
        assert_eq!(category.as_str(), label);
        assert_eq!(
            serde_json::to_value(category).expect("category JSON"),
            label
        );
    }
    let states = [
        AgentWatchProviderState::Retrying {
            category: AgentWatchProviderCategory::Throttle,
            attempt: u32::MAX,
            next_retry_delay_secs: u32::MAX,
        },
        AgentWatchProviderState::RecoveringContext { attempt: 7 },
        AgentWatchProviderState::Blocked {
            category: AgentWatchProviderCategory::Compaction,
        },
        AgentWatchProviderState::DispatchUncertain {
            category: AgentWatchProviderCategory::Transport,
        },
        AgentWatchProviderState::TerminalError {
            failure_kind: ProviderFailureKind::ContextWindowExceeded,
            attempt: 9,
        },
    ];

    for state in states {
        let json = serde_json::to_value(&state).expect("serialize state as JSON");
        assert_eq!(
            json["phase"],
            serde_json::Value::String(state.phase_str().to_owned())
        );
        assert_eq!(
            serde_json::from_value::<AgentWatchProviderState>(json).expect("decode JSON state"),
            state
        );
        let mut cbor = Vec::new();
        ciborium::into_writer(&state, &mut cbor).expect("serialize state as CBOR");
        assert_eq!(
            ciborium::from_reader::<AgentWatchProviderState, _>(cbor.as_slice())
                .expect("decode CBOR state"),
            state
        );
    }

    for malformed in [
        serde_json::json!({"phase":"retrying","attempt":1,"next_retry_delay_secs":2}),
        serde_json::json!({"phase":"terminal_error","attempt":1}),
        serde_json::json!({
            "phase":"recovering_context",
            "attempt":1,
            "category":"unknown"
        }),
        serde_json::json!({
            "phase":"blocked",
            "category":"transport",
            "failure_kind":"unknown"
        }),
    ] {
        assert!(
            serde_json::from_value::<AgentWatchProviderState>(malformed).is_err(),
            "contradictory or incomplete tagged state must be rejected"
        );
    }

    let notification = AgentWatchProviderStatusNotification {
        session_id: test_session_id("session-wire"),
        subscription_id: "watch-wire".to_owned(),
        turn_generation: 4,
        agent_prompt_id: test_agent_prompt_id("sp-wire"),
        state: AgentWatchProviderState::Retrying {
            category: AgentWatchProviderCategory::Throttle,
            attempt: 5,
            next_retry_delay_secs: 6,
        },
        initial: false,
    };
    let mut notification_json =
        serde_json::to_value(&notification).expect("serialize notification");
    assert_eq!(notification_json["state"]["phase"], "retrying");
    assert!(
        notification_json.get("phase").is_none(),
        "state discriminator stays nested so additive envelope fields remain compatible"
    );
    notification_json["future_envelope_field"] = serde_json::json!("ignored");
    assert_eq!(
        serde_json::from_value::<AgentWatchProviderStatusNotification>(notification_json.clone())
            .expect("unknown envelope field is additive"),
        notification
    );
    notification_json["state"]["future_state_field"] = serde_json::json!(true);
    assert!(
        serde_json::from_value::<AgentWatchProviderStatusNotification>(notification_json).is_err(),
        "unknown state fields remain fail-closed"
    );
    let mut notification_cbor = Vec::new();
    ciborium::into_writer(&notification, &mut notification_cbor)
        .expect("serialize notification CBOR");
    assert_eq!(
        ciborium::from_reader::<AgentWatchProviderStatusNotification, _>(
            notification_cbor.as_slice()
        )
        .expect("decode notification CBOR"),
        notification
    );
}

/// Standalone-compaction triggers and recovery fields must preserve literal
/// wire tags, legacy omission defaults, and correlation across JSON and CBOR
/// replay.
#[test]
fn standalone_compaction_and_context_recovery_wire_contract() {
    fn cbor_event(event: &Event) -> ciborium::value::Value {
        let mut bytes = Vec::new();
        ciborium::into_writer(event, &mut bytes).expect("encode event CBOR");
        ciborium::from_reader(bytes.as_slice()).expect("decode CBOR value")
    }

    fn remove_cbor_field(value: &mut ciborium::value::Value, field: &str) -> bool {
        match value {
            ciborium::value::Value::Map(entries) => {
                let original_len = entries.len();
                entries.retain(
                    |(key, _)| !matches!(key, ciborium::value::Value::Text(text) if text == field),
                );
                let removed = entries.len() != original_len;
                removed
                    || entries
                        .iter_mut()
                        .any(|(_, value)| remove_cbor_field(value, field))
            }
            ciborium::value::Value::Array(values) => values
                .iter_mut()
                .any(|value| remove_cbor_field(value, field)),
            _ => false,
        }
    }

    fn has_cbor_text_field(value: &ciborium::value::Value, field: &str, expected: &str) -> bool {
        match value {
            ciborium::value::Value::Map(entries) => {
                entries.iter().any(|(key, value)| {
                    matches!(
                        (key, value),
                        (
                            ciborium::value::Value::Text(key),
                            ciborium::value::Value::Text(value)
                        ) if key == field && value == expected
                    )
                }) || entries
                    .iter()
                    .any(|(_, value)| has_cbor_text_field(value, field, expected))
            }
            ciborium::value::Value::Array(values) => values
                .iter()
                .any(|value| has_cbor_text_field(value, field, expected)),
            _ => false,
        }
    }

    fn decode_cbor_event(value: &ciborium::value::Value) -> Event {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).expect("encode modified CBOR");
        ciborium::from_reader(bytes.as_slice()).expect("decode modified event")
    }

    let mut started = AgentStandaloneCompactionStarted {
        agent_id: AgentId::parse("wire-agent").expect("agent id"),
        transaction_id: CompactionTransactionId::parse("ct-wire").expect("transaction id"),
        compact_prompt_id: test_agent_prompt_id("ap-compact"),
        cut: AgentHead::Root,
        resume_through: Some(AgentHead::Root),
        model: "provider/model".into(),
        operation: PromptOperation::StandaloneCompaction,
        originator: PromptOriginator::User,
        supersedes: None,
        trigger: StandaloneCompactionTrigger::Manual,
    };
    let mut legacy_started = serde_json::to_value(&started).expect("encode manual start");
    legacy_started
        .as_object_mut()
        .expect("start object")
        .remove("trigger");
    assert_eq!(
        serde_json::from_value::<AgentStandaloneCompactionStarted>(legacy_started)
            .expect("omitted trigger defaults"),
        started
    );

    started.trigger = StandaloneCompactionTrigger::ReactiveContextOverflow {
        failed_agent_prompt_id: test_agent_prompt_id("ap-overflow"),
    };
    let encoded = serde_json::to_value(&started).expect("encode reactive start");
    assert_eq!(
        serde_json::from_value::<AgentStandaloneCompactionStarted>(encoded)
            .expect("decode reactive start"),
        started
    );
    let mut started_cbor = Vec::new();
    ciborium::into_writer(&started, &mut started_cbor).expect("encode reactive start CBOR");
    assert_eq!(
        ciborium::from_reader::<AgentStandaloneCompactionStarted, _>(started_cbor.as_slice())
            .expect("decode reactive start CBOR"),
        started
    );
    started.trigger = StandaloneCompactionTrigger::AutomaticThreshold;
    let automatic_json = serde_json::to_value(&started).expect("encode automatic start");
    assert_eq!(
        automatic_json["trigger"]["kind"],
        serde_json::json!("automatic_threshold")
    );
    assert_eq!(
        serde_json::from_value::<AgentStandaloneCompactionStarted>(automatic_json)
            .expect("decode automatic start"),
        started
    );
    let manual_event = Event::AgentStandaloneCompactionStarted(AgentStandaloneCompactionStarted {
        trigger: StandaloneCompactionTrigger::Manual,
        ..started.clone()
    });
    let mut legacy_started_cbor = cbor_event(&manual_event);
    assert!(remove_cbor_field(&mut legacy_started_cbor, "trigger"));
    assert_eq!(decode_cbor_event(&legacy_started_cbor), manual_event);
    let automatic_event = Event::AgentStandaloneCompactionStarted(started.clone());
    assert!(has_cbor_text_field(
        &cbor_event(&automatic_event),
        "kind",
        "automatic_threshold"
    ));
    assert_eq!(
        decode_cbor_event(&cbor_event(&automatic_event)),
        automatic_event
    );

    let checkpoint = AgentInferenceDispatchStarted {
        agent_id: started.agent_id.clone(),
        transaction_id: None,
        agent_prompt_id: test_agent_prompt_id("ap-inference"),
        through: AgentHead::Root,
        model: Some("provider/model".into()),
        operation: Some(PromptOperation::Inference),
        activation_cut: Some(AgentHead::Root),
    };
    let checkpoint_json = serde_json::to_value(&checkpoint).expect("encode checkpoint");
    assert_eq!(
        serde_json::from_value::<AgentInferenceDispatchStarted>(checkpoint_json)
            .expect("decode checkpoint"),
        checkpoint
    );
    let mut legacy_checkpoint = serde_json::to_value(&checkpoint).expect("encode checkpoint");
    let object = legacy_checkpoint
        .as_object_mut()
        .expect("checkpoint object");
    object.remove("model");
    object.remove("operation");
    object.remove("activation_cut");
    let decoded_legacy = serde_json::from_value::<AgentInferenceDispatchStarted>(legacy_checkpoint)
        .expect("decode legacy checkpoint");
    assert_eq!(decoded_legacy.model, None);
    assert_eq!(decoded_legacy.operation, None);
    assert_eq!(decoded_legacy.activation_cut, None);
    let mut checkpoint_cbor = Vec::new();
    ciborium::into_writer(&checkpoint, &mut checkpoint_cbor).expect("encode checkpoint CBOR");
    assert_eq!(
        ciborium::from_reader::<AgentInferenceDispatchStarted, _>(checkpoint_cbor.as_slice())
            .expect("decode checkpoint CBOR"),
        checkpoint
    );
    let checkpoint_event = Event::AgentInferenceDispatchStarted(checkpoint.clone());
    assert_eq!(
        decode_cbor_event(&cbor_event(&checkpoint_event)),
        checkpoint_event
    );
    let legacy_checkpoint_event =
        Event::AgentInferenceDispatchStarted(AgentInferenceDispatchStarted {
            model: None,
            operation: None,
            activation_cut: None,
            ..checkpoint.clone()
        });
    let mut legacy_checkpoint_cbor = cbor_event(&checkpoint_event);
    for field in ["model", "operation", "activation_cut"] {
        assert!(remove_cbor_field(&mut legacy_checkpoint_cbor, field));
    }
    assert_eq!(
        decode_cbor_event(&legacy_checkpoint_cbor),
        legacy_checkpoint_event
    );

    let mut response = ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: checkpoint.agent_prompt_id,
        agent_id: started.agent_id,
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::Error,
        error: Some("safe error".to_owned()),
        failure_kind: Some(ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: ContextRecoveryDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let none_json = serde_json::to_value(&response).expect("encode no-disposition response");
    assert!(none_json.get("recovery_disposition").is_none());
    assert_eq!(
        serde_json::from_value::<ProviderResponseFinished>(none_json)
            .expect("omitted disposition defaults"),
        response
    );
    response.recovery_disposition = ContextRecoveryDisposition::ReactiveCompactionPlanned;
    let planned_json = serde_json::to_value(&response).expect("encode planned response");
    assert_eq!(
        planned_json["recovery_disposition"],
        "reactive_compaction_planned"
    );
    assert_eq!(
        serde_json::from_value::<ProviderResponseFinished>(planned_json)
            .expect("decode planned response"),
        response
    );
    let mut response_cbor = Vec::new();
    ciborium::into_writer(&response, &mut response_cbor).expect("encode planned response CBOR");
    assert_eq!(
        ciborium::from_reader::<ProviderResponseFinished, _>(response_cbor.as_slice())
            .expect("decode planned response CBOR"),
        response
    );
    let planned_event = Event::ProviderResponseFinished(response.clone());
    assert_eq!(
        decode_cbor_event(&cbor_event(&planned_event)),
        planned_event
    );
    let none_event = Event::ProviderResponseFinished(ProviderResponseFinished {
        recovery_disposition: ContextRecoveryDisposition::None,
        ..response
    });
    let none_cbor = cbor_event(&none_event);
    let mut disposition_probe = none_cbor.clone();
    assert!(
        !remove_cbor_field(&mut disposition_probe, "recovery_disposition"),
        "None disposition is omitted from the enclosing CBOR event"
    );
    assert_eq!(decode_cbor_event(&none_cbor), none_event);
    let mut legacy_response_cbor = cbor_event(&planned_event);
    assert!(remove_cbor_field(
        &mut legacy_response_cbor,
        "recovery_disposition"
    ));
    assert_eq!(decode_cbor_event(&legacy_response_cbor), none_event);
}
/// Streaming protocol readers reject a single oversized peer-controlled frame
/// before handing it to higher-level routing or activation queues.
#[test]
fn streaming_reader_rejects_oversized_protocol_message() {
    let message = HarnessInputMessage::ConfigError(ConfigError {
        message: "x".repeat(MAX_PROTOCOL_MESSAGE_BYTES as usize + 1),
    });
    let mut encoded = Vec::new();
    HarnessInputWriter::new(&mut encoded)
        .write_message(&message)
        .expect("encode oversized fixture");

    let error = HarnessInputReader::new(std::io::Cursor::new(encoded))
        .read_message()
        .expect_err("oversized frame must fail");
    assert!(error.to_string().contains("protocol message exceeds"));
}

/// Locks the navigation request wire spelling, including explicit active-auto.
#[test]
fn agent_navigation_mode_request_round_trips_with_snake_case_actions() {
    for (action, spelling) in [
        (UiAgentNavigationModeAction::SetActive, "set_active"),
        (
            UiAgentNavigationModeAction::SetActiveAuto,
            "set_active_auto",
        ),
        (UiAgentNavigationModeAction::SetSuspended, "set_suspended"),
    ] {
        let event = Event::UiSetAgentNavigationMode(UiSetAgentNavigationMode {
            request_id: "navigation-1".to_owned(),
            session_id: test_session_id("session-1"),
            agent_id: AgentId::parse("agent-1").expect("valid agent id"),
            action,
        });
        let value = serde_json::to_value(&event).expect("serialize navigation request");
        assert_eq!(value["event"], "ui.set_agent_navigation_mode");
        assert_eq!(value["payload"]["action"], spelling);
        assert!(!event.defaults_to_persist());
        assert_eq!(
            serde_json::from_value::<Event>(value).expect("round trip"),
            event
        );
    }
}

/// Locks requester-result applied/rejection wire spellings and default
/// persistence.
#[test]
fn agent_navigation_mode_results_round_trip() {
    for outcome in [
        UiSetAgentNavigationModeOutcome::Applied,
        UiSetAgentNavigationModeOutcome::Rejected {
            reason: UiSetAgentNavigationModeRejection::StaleSession,
        },
        UiSetAgentNavigationModeOutcome::Rejected {
            reason: UiSetAgentNavigationModeRejection::AgentNotLoaded,
        },
    ] {
        let event = Event::UiSetAgentNavigationModeResult(UiSetAgentNavigationModeResult {
            request_id: "navigation-result".to_owned(),
            session_id: test_session_id("session-1"),
            agent_id: AgentId::parse("agent-1").expect("valid agent id"),
            outcome,
        });
        let value = serde_json::to_value(&event).expect("serialize result");
        assert_eq!(value["event"], "ui.set_agent_navigation_mode_result");
        assert!(!event.defaults_to_persist());
        assert_eq!(
            serde_json::from_value::<Event>(value).expect("round trip"),
            event
        );
    }
}

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> crate::ExtensionName {
    crate::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}
