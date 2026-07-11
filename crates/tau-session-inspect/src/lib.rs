//! Read-only session/policy inspection for CLI sub-commands and scripts.
//!
//! Operates entirely on `tau-core` types and the on-disk session/policy
//! format. Intentionally has no dependency on the harness daemon, so
//! `tau session show` / `tau policy list` / similar commands don't drag
//! in the agent, extension supervisor, or event-loop graph just to
//! render an events.jsonl.

use std::path::{Path, PathBuf};
use std::{fmt, io};

use tau_core::{AgentEntry, PolicyStore, SessionStore, SessionStoreError};
use tau_proto::{
    CborValue, ContentPart, ContextItem, EventSelector, ToolCallItem, ToolResultStatus,
};

/// Errors from the read-only inspection paths.
#[derive(Debug)]
pub enum InspectError {
    /// A filesystem operation failed while reading inspection data.
    Io(io::Error),
    /// The session or policy store could not be opened or decoded.
    SessionStore(SessionStoreError),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::SessionStore(source) => write!(f, "session store error: {source}"),
        }
    }
}

impl std::error::Error for InspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::SessionStore(source) => Some(source),
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

/// Returns the default per-state directory: `$XDG_STATE_HOME/tau` (typically
/// `~/.local/state/tau` on Linux), or `.tau/state` if no state dir is
/// available.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    tau_config::settings::state_dir().unwrap_or_else(|| PathBuf::from(".tau").join("state"))
}

/// Returns the default per-session storage root: `default_state_dir()` joined
/// with `sessions/`. Session subdirectories live one level deeper to keep the
/// state-dir top level reserved for tau-wide scalar files (`policy.cbor`,
/// `cli.json`, …).
#[must_use]
pub fn default_sessions_dir() -> PathBuf {
    tau_config::settings::sessions_dir_of(&default_state_dir())
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

/// Opens a policy store at `path` using the core policy-store implementation.
///
/// This creates the parent directory when it does not already exist. Prefer
/// [`policy_lines`] for read-only command output.
pub fn open_policy_store(path: impl AsRef<Path>) -> Result<PolicyStore, InspectError> {
    PolicyStore::open(path.as_ref()).map_err(InspectError::from)
}

/// Returns printable lines describing the currently loaded agents in one
/// session.
///
/// Missing session roots and missing session ids are reported as a
/// human-readable “not found” line instead of creating on-disk session
/// directories.
pub fn session_lines(
    path: impl AsRef<Path>,
    session_id: &str,
) -> Result<Vec<String>, InspectError> {
    let path = path.as_ref();
    if !path.try_exists()? {
        return Ok(vec![format!("session {session_id} not found")]);
    }
    let store = open_session_store(path)?;
    let Some(tree) = store.session(session_id) else {
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
    let store = open_session_store(path)?;
    let mut sessions = store.sessions();
    sessions.sort_by(|a, b| a.session_id().cmp(b.session_id()));
    if sessions.is_empty() {
        return Ok(vec!["no sessions".to_owned()]);
    }
    Ok(sessions
        .into_iter()
        .map(|s| {
            let loaded = s.loaded_agents();
            format!("{} ({} loaded agent(s))", s.session_id(), loaded.len())
        })
        .collect())
}

/// Returns printable lines summarizing persisted subscription-policy approvals.
///
/// A missing policy file or state directory is treated as an empty policy store
/// without creating it.
pub fn policy_lines(path: impl AsRef<Path>) -> Result<Vec<String>, InspectError> {
    let path = path.as_ref();
    if !path.try_exists()? {
        return Ok(vec!["no policy approvals".to_owned()]);
    }
    let store = open_policy_store(path)?;
    let mut approvals = store.approvals().to_vec();
    approvals.sort_by(|a, b| a.connection_name.cmp(&b.connection_name));
    if approvals.is_empty() {
        return Ok(vec!["no policy approvals".to_owned()]);
    }
    Ok(approvals
        .into_iter()
        .map(|a| {
            let sels = a
                .selectors
                .iter()
                .map(|s| match s {
                    EventSelector::Exact(n) => n.to_string(),
                    EventSelector::Prefix(p) => format!("{p}*"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} [{:?}] -> {sels}",
                a.connection_name, a.connection_origin
            )
        })
        .collect())
}

/// Pretty-print one session entry for line-oriented inspection output
/// (`tau session show`, `/tree`, debug log).
#[must_use]
pub fn format_session_entry(entry: &AgentEntry) -> String {
    match entry {
        AgentEntry::UserInput { items } => {
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
        AgentEntry::MessageEnvelope { item } => item.render_provider_text(),
        AgentEntry::Compaction { replacement_window } => {
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
                let ContentPart::Text { text: part } = part;
                text.push_str(part);
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
