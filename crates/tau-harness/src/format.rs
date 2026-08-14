//! Pretty-printing helpers for harness lifecycle and tool-progress
//! events. Session-entry rendering lives in `tau-session-inspect`; the
//! harness pulls in [`tau_session_inspect::format_session_entry`] for
//! its tree-preview helper.

use tau_core::AgentEntry;
use tau_proto::{Event, ProgressUpdate, ToolProgress};

/// Formats a tool progress event for display.
#[must_use]
pub fn format_tool_progress(progress: &ToolProgress) -> String {
    let mut text = progress.tool_name.to_string();
    if let Some(message) = &progress.message {
        text.push_str(": ");
        text.push_str(message);
    }
    if let Some(ProgressUpdate {
        current: Some(current),
        total: Some(total),
    }) = &progress.progress
    {
        text.push_str(&format!(" ({current}/{total})"));
    }
    text
}

/// Formats an extension lifecycle event for display.
#[must_use]
pub fn format_extension_event(event: &Event) -> String {
    match event {
        Event::ExtensionStarting(s) => format!("extension {} starting", s.extension_name),
        Event::ExtensionReady(r) => format!("extension {} ready", r.extension_name),
        Event::ExtensionExited(e) => format!("extension {} exited", e.extension_name),
        Event::ExtensionRestarting(r) => format!("extension {} restarting", r.extension_name),
        _ => event.name().to_string(),
    }
}

/// Renders one session entry for terminal-inert `:tree` presentation.
///
/// The preview selects the bounded source-scalar window before visibly
/// encoding unsafe scalars and backslashes, so encoding cannot move the
/// established truncation boundary or produce a partial escape.
pub(crate) fn render_entry_preview(entry: &AgentEntry) -> String {
    let raw = tau_session_inspect::format_session_entry(entry);
    let mut scalars = raw.chars();
    let mut preview = String::new();
    for character in scalars.by_ref().take(60) {
        match character {
            '\n' => preview.push(' '),
            '\\' => preview.push_str("\\\\"),
            character if tau_proto::requires_visible_escape(character) => {
                use std::fmt::Write as _;
                let _ = write!(preview, "\\u{{{:04X}}}", character as u32);
            }
            character => preview.push(character),
        }
    }
    if scalars.next().is_some() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests;
