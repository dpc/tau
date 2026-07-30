use super::*;

/// The native representation must distinguish every CBOR container and
/// scalar domain that ordinary JSON would conflate or reject.
#[test]
fn tagged_json_preserves_cbor_edge_types() {
    let value = CborValue::Map(vec![
        (
            CborValue::Bytes(vec![0, 255]),
            CborValue::Array(vec![
                CborValue::Tag(42, Box::new(CborValue::Text("tagged".to_owned()))),
                CborValue::Integer(u64::MAX.into()),
                CborValue::Float(f64::from_bits(0x7ff8_0000_0000_0042)),
                CborValue::Float(f64::INFINITY),
            ]),
        ),
        (CborValue::Integer((-1).into()), CborValue::Null),
    ]);

    let json = typed_cbor(&value);
    let encoded = serde_json::to_string(&json).expect("valid tagged JSON");

    assert!(encoded.contains("\"type\":\"bytes\""));
    assert!(encoded.contains("\"type\":\"tag\""));
    assert!(encoded.contains(&u64::MAX.to_string()));
    assert!(encoded.contains("7ff8000000000042"));
    assert!(encoded.contains("7ff0000000000000"));
    assert!(encoded.contains("\"key\":{\"type\":\"integer\",\"value\":\"-1\"}"));
}

/// A real typed tool terminal preserves its unrestricted CBOR result
/// through the same event projection used by native and OTLP raw
/// occurrences.
#[test]
fn typed_tool_event_uses_lossless_cbor_projection() {
    let event = Event::ProviderToolResult(tau_proto::ToolResult {
        call_id: "call-lossless".into(),
        tool_name: tau_proto::ToolName::new("lossless_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![(
            CborValue::Bytes(vec![1, 2, 3]),
            CborValue::Float(f64::NAN),
        )]),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: std::sync::Arc::from([0_u8]),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    });

    let projected = event_json(&event).expect("lossless typed event");
    let encoded = serde_json::to_string(&projected).expect("event JSON");

    assert!(encoded.contains("\"type\":\"bytes\""));
    assert!(encoded.contains("7ff8000000000000"));
    assert!(encoded.contains("AA=="));
}
