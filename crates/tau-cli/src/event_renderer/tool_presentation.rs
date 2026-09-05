//! Built-in tool display normalization.

use std::time::Duration;

use tau_proto::CborValue;

use super::{BLOCKER_TOOL_NAME, ToolCallDisplay, ToolStatus};

#[derive(Clone, Copy)]
/// Safe action discriminator retained from a blocker invocation.
pub(super) enum BlockerAction {
    /// Add a blocker.
    Add,
    /// Cancel a blocker.
    Cancel,
    /// List blockers.
    List,
}

impl BlockerAction {
    /// Returns the stable compact label for this action.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Cancel => "cancel",
            Self::List => "list",
        }
    }
}

/// Extracts the sole safe compact descriptor from a built-in blocker
/// invocation.
///
/// Blocker payloads carry titles, descriptions, answers, and cancellation
/// reasons. The action discriminant alone distinguishes the operation without
/// displaying any of that payload.
pub(super) fn blocker_action_descriptor(started: &tau_proto::ToolStarted) -> Option<BlockerAction> {
    if !is_blocker_tool_name(started.tool_name.as_str()) {
        return None;
    }
    let CborValue::Map(entries) = &started.arguments else {
        return None;
    };
    let mut action = None;
    for (key, value) in entries {
        if !matches!(key, CborValue::Text(key) if key == "action") {
            continue;
        }
        let CborValue::Text(value) = value else {
            return None;
        };
        if action.is_some() {
            return None;
        }
        action = match value.as_str() {
            "add" => Some(BlockerAction::Add),
            "cancel" => Some(BlockerAction::Cancel),
            "list" => Some(BlockerAction::List),
            _ => return None,
        };
    }
    action
}

/// Recognizes the bundled Swarm blocker name with an optional structural
/// extension-instance prefix, but never its removed legacy alias.
pub(super) fn is_blocker_tool_name(name: &str) -> bool {
    name == BLOCKER_TOOL_NAME
        || name
            .strip_suffix("_task_blocker")
            .is_some_and(|prefix| !prefix.is_empty())
}

/// Returns the effective timeout for a built-in shell invocation.
///
/// Shell providers enforce a 300-second default when the agent omits
/// `timeout`. This narrow presentation projection retains that declared limit
/// so the generic duration chip can show elapsed time against the actual
/// command budget without changing the provider display protocol.
pub(super) fn effective_shell_timeout(started: &tau_proto::ToolStarted) -> Option<Duration> {
    const DEFAULT_TIMEOUT_SECS: u64 = 300;

    if !matches!(started.tool_name.as_str(), "shell" | "gpt_shell") {
        return None;
    }
    let CborValue::Map(entries) = &started.arguments else {
        return None;
    };

    let mut timeout = None;
    for (key, value) in entries {
        if !matches!(key, CborValue::Text(key) if key == "timeout") {
            continue;
        }
        let CborValue::Integer(value) = value else {
            return None;
        };
        let Ok(value) = u64::try_from(*value) else {
            return None;
        };
        if timeout.replace(value).is_some() {
            return None;
        }
    }
    Some(Duration::from_secs(timeout.unwrap_or(DEFAULT_TIMEOUT_SECS)))
}

/// Projects a blocker display through the action-only presentation boundary.
pub(super) fn sanitize_blocker_display(
    display: &mut ToolCallDisplay,
    is_blocker: bool,
    action: Option<BlockerAction>,
) {
    if !is_blocker {
        return;
    }
    display.mode.clear();
    display.args = action.map_or_else(String::new, |action| action.as_str().to_owned());
    display.range = None;
    display.suffixes.retain(|suffix| {
        matches!(
            suffix.status,
            ToolStatus::Success
                | ToolStatus::Warning
                | ToolStatus::Error
                | ToolStatus::Pending
                | ToolStatus::Progress
                | ToolStatus::Time
        )
    });
    display.payload = None;
}
