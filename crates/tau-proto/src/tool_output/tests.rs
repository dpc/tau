use super::*;

/// Captured native text uses the existing root framing, including header-only
/// no-final-LF, without dropping available cancellation tails or adding status
/// twice at provider lowering.
#[test]
fn native_capture_preserves_framing_and_status_once() {
    let header = CborValue::Map(vec![(
        CborValue::Text("status".into()),
        CborValue::Integer(0.into()),
    )]);
    let capture =
        ToolFinalOutput::capture_native(&header, crate::ProviderToolResultStatus::Success);
    assert_eq!(
        crate::ToolResponse::from_captured_output(&[], &capture).render(),
        "status: 0"
    );
    for value in [
        CborValue::Null,
        CborValue::Text(String::new()),
        CborValue::Text("tail\n".into()),
    ] {
        let capture = ToolFinalOutput::capture_native(
            &value,
            crate::ProviderToolResultStatus::Cancelled { reason: "stopped" },
        );
        let item = crate::ToolResultItem {
            call_id: "call".into(),
            tool_type: crate::ToolType::Function,
            status: crate::ToolResultStatus::Cancelled {
                reason: "stopped".into(),
            },
            output: crate::ToolResponse::from_captured_output(&[], &capture),
            presentation: crate::ToolResultPresentation::NativeCapturedOutput,
            provider_content: Vec::new(),
        };
        let expected = if value == CborValue::Text("tail\n".into()) {
            "cancelled: stopped\n\ntail\n"
        } else {
            "cancelled: stopped"
        };
        assert_eq!(item.render_provider_text(), expected);
    }
}

/// Explicit canonical batch boundaries keep partial lines and omission facts
/// separate. Composition retains the full incremental allowance rather than
/// applying the smaller final cap a second time.
#[test]
fn native_composition_retains_batches_and_separate_final_allowance() {
    let updates = (0..8)
        .map(|sequence| ToolOutputUpdate {
            call: ToolCallRef {
                declaration: crate::ObservationId::from_bytes([1; 16]),
                item_index: 0,
            },
            output: ToolOutputReport {
                call_id: "call".into(),
                tool_name: ToolName::new("tool"),
                sequence,
                text: "λ".repeat(MAX_TOOL_OUTPUT_CHUNK_BYTES / 2),
                omitted_bytes: 7,
            },
        })
        .collect::<Vec<_>>();
    let capture = ToolFinalOutput::capture_native(
        &CborValue::Text(format!("final-prefix{}final-tail", "x".repeat(100_000))),
        crate::ProviderToolResultStatus::Error { message: "failed" },
    );
    assert!(capture.text.len() <= MAX_TOOL_OUTPUT_FINAL_BYTES);
    assert!(capture.omitted_bytes > 0);
    let response = crate::ToolResponse::from_captured_output(&updates, &capture);
    let rendered = response.render();
    assert_eq!(
        rendered.matches('λ').count(),
        MAX_TOOL_OUTPUT_LIVE_BYTES / 2
    );
    assert_eq!(rendered.matches("omitted_bytes: 7\n").count(), 8);
    assert_eq!(rendered.matches("error: failed").count(), 1);
    assert!(rendered.ends_with("final-tail"));
    assert!(rendered.contains("final-prefix"));
    assert!(rendered.contains("final_output: true\nomitted_bytes:"));
    assert!(rendered.len() > MAX_TOOL_OUTPUT_LIVE_BYTES);
}
