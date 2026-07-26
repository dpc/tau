//! Focused compact argument projection tests.

use base64::Engine as _;

use super::*;

/// A nested non-JSON value forces one complete tagged-CBOR projection,
/// preserving non-finite floats, duplicate keys, bytes, and tags.
#[test]
fn arguments_use_complete_tagged_cbor_fallback() {
    let arguments = CborValue::Map(vec![
        (
            CborValue::Text("duplicate".into()),
            CborValue::Float(f64::NAN),
        ),
        (
            CborValue::Text("duplicate".into()),
            CborValue::Bytes(vec![1, 2]),
        ),
        (
            CborValue::Integer(1.into()),
            CborValue::Tag(42, Box::new(CborValue::Text("tagged".into()))),
        ),
    ]);

    let projected = ArgumentsProjection::from_cbor(&arguments);

    assert!(projected.is_tagged());
    let projected = projected.value();
    assert_eq!(projected["type"], "map");
    assert_eq!(projected["value"].as_array().expect("map entries").len(), 3);
    assert_eq!(projected["value"][0]["value"]["type"], "float64_bits");
    assert_eq!(projected["value"][1]["value"]["type"], "bytes");
    assert_eq!(projected["value"][2]["value"]["type"], "tag");
}

/// Every CBOR float uses tagged bits so JSONL and TOON reconstruction preserve
/// integer-looking values and negative zero exactly.
#[test]
fn finite_floats_use_exact_tagged_bits() {
    let projected = ArgumentsProjection::from_cbor(&CborValue::Array(vec![
        CborValue::Float(1.0),
        CborValue::Float(1e6),
        CborValue::Float(-0.0),
    ]));

    assert!(projected.is_tagged());
    let value = projected.value();
    assert_eq!(value["value"][0]["value"], "3ff0000000000000");
    assert_eq!(value["value"][1]["value"], "412e848000000000");
    assert_eq!(value["value"][2]["value"], "8000000000000000");
}

/// Unsafe payload controls use field-level Base64 framing, keeping the readable
/// TOON envelope terminal-safe while reconstructing every exceptional field.
#[test]
fn toon_control_payload_uses_lossless_field_base64() {
    let agent_id = AgentId::parse("agent-safe").expect("agent id");
    let call_id = ToolCallId::from("call-\0\u{1b}\u{85}");
    let tool = ToolName::new("shell_command");
    let record = CallRecord {
        record_type: "call",
        at_us: 1,
        agent_id: &agent_id,
        call_id: &call_id,
        tool: &tool,
        command: Some("printf '\\0\\x1b'\0\u{1b}".into()),
        arguments: ArgumentsProjection::Ordinary(
            serde_json::json!({"nested": ["safe", "\u{8}\u{85}"]}),
        ),
        status: Status::Ok,
        duration_us: Some(2),
        output: OutputProjection::Full {
            output: "body\u{c}\u{7f}".into(),
        },
    };

    let encoded = serde_toon::to_string(&record.toon_projection().expect("TOON projection"))
        .expect("TOON encode");
    assert!(
        !encoded
            .chars()
            .any(|character| character.is_control() && character != '\n'),
        "raw payload control in {encoded:?}"
    );
    let decoded: serde_json::Value = serde_toon::from_str(&encoded).expect("TOON decode");
    let reconstructed_call_id = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(
                decoded["call_id_base64"]
                    .as_str()
                    .expect("call-ID Base64 scalar"),
            )
            .expect("base64 call ID"),
    )
    .expect("UTF-8 call ID");
    let command = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(
                decoded["command_base64"]
                    .as_str()
                    .expect("command Base64 scalar"),
            )
            .expect("base64 command"),
    )
    .expect("UTF-8 command");
    let arguments: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(
                decoded["arguments_json_base64"]
                    .as_str()
                    .expect("arguments JSON Base64 scalar"),
            )
            .expect("base64 arguments"),
    )
    .expect("arguments JSON");
    let output = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(
                decoded["output_base64"]
                    .as_str()
                    .expect("output Base64 scalar"),
            )
            .expect("base64 output"),
    )
    .expect("UTF-8 output");

    assert_eq!(command, record.command.as_deref().expect("command"));
    assert_eq!(reconstructed_call_id, record.call_id.as_str());
    assert_eq!(arguments, *record.arguments.value());
    assert_eq!(output, "body\u{c}\u{7f}");
    assert_eq!(decoded["status"], "ok");
}

/// Direct readable TOON round-trips nested ordinary JSON, empty containers, and
/// every control/quote escape its scalar grammar promises.
#[test]
fn toon_direct_payload_round_trips_safe_sensitive_strings() {
    let agent_id = AgentId::parse("agent-safe").expect("agent id");
    let call_id = ToolCallId::from("call-safe");
    let tool = ToolName::new("ordinary");
    let arguments = serde_json::json!({
        "nested": {"array": ["line\ncarriage\rtab\tquote\"slash\\", [], {}]},
        "empty_array": [],
        "empty_object": {},
    });
    let record = CallRecord {
        record_type: "call",
        at_us: 3,
        agent_id: &agent_id,
        call_id: &call_id,
        tool: &tool,
        command: None,
        arguments: ArgumentsProjection::Ordinary(arguments.clone()),
        status: Status::Incomplete,
        duration_us: None,
        output: OutputProjection::Counts {
            output_bytes: 0,
            output_lines: 0,
        },
    };

    let encoded = serde_toon::to_string(&record.toon_projection().expect("TOON projection"))
        .expect("TOON encode");
    let decoded: serde_json::Value = serde_toon::from_str(&encoded).expect("TOON decode");

    assert_eq!(decoded["arguments"], arguments);
    assert_eq!(decoded["status"], "incomplete");
    assert!(decoded.get("arguments_json_base64").is_none());
}
