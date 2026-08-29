use serde::Deserialize;
use serde_json::value::RawValue;

use super::{DecodedEvent, RAW_INDEX_SCANS, SEMANTIC_DECODES};

/// The lexical index must remain byte-for-byte equivalent to the former
/// borrowed `RawValue` sidecar projection.
#[test]
fn raw_item_index_matches_raw_value_oracle() {
    #[derive(Deserialize)]
    struct Oracle<'a> {
        #[serde(borrow)]
        item: Option<&'a RawValue>,
    }

    for item in [
        r#"{"type":"reasoning","n":1.2300}"#,
        r#"{ "nested" : [1, {"escaped":"a\\\"b"}, null] }"#,
        r#"["unknown",{"deep":[[[]]]}]"#,
    ] {
        for raw in [
            format!(r#"{{"item":{item},"type":"response.output_item.done"}}"#),
            format!("{{\n\"type\":\"event\", \"item\" : {item}\t}}"),
            format!(r#"{{"\u0069tem":{item},"type":"event"}}"#),
        ] {
            let decoded = DecodedEvent::decode(&raw).expect("valid event");
            let oracle: Oracle<'_> = serde_json::from_str(&raw).expect("oracle");
            assert_eq!(decoded.raw_item(), oracle.item.map(RawValue::get));
        }
    }
}

/// Malformed and truncated frames fail semantic decoding and never produce
/// a borrowed sidecar.
#[test]
fn malformed_inputs_do_not_reach_raw_index() {
    for raw in ["", "{", r#"{"item":"#, r#"{"item":[1,2}"#] {
        assert!(DecodedEvent::decode(raw).is_err());
    }
}

/// Duplicate items preserve the previous best-effort sidecar behavior:
/// semantic `Value` keeps the last item while exact replay gets no item.
#[test]
fn duplicate_items_omit_raw_sidecar() {
    let decoded = DecodedEvent::decode(r#"{"item":{"n":1},"item":{"n":2}}"#)
        .expect("semantic JSON accepts duplicate keys");
    assert_eq!(decoded.value()["item"]["n"], 2);
    assert_eq!(decoded.raw_item(), None);
}

/// One envelope construction performs exactly one semantic decode and one
/// bounded lexical indexing operation.
#[test]
fn decode_and_scan_counters_advance_once() {
    let decodes = SEMANTIC_DECODES.get();
    let scans = RAW_INDEX_SCANS.get();
    DecodedEvent::decode(r#"{"type":"response.heartbeat"}"#).expect("event");
    assert_eq!(SEMANTIC_DECODES.get(), decodes + 1);
    assert_eq!(RAW_INDEX_SCANS.get(), scans + 1);
}
