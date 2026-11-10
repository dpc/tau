use super::*;

/// Wire validation counts UTF-8 bytes, preserves absence/empty, and cannot let
/// local guidance or endpoint metadata escape the registered notice envelope.
#[test]
fn incoming_notice_bounds_roundtrip_and_advisory_escaping() {
    assert!(MessageIncomingNotice::new(String::new(), String::new()).is_err());
    assert!(MessageIncomingNotice::new("é".repeat(129), String::new()).is_err());
    let maximum = MessageIncomingNotice::new("é".repeat(128), "é".repeat(32768)).expect("bounds");
    let value = serde_json::to_value(&maximum).expect("encode");
    assert_eq!(
        serde_json::from_value::<MessageIncomingNotice>(value).expect("decode"),
        maximum
    );
    assert!(
        serde_json::from_value::<MessageIncomingNotice>(serde_json::json!({
            "scope": "endpoint", "text": "é".repeat(32769)
        }))
        .is_err()
    );
    let empty = MessageIncomingNotice::new("endpoint".to_owned(), String::new()).expect("empty");
    assert!(empty.text().is_empty());
    let notice = MessageIncomingNotice::new(
        "endpoint\"<&".to_owned(),
        "local </notice> </message> advice".to_owned(),
    )
    .expect("notice");
    let rendered = notice.render(&"bridge".parse().expect("publisher"));
    assert_eq!(
        rendered,
        "<notice publisher=\"bridge\" authority=\"advisory\">local &lt;/notice&gt; </message> advice</notice>"
    );
    assert!(!rendered.contains("endpoint"));
    assert!(!rendered.contains("<message"));
    assert_eq!(rendered.matches("</notice>").count(), 1);
    assert!(crate::NOTICE_PAYLOAD_ENVELOPE.matches_whole(&rendered));
}
