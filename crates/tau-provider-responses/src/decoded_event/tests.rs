use serde::Deserialize;
use serde_json::value::RawValue;

use super::{DecodedEvent, RAW_INDEX_SCANS, SEMANTIC_DECODES};

/// The lexical index must return the same byte-exact item as the former
/// borrowed `RawValue` projection across ordering, whitespace, and nesting.
#[test]
fn raw_item_index_matches_raw_value_oracle() {
    #[derive(Deserialize)]
    struct Oracle<'a> {
        #[serde(borrow)]
        item: Option<&'a RawValue>,
    }

    let items = [
        r#"{"type":"message","text":"plain"}"#,
        r#"{ "nested" : [1, {"escaped":"a\\\"b"}, true, null] }"#,
        r#"["array",{"deep":[[[]]]},1.2300]"#,
    ];
    for (index, item) in items.into_iter().enumerate() {
        for raw in [
            format!(r#"{{"item":{item},"type":"event","n":{index}}}"#),
            format!("{{ \n \"type\":\"event\", \"item\" : {item} \t}}"),
            format!(r#"{{"n":{index},"\u0069tem":{item},"type":"event"}}"#),
        ] {
            let decoded = DecodedEvent::decode(&raw).expect("valid event");
            let oracle: Oracle<'_> = serde_json::from_str(&raw).expect("oracle");
            assert_eq!(decoded.raw_item(), oracle.item.map(RawValue::get));
        }
    }
}

/// Terminal output indexing must preserve every element's exact spelling,
/// including internal whitespace and number formatting.
#[test]
fn terminal_output_elements_remain_exact() {
    let raw = r#"{"type":"response.completed","response":{"output":[ {"n":1.2300},["x", true],{"s":"a\\\"b"} ]}}"#;
    let decoded = DecodedEvent::decode(raw).expect("valid terminal");
    assert_eq!(
        decoded.raw_output_items().expect("array"),
        Some(vec![
            r#"{"n":1.2300}"#,
            r#"["x", true]"#,
            r#"{"s":"a\\\"b"}"#
        ])
    );
}

/// Malformed and truncated inputs must fail before the raw index is built.
#[test]
fn malformed_inputs_do_not_reach_raw_index() {
    for raw in ["", "{", r#"{"item":"#, r#"{"item":[1,2}"#] {
        assert!(DecodedEvent::decode(raw).is_err());
    }
}

/// Duplicate sidecar members retain the former strict public Responses
/// behavior instead of selecting or synthesizing one spelling.
#[test]
fn duplicate_raw_members_are_rejected() {
    assert!(DecodedEvent::decode(r#"{"item":1,"item":2}"#).is_err());
    assert!(
        DecodedEvent::decode(r#"{"response":{"output":[]},"response":{"output":[]}}"#).is_err()
    );
    assert!(
        DecodedEvent::decode(r#"{"response":{"output":[],"\u006futput":[]},"output":[]}"#).is_err()
    );
    assert!(DecodedEvent::decode(r#"{"output":[],"\u006futput":[]}"#).is_err());
}

/// Root and nested response shapes retain the former derived-struct
/// validation rather than becoming silently unknown events.
#[test]
fn raw_projection_shape_validation_matches_legacy_structs() {
    for raw in ["null", "[]", r#""event""#, "1", "true"] {
        assert!(DecodedEvent::decode(raw).is_err(), "{raw}");
    }
    for response in ["[]", r#""response""#, "1", "true"] {
        let raw = format!(r#"{{"type":"unknown","response":{response}}}"#);
        assert!(DecodedEvent::decode(&raw).is_err(), "{raw}");
    }
    assert!(DecodedEvent::decode(r#"{"type":"unknown","response":null}"#).is_ok());
}

/// Nested terminal output takes the same precedence over top-level output
/// as the former `RawEvent` projection.
#[test]
fn nested_terminal_output_precedes_top_level_fallback() {
    let nested = DecodedEvent::decode(
        r#"{"response":{"output":[{"source":"nested"}]},"output":[{"source":"top"}]}"#,
    )
    .expect("event");
    assert_eq!(
        nested.raw_output_items().expect("array"),
        Some(vec![r#"{"source":"nested"}"#])
    );
    let top = DecodedEvent::decode(r#"{"output":[ {"source":"top"} ]}"#).expect("event");
    assert_eq!(
        top.raw_output_items().expect("array"),
        Some(vec![r#"{"source":"top"}"#])
    );
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
