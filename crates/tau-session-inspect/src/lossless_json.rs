//! Lossless JSON projection for CBOR-backed durable protocol payloads.

use base64::Engine as _;
use serde_json::{Value, json};
use tau_proto::{CborValue, Event};

use crate::InspectError;

/// Converts a typed event into its name and an explicitly typed, lossless CBOR
/// payload representation.
pub(super) fn event_json(event: &Event) -> Result<Value, InspectError> {
    let encoded = CborValue::serialized(event).map_err(|error| {
        InspectError::Trace(crate::AgentTraceError::Projection(format!(
            "failed to encode event payload: {error}"
        )))
    })?;
    let CborValue::Map(fields) = encoded else {
        return Err(InspectError::Trace(crate::AgentTraceError::Projection(
            "typed event did not encode as a CBOR map".to_owned(),
        )));
    };
    let field = |name: &str| {
        fields.iter().find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == name).then_some(value)
        })
    };
    let event_name = match field("event") {
        Some(CborValue::Text(name)) => name.clone(),
        _ => {
            return Err(InspectError::Trace(crate::AgentTraceError::Projection(
                "typed event encoding omitted its event name".to_owned(),
            )));
        }
    };
    let payload = field("payload").map_or_else(|| typed_cbor(&CborValue::Null), typed_cbor);
    Ok(json!({ "event": event_name, "payload": payload }))
}

/// Converts every CBOR variant to a tagged JSON form without conflating types
/// or relying on JSON's restricted number and object-key domains.
pub(super) fn typed_cbor(value: &CborValue) -> Value {
    match value {
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            json!({ "type": "integer", "value": value.to_string() })
        }
        CborValue::Bytes(value) => json!({
            "type": "bytes",
            "encoding": "base64",
            "value": base64::engine::general_purpose::STANDARD.encode(value),
        }),
        CborValue::Float(value) => json!({
            "type": "float64_bits",
            "value": format!("{:016x}", value.to_bits()),
        }),
        CborValue::Text(value) => json!({ "type": "text", "value": value }),
        CborValue::Bool(value) => json!({ "type": "bool", "value": value }),
        CborValue::Null => json!({ "type": "null" }),
        CborValue::Tag(tag, value) => json!({
            "type": "tag",
            "tag": tag.to_string(),
            "value": typed_cbor(value),
        }),
        CborValue::Array(values) => json!({
            "type": "array",
            "value": values.iter().map(typed_cbor).collect::<Vec<_>>(),
        }),
        CborValue::Map(entries) => json!({
            "type": "map",
            "value": entries
                .iter()
                .map(|(key, value)| json!({
                    "key": typed_cbor(key),
                    "value": typed_cbor(value),
                }))
                .collect::<Vec<_>>(),
        }),
        _ => unreachable!("ciborium Value is non-exhaustive"),
    }
}

#[cfg(test)]
mod tests {
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
}
