use tau_proto::{CborValue, ToolCallId, ToolName, ToolUseState, ToolUseStatus};

use crate::tool_render::synthesize_fallback_display;

/// Canonical outcome that owns a terminal tool row's displayed status.
#[derive(Clone, Copy)]
pub(super) enum TerminalToolOutcome<'a> {
    /// The terminal event reports successful completion.
    SuccessResult,
    /// The terminal event reports failure with this canonical message.
    Error { canonical_message: &'a str },
    /// The terminal event reports cancellation.
    Cancelled,
}

/// Borrowed fields shared by foreground and background tool-error terminals.
pub(super) struct BorrowedToolError<'a> {
    /// Stable call identity used to finish runtime state.
    pub(super) call_id: &'a ToolCallId,
    /// Generic tool identity rendered in the terminal row.
    pub(super) tool_name: &'a ToolName,
    /// Canonical terminal error message.
    pub(super) message: &'a str,
    /// Optional structured details used by generic delegate fallback rendering.
    pub(super) details: Option<&'a CborValue>,
    /// Optional producer-supplied generic display descriptor.
    pub(super) descriptor: Option<&'a ToolUseState>,
    /// Whether this terminal belongs to the user-facing conversation.
    pub(super) originator_is_user: bool,
}

/// Makes a producer descriptor's status agree with its canonical terminal
/// event.
///
/// The descriptor still owns all non-status presentation metadata. A successful
/// terminal may retain a completed warning, while an error descriptor may
/// retain its nonempty label only when it already described an error.
pub(super) fn normalize_terminal_tool_use_state(
    mut descriptor: ToolUseState,
    outcome: TerminalToolOutcome<'_>,
) -> ToolUseState {
    match outcome {
        TerminalToolOutcome::SuccessResult => {
            if descriptor.status == ToolUseStatus::Warning {
                if descriptor.status_text.trim().is_empty() {
                    descriptor.status_text = "warn".to_owned();
                }
            } else {
                descriptor.status = ToolUseStatus::Success;
                descriptor.status_text = "ok".to_owned();
            }
        }
        TerminalToolOutcome::Error { canonical_message } => {
            let retain_producer_label = descriptor.status == ToolUseStatus::Error
                && !descriptor.status_text.trim().is_empty();
            descriptor.status = ToolUseStatus::Error;
            if !retain_producer_label {
                descriptor.status_text = if canonical_message.trim().is_empty() {
                    "err".to_owned()
                } else {
                    let fallback =
                        synthesize_fallback_display("", Some(canonical_message)).status_text;
                    if fallback.trim().is_empty() {
                        "err".to_owned()
                    } else {
                        fallback
                    }
                };
            }
        }
        TerminalToolOutcome::Cancelled => {
            descriptor.status = ToolUseStatus::Warning;
            descriptor.status_text = "cancelled".to_owned();
        }
    }
    descriptor
}
