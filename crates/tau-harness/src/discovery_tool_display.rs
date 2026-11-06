//! Selected, already-redacted discovery facts for generic tool presentation.

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStats};

/// Closed discovery result family whose public identity rows may be displayed.
#[derive(Clone, Copy)]
pub enum DiscoveryKind {
    /// Redacted current-session agent identities.
    Agents,
    /// Redacted peer-session identities.
    Sessions,
}

impl DiscoveryKind {
    /// Return the producer-owned collection field.
    fn collection(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Sessions => "sessions",
        }
    }
}

/// Describe returned identities without copying private prompts or raw results.
pub fn discovery_tool_display(result: &CborValue, kind: DiscoveryKind) -> ToolUseState {
    let rows = match tau_proto::cbor_field(result, kind.collection()) {
        Some(CborValue::Array(rows)) => rows.as_slice(),
        _ => &[],
    };
    let mut lines = Vec::new();
    for row in rows {
        let fields = ["agent_id", "session_id", "role", "group", "state"];
        let parts = fields
            .into_iter()
            .filter_map(|key| {
                tau_proto::cbor_text_field(row, key).map(|value| format!("{key}: {value}"))
            })
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            lines.push(parts.join("  "));
        }
    }
    let mut display = ToolUseState {
        stats: ToolUseStats {
            matches: Some(rows.len() as u64),
            ..Default::default()
        },
        status_text: "ok".to_owned(),
        ..Default::default()
    };
    for key in ["truncated", "scan_truncated"] {
        if matches!(
            tau_proto::cbor_field(result, key),
            Some(CborValue::Bool(true))
        ) {
            display.info_chips.push(key.replace('_', " "));
            lines.push(format!("{key}: true"));
        }
    }
    if !lines.is_empty() {
        display.payload = Some(ToolUsePayload::Text {
            text: lines.join("\n"),
        });
    }
    display
}

#[cfg(test)]
mod tests;
