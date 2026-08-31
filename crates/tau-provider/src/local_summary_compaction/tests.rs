use super::*;

/// Generic fallback derives only a same-domain token output cap and publishes
/// no fabricated prefix byte cap.
#[test]
fn defaults_publish_no_cross_unit_limits() {
    let config = Config::default_for(128_000).expect("ordinary model context");
    assert_eq!(config.max_output_tokens(), 4096);
    assert_eq!(config.max_input_bytes(), None);
    assert!(Config::default_for(0).is_none());
}

/// The shared instruction must identify harness authority and forbid tools
/// without replacing the ordinary system prompt.
#[test]
fn request_is_the_cache_aligned_trailing_user_instruction() {
    assert!(REQUEST.starts_with("<tau_internal>\n"));
    assert!(REQUEST.ends_with("\n&lt;/tau_internal&gt;"));
    assert!(REQUEST.contains("Do not make or request any tool calls."));
    assert!(REQUEST.contains("Return only the summary."));

    let independent_byte_cap = Config::new(
        NonZeroU64::new(2048).expect("positive"),
        2048,
        NonZeroU64::new(255).expect("positive"),
        NonZeroU32::new(1).expect("positive"),
        NonZeroU64::new(1).expect("positive"),
    );
    assert_eq!(
        independent_byte_cap
            .expect("byte work cap is independent of token capacity")
            .max_input_bytes(),
        Some(tau_proto::ByteCount::new(255))
    );
}

/// Bounded prefix measurement must preserve the canonical JSON byte boundary
/// while avoiding a cloned context or materialized serialized buffer.
#[test]
fn historical_prefix_budget_accepts_exact_bytes_and_rejects_the_next_byte() {
    let context = tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
                    role: tau_proto::ContextRole::User,
                    content: vec![tau_proto::ContentPart::Text {
                        text: "bounded prefix".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![tau_proto::ContextItem::CompactionTrigger],
            }),
        ],
    };
    validate_trailing_trigger(&context).expect("exact trailing trigger");
    let exact = historical_prefix_json_bytes(&context).expect("canonical prefix bytes");

    assert_eq!(
        historical_prefix_fits_json_budget(&context, exact),
        Some(true)
    );
    assert_eq!(
        historical_prefix_fits_json_budget(&context, tau_proto::ByteCount::new(exact.get() - 1)),
        Some(false)
    );
}

/// Budget accounting must retain byte-domain units across zero and chunked
/// serializer writes, accepting the final byte exactly at the selected
/// boundary.
#[test]
fn json_budget_writer_accepts_zero_and_chunked_writes_at_exact_boundary() {
    let mut writer = JsonBudgetWriter::new(tau_proto::ByteCount::new(3));

    assert_eq!(writer.write(b"").expect("zero-sized write"), 0);
    assert_eq!(writer.write(b"a").expect("first chunk"), 1);
    assert_eq!(writer.write(b"bc").expect("final chunk"), 2);
    assert_eq!(writer.remaining, tau_proto::ByteCount::ZERO);
    assert!(!writer.exceeded);
}

/// A write that would cross the boundary must not consume a partial chunk and
/// must preserve the historical budget-exhaustion diagnostic.
#[test]
fn json_budget_writer_rejects_overflow_without_consuming_partial_chunk() {
    let mut writer = JsonBudgetWriter::new(tau_proto::ByteCount::new(2));

    assert_eq!(writer.write(b"a").expect("first chunk"), 1);
    let error = writer
        .write(b"bc")
        .expect_err("chunk crosses remaining budget");

    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), "historical prompt prefix exceeds budget");
    assert_eq!(writer.remaining, tau_proto::ByteCount::new(1));
    assert!(writer.exceeded);
}
