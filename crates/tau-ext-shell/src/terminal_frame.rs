//! Complete-frame budgeting for terminal tool reports.

#[cfg(test)]
mod tests;

use tau_proto::{Event, HarnessInputMessage, ToolError, ToolResult, ToolUsePayload, ToolUseStatus};

const IMAGE_TOO_LARGE_MESSAGE: &str =
    "typed image result exceeds the complete terminal frame byte limit";

/// Fit optional terminal presentation into the client frame budget.
///
/// Typed provider images have no alternate representation and become a clean
/// tool error when their complete envelope is too large. Successful filesystem
/// effects retain their result and display metadata; only a structured UI diff
/// is replaced with an explicit truncation marker.
pub(crate) fn budget_terminal_report(
    event: Event,
) -> tau_client::ClientResult<HarnessInputMessage> {
    let message = HarnessInputMessage::emit_with_persist(event, false);
    if message_fits(&message)? {
        return Ok(message);
    }
    let event = match message {
        HarnessInputMessage::Emit(emit) => *emit.event,
        _ => {
            return Err(tau_client::ClientError::handler(
                "terminal frame budget expected an emit message",
            ));
        }
    };

    let (event, path_labelled_diff) = match event {
        Event::ToolResultReported(result) if !result.provider_content.is_empty() => (
            Event::ToolErrorReported(image_too_large_error(result)),
            false,
        ),
        Event::ToolResultReported(mut result) => {
            let path_labelled = truncate_diff(&mut result.display);
            (Event::ToolResultReported(result), path_labelled)
        }
        Event::ToolErrorReported(mut error) => {
            let path_labelled = truncate_diff(&mut error.display);
            (Event::ToolErrorReported(error), path_labelled)
        }
        event => (event, false),
    };
    let mut message = HarnessInputMessage::emit_with_persist(event, false);
    if message_fits(&message)? {
        return Ok(message);
    }
    if path_labelled_diff {
        compact_diff_marker(&mut message);
        if message_fits(&message)? {
            return Ok(message);
        }
    }
    Err(tau_client::ClientError::Overloaded)
}

/// Return whether one already-constructed complete frame fits client admission.
fn message_fits(message: &HarnessInputMessage) -> tau_client::ClientResult<bool> {
    Ok(tau_client::encoded_outbound_frame_bytes(message)? <= tau_client::MAX_OUTBOUND_FRAME_BYTES)
}

/// Return whether the exact transient emit envelope fits the shared cap.
#[cfg(test)]
fn frame_fits(event: &Event) -> tau_client::ClientResult<bool> {
    let message = HarnessInputMessage::emit_with_persist(event.clone(), false);
    message_fits(&message)
}

/// Convert an unreportable typed-image success into a byte-free local error.
fn image_too_large_error(result: ToolResult) -> ToolError {
    let ToolResult {
        presentation,
        call_id,
        tool_name,
        tool_type,
        result: _,
        provider_content: _,
        kind: _,
        mut display,
        originator,
    } = result;
    if let Some(display) = &mut display {
        display.status = ToolUseStatus::Error;
        display.status_text = IMAGE_TOO_LARGE_MESSAGE.to_owned();
        display.payload = None;
    }
    ToolError {
        presentation,
        call_id,
        tool_name,
        tool_type,
        message: IMAGE_TOO_LARGE_MESSAGE.to_owned(),
        details: None,
        display,
        originator,
    }
}

/// Replace only a structured UI diff while retaining all other display facts.
fn truncate_diff(display: &mut Option<tau_proto::ToolUseState>) -> bool {
    let Some(payload) = display
        .as_mut()
        .and_then(|display| display.payload.as_mut())
    else {
        return false;
    };
    let marker = format!(
        "[diff truncated: complete terminal frame would exceed {} bytes]",
        tau_client::MAX_OUTBOUND_FRAME_BYTES
    );
    let path_labelled = matches!(payload, ToolUsePayload::Diffs { .. });
    let text = match payload {
        ToolUsePayload::Diff(_) => marker,
        ToolUsePayload::Diffs { files } => format!(
            "{marker}\nChanged files:\n{}",
            files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ),
        ToolUsePayload::Text { .. } => return false,
    };
    *payload = ToolUsePayload::Text { text };
    path_labelled
}

/// Drop duplicated optional path labels when the truthful result/details
/// already carry them and the expanded marker still cannot fit.
fn compact_diff_marker(message: &mut HarnessInputMessage) {
    let HarnessInputMessage::Emit(emit) = message else {
        return;
    };
    let display = match emit.event.as_mut() {
        Event::ToolResultReported(result) => &mut result.display,
        Event::ToolErrorReported(error) => &mut error.display,
        _ => return,
    };
    let Some(ToolUsePayload::Text { text }) = display
        .as_mut()
        .and_then(|display| display.payload.as_mut())
    else {
        return;
    };
    *text = "[diff truncated]".to_owned();
}
