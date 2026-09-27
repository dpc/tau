//! Credential-free xAI compact fixture and malformed-output oracles.

use super::*;

/// Exactly one opaque item retains provider JSON syntax and usage.
#[test]
fn compact_response_preserves_one_opaque_item_and_usage() {
    let raw = r#"{"object":"response.compaction","id":"cmp_1","output":[{"type":"compaction","id":"cmp_1","encrypted_content":"sealed","extra\u005fkey":1.2300}],"usage":{"input_tokens":12000,"input_tokens_details":{"cached_tokens":300},"output_tokens":800}}"#;
    let success = parse(raw.as_bytes()).expect("valid compact response");
    let ContextItem::Compaction(item) = success.output else {
        panic!("opaque compaction item");
    };
    assert!(item.raw_json().contains(r#""extra\u005fkey":1.2300"#));
    assert_eq!(success.response_id.as_deref(), Some("cmp_1"));
    let usage = success.usage.expect("usage");
    assert_eq!(usage.prompt_sent_tokens, 12000);
    assert_eq!(usage.prompt_cached_tokens, 300);
    assert_eq!(usage.response_received_tokens, 800);
}

/// Never install partial, mixed, missing-ciphertext, or non-compaction output.
#[test]
fn compact_response_rejects_invalid_replacements() {
    for output in [
        r#"[]"#,
        r#"[{"type":"message","id":"msg_1"}]"#,
        r#"[{"type":"compaction","id":"cmp_1"}]"#,
        r#"[{"type":"compaction","id":"cmp_1","encrypted_content":"x"},{"type":"message"}]"#,
    ] {
        let response =
            format!(r#"{{"object":"response.compaction","id":"cmp_1","output":{output}}}"#);
        assert!(parse(response.as_bytes()).is_err(), "{output}");
    }
    assert!(parse(br#"{"object":"response.compaction","id":"cmp_other","output":[{"type":"compaction","id":"cmp_1","encrypted_content":"x"}]}"#).is_err());
    assert!(parse(br#"{"object":"response","output":[{"type":"compaction","id":"cmp_1","encrypted_content":"x"}]}"#).is_err());
}

/// A stalled HTTP phase observes cancellation instead of waiting for the
/// five-minute absolute deadline or keeping provider output alive.
#[test]
fn stalled_unary_receive_observes_cancellation() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut checks = 0;
    let result = runtime.block_on(wait(
        std::future::pending::<()>(),
        Instant::now() + Duration::from_secs(5),
        &mut || {
            checks += 1;
            true
        },
    ));
    assert!(result.is_err());
    assert_eq!(checks, 1);
}

/// Final cancellation after a fully parsed provider item still discards the
/// output; the wire may be paid, but the transaction cannot install it.
#[test]
fn final_cancellation_wins_after_successful_parse() {
    let success = parse(br#"{"object":"response.compaction","id":"cmp_1","output":[{"type":"compaction","id":"cmp_1","encrypted_content":"sealed"}]}"#)
        .expect("valid item");
    assert!(matches!(
        classify(Ok(success), true, true),
        Outcome::Canceled
    ));
}
