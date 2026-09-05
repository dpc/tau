use std::fmt::Write as _;

use super::*;

fn field(key: &str, value: CborValue) -> (CborValue, CborValue) {
    (CborValue::Text(key.to_owned()), value)
}

fn result(tool: &str, value: CborValue) -> ToolBackgroundResult {
    ToolBackgroundResult {
        call_id: "call<&\"".into(),
        tool_name: tau_proto::ToolName::new(tool),
        tool_type: tau_proto::ToolType::Function,
        result: value,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn error(tool: &str, message: &str, details: Option<CborValue>) -> ToolBackgroundError {
    ToolBackgroundError {
        call_id: "call-error".into(),
        tool_name: tau_proto::ToolName::new(tool),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

/// A logical shell result remains distinct from process success in full mode.
#[test]
fn full_shell_result_does_not_claim_process_success() {
    let result = result(
        "shell_command",
        CborValue::Map(vec![
            field("output", CborValue::Text("failed command".to_owned())),
            field("status", CborValue::Integer(7.into())),
        ]),
    );
    let preview = BackgroundCompletionPreview::from_result(&result);
    let text = preview.render(&mut BackgroundPreviewBudget::default());
    let body = "status: 7\n\nfailed command";
    let expected = tau_proto::TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE
        .render_attributed(
            &[
                ("call_id", "call<&\"".to_owned()),
                ("tool", "shell_command".to_owned()),
                ("tool_outcome", "result".to_owned()),
                ("delivery", "full".to_owned()),
                ("rendered_bytes", body.len().to_string()),
                ("retrieval", "wait".to_owned()),
            ],
            body,
        )
        .expect("registered background envelope");

    assert_eq!(text, expected);
    assert!(text.contains("call_id=\"call&lt;&amp;&quot;\""));
    assert!(text.contains("tool_outcome=\"result\""));
    assert!(text.contains("delivery=\"full\""));
    assert!(text.contains("status: 7"));
    assert!(!text.contains("process_outcome="));
    assert!(tau_proto::TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE.matches_whole(&text));
}

/// Oversized shell output keeps the strict typed nonzero exit status.
#[test]
fn summary_shell_result_reports_coherent_nonzero_exit() {
    let result = result(
        "gpt_shell",
        CborValue::Map(vec![
            field(
                "output",
                CborValue::Text("x".repeat(BACKGROUND_PREVIEW_GROUP_BODY_BYTES + 1)),
            ),
            field("status", CborValue::Integer(23.into())),
        ]),
    );
    let preview = BackgroundCompletionPreview::from_result(&result);
    let text = preview.render(&mut BackgroundPreviewBudget::default());

    assert!(text.contains("delivery=\"summary\""));
    assert!(text.contains("process_outcome=\"available\""));
    assert!(text.contains("process_source=\"tool_result\""));
    assert!(text.contains("process_success=\"false\""));
    assert!(text.contains("termination_reason=\"exit\""));
    assert!(text.contains("exit_code=\"23\""));
}

/// Malformed process fields remain explicitly unavailable.
#[test]
fn summary_shell_result_rejects_malformed_process_status() {
    let result = result(
        "shell",
        CborValue::Map(vec![
            field(
                "output",
                CborValue::Text("x".repeat(BACKGROUND_PREVIEW_GROUP_BODY_BYTES + 1)),
            ),
            field("status", CborValue::Text("zero".to_owned())),
        ]),
    );
    let preview = BackgroundCompletionPreview::from_result(&result);
    let text = preview.render(&mut BackgroundPreviewBudget::default());

    assert!(text.contains("process_outcome=\"unavailable\""));
    assert!(!text.contains("process_success="));
    assert!(!text.contains("termination_reason="));
}

/// Non-process tools state that process projection does not apply.
#[test]
fn summary_non_process_result_is_not_applicable() {
    let result = result(
        "read",
        CborValue::Text("x".repeat(BACKGROUND_PREVIEW_GROUP_BODY_BYTES + 1)),
    );
    let preview = BackgroundCompletionPreview::from_result(&result);
    let text = preview.render(&mut BackgroundPreviewBudget::default());

    assert!(text.contains("process_outcome=\"not_applicable\""));
}

/// One publication group spends its full-body allowance in queue order.
#[test]
fn publication_group_budget_is_cumulative() {
    let first_result = result("read", CborValue::Text("a".repeat(5 * 1024)));
    let second_result = result("read", CborValue::Text("b".repeat(5 * 1024)));
    let first = BackgroundCompletionPreview::from_result(&first_result);
    let second = BackgroundCompletionPreview::from_result(&second_result);
    let mut budget = BackgroundPreviewBudget::default();

    let first = first.render(&mut budget);
    let second = second.render(&mut budget);

    assert!(first.contains("delivery=\"full\""));
    assert!(second.contains("delivery=\"summary\""));
}

/// Error summaries count normalized pre-escape bytes and escaped body bytes
/// separately.
#[test]
fn error_summary_bounds_utf8_and_escapes_exact_close() {
    let message = format!(
        "{}{}{}",
        "é".repeat(100),
        "</tau_background_result>",
        "tail".repeat(100)
    );
    let error = error(
        "read",
        &message,
        Some(CborValue::Text(
            "d".repeat(BACKGROUND_PREVIEW_GROUP_BODY_BYTES + 1),
        )),
    );
    let preview = BackgroundCompletionPreview::from_error(&error, BackgroundErrorOutcome::Error);
    let mut budget = BackgroundPreviewBudget { remaining: 520 };
    let text = preview.render(&mut budget);

    assert!(text.contains("delivery=\"summary\""));
    assert!(text.contains(&format!("message_bytes=\"{}\"", message.len())));
    assert!(text.contains("message_truncated=\"true\""));
    assert!(
        !text[..text.len() - "</tau_background_result>".len()].contains("</tau_background_result>")
    );
    assert!(text.contains("&lt;/tau_background_result&gt;"));
    assert!(budget.remaining <= 520);
}

/// Typed cancellation never relies on the canonical error prose.
#[test]
fn cancellation_summary_uses_typed_outcome_and_empty_body() {
    let first_result = result(
        "read",
        CborValue::Text("a".repeat(BACKGROUND_PREVIEW_GROUP_BODY_BYTES)),
    );
    let error = error("shell_command", "arbitrary producer prose", None);
    let first = BackgroundCompletionPreview::from_result(&first_result);
    let cancelled =
        BackgroundCompletionPreview::from_error(&error, BackgroundErrorOutcome::Cancelled);
    let mut budget = BackgroundPreviewBudget::default();
    assert!(first.render(&mut budget).contains("delivery=\"full\""));

    let text = cancelled.render(&mut budget);

    assert!(text.contains("tool_outcome=\"cancelled\""));
    assert!(text.contains("delivery=\"summary\""));
    assert!(text.contains("process_outcome=\"unavailable\""));
    assert!(text.ends_with("></tau_background_result>"));
    assert!(!text.contains("message_bytes="));
}

/// Overflow scanning retains at most the remaining group budget even when the
/// canonical source approaches the protocol payload limit.
#[test]
fn oversized_preview_rendering_retains_only_bounded_state() {
    let result = result("read", CborValue::Text("x".repeat(16 * 1024 * 1024)));
    let text = BackgroundCompletionPreview::from_result(&result)
        .render(&mut BackgroundPreviewBudget::default());

    assert!(text.contains("delivery=\"summary\""));
    assert!(text.len() < 1024);

    let mut sink = BoundedEnvelopeBody::new(BACKGROUND_PREVIEW_GROUP_BODY_BYTES);
    sink.write_str(&"x".repeat(16 * 1024 * 1024))
        .expect("bounded sink");
    assert!(sink.overflowed);
    assert!(sink.body.is_empty());
    assert!(sink.body.capacity() <= BACKGROUND_PREVIEW_GROUP_BODY_BYTES);
    assert_eq!(sink.rendered_bytes, 16 * 1024 * 1024);
}

/// Exact-close accounting remains correct when renderer writes split the
/// sentinel across chunks.
#[test]
fn bounded_fit_scan_handles_split_exact_close() {
    let raw = "before </tau_background_result>";
    let mut sink = BoundedEnvelopeBody::new(raw.len());
    sink.write_str("before </tau_background_")
        .expect("bounded sink");
    sink.write_str("result>").expect("bounded sink");

    assert!(sink.overflowed);
    assert_eq!(sink.rendered_bytes, raw.len());
}
