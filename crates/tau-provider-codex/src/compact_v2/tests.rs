use tau_proto::{ContextBlock, OpaqueProviderItem, UserInputBlock};

use super::*;

fn message(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn context(items: Vec<ContextItem>) -> PromptContext {
    PromptContext {
        blocks: vec![ContextBlock::UserInput(UserInputBlock { items })],
    }
}

fn compact_item() -> ContextItem {
    ContextItem::Compaction(
        OpaqueProviderItem::from_raw_json(r#"{"type":"compaction"}"#)
            .expect("valid compaction item"),
    )
}

fn watch_prompt(body_bytes: usize) -> ContextItem {
    let prefix = "<tau_internal>Watched agent worker received a user prompt\n\n<prompt>\n";
    let suffix = "\n</prompt>&lt;/tau_internal&gt;";
    message(format!("{prefix}{}{suffix}", "x".repeat(body_bytes)))
}

/// The v2 replacement keeps eligible input order and appends the provider
/// compaction item last while dropping non-message transcript mechanics.
#[test]
fn replacement_retains_user_messages_and_appends_compaction() {
    let input = context(vec![
        message("old"),
        ContextItem::CompactionTrigger,
        message("new"),
    ]);

    let output = build_v2_compacted_window(&input, vec![compact_item()]);

    assert_eq!(output, vec![message("old"), message("new"), compact_item()]);
}

/// An oversized agent-message envelope is omitted before aggregate budgeting
/// at the exact 40,000-byte per-message boundary.
#[test]
fn oversized_agent_message_is_not_retained() {
    let input = context(vec![
        message(format!(
            "<tau_internal>You received a peer message\n\n<message>{}</message></tau_internal>",
            "x".repeat(40_001)
        )),
        message("recent"),
    ]);

    let output = build_v2_compacted_window(&input, vec![compact_item()]);

    assert_eq!(output, vec![message("recent"), compact_item()]);
}

/// The production replacement builder must apply the existing per-agent-message
/// cap to canonical watch-prompt input before cloning it into the candidate
/// set. Equality remains accepted, while one additional UTF-8 byte omits the
/// prompt.
#[test]
fn watch_prompt_obeys_exact_agent_message_byte_cap() {
    let wrapper_bytes = match watch_prompt(0) {
        ContextItem::Message(message) => message_bytes(&message),
        _ => unreachable!("watch prompt helper always builds a message"),
    };
    let exact = watch_prompt(MAX_RETAINED_AGENT_MESSAGE_BYTES - wrapper_bytes);
    let oversized = watch_prompt(MAX_RETAINED_AGENT_MESSAGE_BYTES - wrapper_bytes + 1);

    assert_eq!(
        build_v2_compacted_window(&context(vec![exact.clone()]), vec![compact_item()]),
        vec![exact, compact_item()]
    );
    assert_eq!(
        build_v2_compacted_window(&context(vec![oversized]), vec![compact_item()]),
        vec![compact_item()]
    );
}

/// A harness-internal message cannot enter the WatchPrompt limit domain merely
/// by containing the wrapper's inner wording. It remains eligible under the
/// separate aggregate retained-message budget.
#[test]
fn unrelated_internal_message_does_not_match_watch_prompt_cap() {
    let unrelated = message(format!(
        "<tau_internal>Notice: worker received a user prompt\n\n<prompt>\n{}\
         \n</prompt>&lt;/tau_internal&gt;",
        "x".repeat(MAX_RETAINED_AGENT_MESSAGE_BYTES)
    ));

    assert_eq!(
        build_v2_compacted_window(&context(vec![unrelated.clone()]), vec![compact_item()]),
        vec![unrelated, compact_item()]
    );
}

/// Aggregate overflow keeps the newest window and middle-truncates exactly
/// one boundary message with a byte-labeled marker.
#[test]
fn aggregate_budget_truncates_oldest_boundary_message() {
    let input = context(vec![
        message("a".repeat(200_000)),
        message("b".repeat(120_000)),
    ]);

    let output = build_v2_compacted_window(&input, vec![compact_item()]);

    let ContextItem::Message(oldest) = &output[0] else {
        panic!("retained boundary message");
    };
    let ContentPart::Text { text } = &oldest.content[0] else {
        panic!("text boundary");
    };
    assert!(text.contains(" bytes truncated…"));
    assert!(text.starts_with('a') && text.ends_with('a'));
    assert_eq!(output[1], message("b".repeat(120_000)));
    assert_eq!(output[2], compact_item());
}

/// A boundary that cannot fit its byte marker still closes the contiguous
/// newest-first retention window; older tiny messages must not leak through.
#[test]
fn unmarkable_overflow_boundary_drops_all_older_messages() {
    let input = context(vec![
        message("older"),
        message("x".repeat(100)),
        message("n".repeat(255_990)),
    ]);

    let output = build_v2_compacted_window(&input, vec![compact_item()]);

    assert_eq!(output[0], message("n".repeat(255_990)));
    assert_eq!(output[1], compact_item());
    assert_eq!(output.len(), 2);
}

/// Empty messages consume exactly zero bytes rather than a fabricated unit.
#[test]
fn empty_message_does_not_reduce_exact_aggregate_budget() {
    let input = context(vec![message("x".repeat(256_000)), message("")]);
    assert_eq!(
        build_v2_compacted_window(&input, vec![compact_item()]),
        vec![message("x".repeat(256_000)), message(""), compact_item()]
    );
}

/// Aggregate accounting uses UTF-8 bytes rather than scalar or token counts.
#[test]
fn non_ascii_message_fits_at_exact_utf8_byte_cap() {
    let exact = "é".repeat(128_000);
    assert_eq!(
        build_v2_compacted_window(&context(vec![message(&exact)]), vec![compact_item()]),
        vec![message(exact), compact_item()]
    );
}
