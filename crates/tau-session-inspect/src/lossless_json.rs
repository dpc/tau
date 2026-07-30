//! Lossless JSON projection for CBOR-backed durable protocol payloads.

#[cfg(test)]
mod tests;
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
