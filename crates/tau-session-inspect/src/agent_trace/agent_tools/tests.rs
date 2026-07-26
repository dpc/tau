//! Focused compact argument projection tests.

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

    let projected = faithful_arguments(&arguments);

    assert_eq!(projected["type"], "map");
    assert_eq!(projected["value"].as_array().expect("map entries").len(), 3);
    assert_eq!(projected["value"][0]["value"]["type"], "float64_bits");
    assert_eq!(projected["value"][1]["value"]["type"], "bytes");
    assert_eq!(projected["value"][2]["value"]["type"], "tag");
}
