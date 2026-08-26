use std::sync::{Arc, mpsc};

use tau_proto::{
    CborValue, DiffHunk, DiffLine, DiffSummary, Event, FileDiffSummary, ImageContent, ImageDetail,
    ImageMediaType, ToolCallId, ToolError, ToolName, ToolResult, ToolResultContentPart,
    ToolResultKind, ToolType, ToolUsePayload, ToolUseState, ToolUseStatus,
};

use super::{IMAGE_TOO_LARGE_MESSAGE, budget_terminal_report, frame_fits};
use crate::Output;

/// Return the reported event from the measured terminal envelope.
fn budgeted_event(event: Event) -> Event {
    let message = budget_terminal_report(event).expect("budget terminal");
    let tau_proto::HarnessInputMessage::Emit(emit) = message else {
        panic!("expected terminal emit");
    };
    *emit.event
}

/// Return the authoritative explicit marker used for a truncated diff.
fn diff_marker() -> String {
    format!(
        "[diff truncated: complete terminal frame would exceed {} bytes]",
        tau_client::MAX_OUTBOUND_FRAME_BYTES
    )
}

/// Builds a representative successful reported terminal around caller-selected
/// provider and display content.
fn result_event(
    result: CborValue,
    provider_content: Vec<ToolResultContentPart>,
    display: ToolUseState,
) -> Event {
    Event::ToolResultReported(ToolResult {
        presentation: Default::default(),
        call_id: ToolCallId::new("call"),
        tool_name: ToolName::new("edit"),
        tool_type: ToolType::Function,
        result,
        provider_content,
        kind: ToolResultKind::Final,
        display: Some(display),
        originator: Default::default(),
    })
}

/// Finds image data whose complete terminal envelope has the requested encoded
/// size; above CBOR's length-header threshold each added byte grows the frame
/// by exactly one byte.
fn image_event_with_frame_size(target: u64) -> Event {
    let mut data_len = usize::try_from(target).expect("test target fits usize");
    loop {
        let event = result_event(
            CborValue::Text("image metadata".to_owned()),
            vec![ToolResultContentPart::Image(ImageContent {
                media_type: ImageMediaType::Png,
                data: Arc::from(vec![0; data_len]),
                width: 1,
                height: 1,
                detail: ImageDetail::High,
            })],
            ToolUseState::default(),
        );
        let message = tau_proto::HarnessInputMessage::emit_with_persist(event.clone(), false);
        let measured =
            tau_client::encoded_outbound_frame_bytes(&message).expect("measure image frame");
        if measured == target {
            return event;
        }
        if measured < target {
            data_len += usize::try_from(target - measured).expect("positive adjustment");
        } else {
            data_len -= usize::try_from(measured - target).expect("negative adjustment");
        }
    }
}

/// Ensures producer admission uses the complete transient emit envelope at the
/// exact shared boundary rather than comparing only the image payload.
#[test]
fn complete_terminal_envelope_accepts_limit_and_rejects_next_byte() {
    let at_limit = image_event_with_frame_size(tau_client::MAX_OUTBOUND_FRAME_BYTES);
    assert!(frame_fits(&at_limit).expect("measure boundary frame"));

    let over_limit = image_event_with_frame_size(tau_client::MAX_OUTBOUND_FRAME_BYTES + 1);
    assert!(!frame_fits(&over_limit).expect("measure oversized frame"));
}

/// Ensures the real output adapter scopes the wire tool name before measuring
/// and accepts a complete reported envelope exactly at the shared limit.
#[test]
fn output_adapter_budgets_exact_frame_after_wire_name_scope() {
    let Event::ToolResultReported(mut result) =
        image_event_with_frame_size(tau_client::MAX_OUTBOUND_FRAME_BYTES)
    else {
        panic!("expected image result fixture");
    };
    result.tool_name = ToolName::new("replace");
    let (tx, rx) = mpsc::channel();
    let output = Output::channel(tx).scoped_tool(ToolName::new("replace"), ToolName::new("edit"));

    output
        .report_tool_terminal(Event::ToolResult(result))
        .expect("report exact-limit terminal");

    let tau_proto::HarnessInputMessage::Emit(emit) = rx.recv().expect("terminal message") else {
        panic!("expected terminal emit");
    };
    let Event::ToolResultReported(result) = *emit.event else {
        panic!("expected successful reported result");
    };
    assert_eq!(result.tool_name, ToolName::new("edit"));
    assert_eq!(result.provider_content.len(), 1);
}

/// Ensures an image that makes its complete envelope oversized becomes a clean
/// typed tool error and never falls back to base64 or generic text content.
#[test]
fn oversized_typed_image_becomes_byte_free_local_error() {
    let event = image_event_with_frame_size(tau_client::MAX_OUTBOUND_FRAME_BYTES + 1);
    let event = budgeted_event(event);
    let Event::ToolErrorReported(error) = event else {
        panic!("expected reported tool error");
    };
    assert_eq!(error.message, IMAGE_TOO_LARGE_MESSAGE);
    assert!(error.details.is_none());
    assert!(frame_fits(&Event::ToolErrorReported(error)).expect("measure local error"));
}

/// Builds one intentionally oversized structured diff while keeping the
/// agent-visible edit result and path evidence compact.
fn oversized_diff_payload(path: &str) -> ToolUsePayload {
    ToolUsePayload::Diffs {
        files: vec![FileDiffSummary {
            path: path.to_owned(),
            diff: oversized_diff_summary(),
        }],
    }
}

/// Builds the singular structured diff emitted by both `edit`
/// implementations.
fn oversized_diff_summary() -> DiffSummary {
    DiffSummary {
        added: 1,
        removed: 0,
        hunks: vec![DiffHunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Add {
                text: "x".repeat(
                    usize::try_from(tau_client::MAX_OUTBOUND_FRAME_BYTES)
                        .expect("frame limit fits usize"),
                ),
            }],
        }],
    }
}

/// Ensures both edit implementations retain their minimal result and all
/// non-payload display facts when a singular optional diff is oversized.
#[test]
fn oversized_singular_edit_diff_retains_minimal_result_and_display_facts() {
    let result_value = CborValue::Map(vec![(
        CborValue::Text("changed".to_owned()),
        CborValue::Bool(true),
    )]);
    let event = result_event(
        result_value.clone(),
        Vec::new(),
        ToolUseState {
            args: "src/large.rs 1..<2".to_owned(),
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            payload: Some(ToolUsePayload::Diff(oversized_diff_summary())),
            ..Default::default()
        },
    );

    let event = budgeted_event(event);
    let Event::ToolResultReported(result) = &event else {
        panic!("expected reported edit result");
    };
    let display = result.display.as_ref().expect("retained display");
    assert_eq!(result.result, result_value);
    assert_eq!(display.args, "src/large.rs 1..<2");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
    assert_eq!(
        display.payload,
        Some(ToolUsePayload::Text {
            text: diff_marker(),
        })
    );
    assert!(frame_fits(&event).expect("measure truncated edit"));
}

/// Ensures a successful side effect retains truthful minimal success and
/// changed-file evidence while only its optional UI diff is truncated.
#[test]
fn oversized_success_diff_retains_result_and_changed_path_evidence() {
    let summary = "Success. Updated the following files:\nM src/large.rs";
    let event = result_event(
        CborValue::Text(summary.to_owned()),
        Vec::new(),
        ToolUseState {
            args: "apply_patch".to_owned(),
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            payload: Some(oversized_diff_payload("src/large.rs")),
            ..Default::default()
        },
    );

    let event = budgeted_event(event);
    let Event::ToolResultReported(result) = &event else {
        panic!("expected reported tool result");
    };
    let display = result.display.as_ref().expect("retained display");
    assert_eq!(result.result, CborValue::Text(summary.to_owned()));
    assert_eq!(display.args, "apply_patch");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
    assert_eq!(
        display.payload.as_ref(),
        Some(&ToolUsePayload::Text {
            text: format!("{}\nChanged files:\nsrc/large.rs", diff_marker()),
        })
    );
    assert!(frame_fits(&event).expect("measure truncated success"));
}

/// Ensures a partially applied edit failure retains its structured changed-file
/// evidence while dropping only the oversized optional UI diff.
#[test]
fn oversized_failure_diff_retains_partial_change_details() {
    let details = CborValue::Map(vec![(
        CborValue::Text("changed_files".to_owned()),
        CborValue::Array(vec![CborValue::Text("src/applied.rs".to_owned())]),
    )]);
    let event = Event::ToolErrorReported(ToolError {
        presentation: Default::default(),
        call_id: ToolCallId::new("call"),
        tool_name: ToolName::new("apply_patch"),
        tool_type: ToolType::Function,
        message: "later hunk failed".to_owned(),
        details: Some(details.clone()),
        display: Some(ToolUseState {
            args: "apply_patch".to_owned(),
            status: ToolUseStatus::Error,
            status_text: "later hunk failed".to_owned(),
            payload: Some(oversized_diff_payload("src/applied.rs")),
            ..Default::default()
        }),
        originator: Default::default(),
    });

    let event = budgeted_event(event);
    let Event::ToolErrorReported(error) = &event else {
        panic!("expected reported tool error");
    };
    let display = error.display.as_ref().expect("retained display");
    assert_eq!(error.details.as_ref(), Some(&details));
    assert_eq!(display.args, "apply_patch");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "later hunk failed");
    assert_eq!(
        display.payload.as_ref(),
        Some(&ToolUsePayload::Text {
            text: format!("{}\nChanged files:\nsrc/applied.rs", diff_marker()),
        })
    );
    assert!(frame_fits(&event).expect("measure truncated failure"));
}

/// Ensures duplicated path labels in an optional multi-file diff cannot keep an
/// otherwise reportable truthful result above the frame cap after truncation.
#[test]
fn oversized_path_label_marker_compacts_and_final_frame_fits() {
    let paths = (0..2_000)
        .map(|index| format!("src/{index:04}-{}.rs", "p".repeat(2_980)))
        .collect::<Vec<_>>();
    let summary = paths.join("\n");
    let files = paths
        .iter()
        .map(|path| FileDiffSummary {
            path: path.clone(),
            diff: DiffSummary::default(),
        })
        .collect();
    let event = result_event(
        CborValue::Text(summary.clone()),
        Vec::new(),
        ToolUseState {
            args: "apply_patch".to_owned(),
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            payload: Some(ToolUsePayload::Diffs { files }),
            ..Default::default()
        },
    );

    let event = budgeted_event(event);
    let Event::ToolResultReported(result) = &event else {
        panic!("expected reported tool result");
    };
    assert_eq!(result.result, CborValue::Text(summary));
    assert_eq!(
        result
            .display
            .as_ref()
            .and_then(|display| display.payload.as_ref()),
        Some(&ToolUsePayload::Text {
            text: "[diff truncated]".to_owned(),
        })
    );
    assert!(frame_fits(&event).expect("measure compacted path marker"));
}
