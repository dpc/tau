//! Read-only session inspection for CLI sub-commands and scripts.
//!
//! Operates entirely on `tau-core` types and on-disk session formats. It has no
//! dependency on the harness daemon, keeping inspection dependency-light.

mod agent_trace;
mod lossless_json;
mod session_stats;

use std::path::{Path, PathBuf};
use std::{fmt, io};

pub use agent_trace::*;
pub use session_stats::*;
use tau_core::{AgentEntry, AgentStoreError, SessionStore, SessionStoreError};
use tau_proto::{CborValue, ContentPart, ContextItem, ToolCallItem, ToolResultStatus};

/// Errors from the read-only inspection paths.
#[derive(Debug)]
pub enum InspectError {
    /// A filesystem operation failed while reading inspection data.
    Io(io::Error),
    /// The session store could not be opened or decoded.
    SessionStore(SessionStoreError),
    /// An authoritative agent journal could not be opened or decoded.
    AgentStore(AgentStoreError),
    /// Trace discovery or serialization could not produce a stable artifact.
    Trace(AgentTraceError),
}

/// Typed failures specific to stable agent-trace preparation.
#[derive(Debug)]
pub enum AgentTraceError {
    /// Creator-based workflow membership changed during snapshot preparation.
    DescendantsChanged,
    /// A typed event could not be represented or an output format failed.
    Projection(String),
}

impl fmt::Display for AgentTraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DescendantsChanged => {
                f.write_str("agent descendants changed during snapshot preparation")
            }
            Self::Projection(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AgentTraceError {}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
            Self::AgentStore(source) => write!(f, "agent store error: {source}"),
            Self::Trace(source) => write!(f, "agent trace error: {source}"),
        }
    }
}

impl std::error::Error for InspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::SessionStore(source) => Some(source),
            Self::AgentStore(source) => Some(source),
            Self::Trace(source) => Some(source),
        }
    }
}

impl From<io::Error> for InspectError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<SessionStoreError> for InspectError {
    fn from(source: SessionStoreError) -> Self {
        Self::SessionStore(source)
    }
}

impl From<AgentStoreError> for InspectError {
    fn from(source: AgentStoreError) -> Self {
        Self::AgentStore(source)
    }
}

/// Returns the default per-state directory: `$XDG_STATE_HOME/tau` (typically
/// `~/.local/state/tau` on Linux), or `.tau/state` if no state dir is
/// available.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    tau_config::settings::state_dir().unwrap_or_else(|| PathBuf::from(".tau").join("state"))
}

/// Returns the default per-session storage root: `default_state_dir()` joined
/// with `sessions/`. Session subdirectories live one level deeper to keep the
/// state-dir top level reserved for tau-wide scalar files such as `cli.json`.
#[must_use]
pub fn default_sessions_dir() -> PathBuf {
    tau_config::settings::sessions_dir_of(&default_state_dir())
}

/// Returns the default durable-agent journal root below Tau's state directory.
#[must_use]
pub fn default_agents_dir() -> PathBuf {
    default_state_dir().join("agents")
}

/// Returns the conventional session id used when no explicit session id is
/// selected.
#[must_use]
pub fn default_session_id() -> &'static str {
    "default"
}

/// Opens a session store at `path` using the core session-store implementation.
///
/// This creates the store root when it does not already exist. Prefer
/// [`session_lines`] or [`session_list_lines`] for read-only command output.
pub fn open_session_store(path: impl AsRef<Path>) -> Result<SessionStore, InspectError> {
    SessionStore::open(path.as_ref()).map_err(InspectError::from)
}

/// Returns printable lines describing the currently loaded agents in one
/// session.
///
/// Missing session roots and missing session ids are reported as a
/// human-readable “not found” line instead of creating on-disk session
/// directories.
pub fn session_lines(
    path: impl AsRef<Path>,
    session_id: &tau_proto::SessionId,
) -> Result<Vec<String>, InspectError> {
    let path = path.as_ref();
    if !path.try_exists()? {
        return Ok(vec![format!("session {session_id} not found")]);
    }
    let store = open_session_store(path)?;
    let Some(tree) = store.session(session_id.as_str()) else {
        return Ok(vec![format!("session {session_id} not found")]);
    };
    Ok(tree
        .loaded_agents()
        .into_iter()
        .enumerate()
        .map(|(i, agent_id)| format!("{}: loaded agent {}", i + 1, agent_id))
        .collect())
}

/// Returns printable lines summarizing all persisted sessions.
///
/// A missing session root is treated as an empty store without creating it.
pub fn session_list_lines(path: impl AsRef<Path>) -> Result<Vec<String>, InspectError> {
    let path = path.as_ref();
    if !path.try_exists()? {
        return Ok(vec!["no sessions".to_owned()]);
    }
    let mut session_ids = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let session_path = entry.path();
        if !session_path.is_dir() || !session_path.join("events.cbor").exists() {
            continue;
        }
        let session_id = session_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                InspectError::SessionStore(SessionStoreError::InvalidSessionDir {
                    path: session_path.clone(),
                })
            })?;
        session_ids.push(session_id.to_owned());
    }
    session_ids.sort();
    if session_ids.is_empty() {
        return Ok(vec!["no sessions".to_owned()]);
    }

    let mut store = SessionStore::open_lazy(path)?;
    let mut lines = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        match store.load_session(&session_id) {
            Ok(Some(session)) => lines.push(format!(
                "{} ({} loaded agent(s))",
                session.session_id(),
                session.loaded_agents().len()
            )),
            Ok(None) => {}
            Err(error) => {
                lines.push(format!("{session_id} (invalid session state: {error})"));
            }
        }
    }
    if lines.is_empty() {
        lines.push("no sessions".to_owned());
    }
    Ok(lines)
}

/// Pretty-print one session entry for line-oriented inspection output
/// (`tau session show`, `:tree`, debug log).
#[must_use]
pub fn format_session_entry(entry: &AgentEntry) -> String {
    match entry {
        AgentEntry::UserInput { items, .. } => {
            format!("user: {}", first_message_text(items).unwrap_or_default())
        }
        AgentEntry::AssistantResponse { output_items, .. } => {
            let body =
                assistant_output_preview(output_items).unwrap_or_else(|| "(no text)".to_owned());
            format!("agent: {body}")
        }
        AgentEntry::ToolResults { items } => {
            if items.is_empty() {
                return "tool.result (empty)".to_owned();
            };
            items
                .iter()
                .map(format_tool_result_item)
                .collect::<Vec<_>>()
                .join("; ")
        }
        AgentEntry::AgentMessage {
            direction, message, ..
        } => {
            let event_name = match direction {
                tau_core::AgentMessageDirection::Outbound => "agent.message_sent",
                tau_core::AgentMessageDirection::Inbound => "agent.message_received",
            };
            format!("{event_name}: {message}")
        }
        AgentEntry::MessageFact { item, .. } => item
            .content
            .iter()
            .map(|part| match part {
                tau_proto::ContentPart::Text { text }
                | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                tau_proto::ContentPart::UrlCitation { .. }
                | tau_proto::ContentPart::CitationMetadataInvalid => "",
            })
            .collect::<Vec<_>>()
            .join("\n"),
        AgentEntry::Compaction {
            replacement_window, ..
        } => {
            format!("[compacted context: {} items]", replacement_window.len())
        }
        AgentEntry::CompactionTrigger { .. } => "[compaction requested]".to_owned(),
    }
}

fn format_tool_result_item(item: &tau_proto::ToolResultItem) -> String {
    match &item.status {
        ToolResultStatus::Success => {
            let preview = truncate_chars(&item.output.render(), 80);
            format!("tool.result {} -> {preview}", item.call_id)
        }
        ToolResultStatus::Error { message } => {
            format!("tool.error {} -> {message}", item.call_id)
        }
        ToolResultStatus::Cancelled { reason } => {
            format!("tool.cancelled {} -> {reason}", item.call_id)
        }
    }
}

fn assistant_output_preview(items: &[ContextItem]) -> Option<String> {
    let parts = items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(_) => first_message_text(std::slice::from_ref(item)),
            ContextItem::ToolCall(call) => Some(tool_call_preview(call)),
            ContextItem::CompactionTrigger => Some("manual compaction requested".to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then_some(parts.join(" "))
}

fn tool_call_preview(call: &ToolCallItem) -> String {
    let args = match call.arguments {
        CborValue::Map(ref entries) => entries.iter().find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value))
                if matches!(key.as_str(), "path" | "pattern" | "command" | "task_name") =>
            {
                Some(value.clone())
            }
            _ => None,
        }),
        _ => None,
    };
    match args {
        Some(args) if !args.is_empty() => format!("tool.call {} {args}", call.name),
        _ => format!("tool.call {}", call.name),
    }
}

fn first_message_text(items: &[ContextItem]) -> Option<String> {
    items.iter().find_map(|item| match item {
        ContextItem::Message(message) => {
            let mut text = String::new();
            for part in &message.content {
                if let ContentPart::Text { text: part }
                | ContentPart::SyntheticCompactionSummary { text: part }
                | ContentPart::HarnessInternalText { text: part } = part
                {
                    text.push_str(part);
                }
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests;
