//! Helpers tools use to attach a [`ToolUseState`] descriptor to their
//! result/error.
//!
//! Tools return `Result<ToolOutput, ToolFailure>`; both carry a
//! `ToolUseState` next to the existing CBOR payload / error message.
//! The dispatcher in [`crate::tools`] unpacks these into `ToolResult`
//! / `ToolError` events with the descriptor attached.

use tau_proto::{
    CborValue, ToolResultContentPart, ToolUsePayload, ToolUseState, ToolUseStats, ToolUseStatus,
};

/// Diagnostic assertions inspect the payload independently of the error
/// identity.
#[cfg(test)]
pub(crate) trait ErrorExplanation {
    /// Diagnostic and recovery payload, without metadata headers.
    fn explanation(&self) -> String;
}

#[cfg(test)]
impl ErrorExplanation for tau_proto::ToolError {
    fn explanation(&self) -> String {
        diagnostic_output(self.details.as_ref())
    }
}

/// Read the diagnostic payload without conflating it with metadata headers.
#[cfg(test)]
fn diagnostic_output(details: Option<&CborValue>) -> String {
    let Some(CborValue::Map(entries)) = details else {
        return String::new();
    };
    entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value)) if key == "output" => {
                Some(value.clone())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Success bundle: the CBOR result the agent consumes and the display
/// descriptor the UI consumes.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    /// Ordinary structured result rendered to normalized provider text.
    pub result: CborValue,
    /// Typed provider content kept out of ordinary text rendering.
    pub provider_content: Vec<ToolResultContentPart>,
    /// Generic metadata-only UI presentation.
    pub display: ToolUseState,
}

/// Error bundle: the message the agent sees, optional structured
/// details (e.g. shell stdout/stderr), and the display descriptor.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    /// Stable model-visible error identifier.
    pub message: String,
    /// Semantic diagnostic headers and explanatory payload.
    pub details: Option<Box<CborValue>>,
    /// Independent UI presentation, including any mutation diffs.
    pub display: Box<ToolUseState>,
}

impl ToolFailure {
    /// Recover explanatory body text for diagnostic-focused regression
    /// assertions.
    #[cfg(test)]
    pub(crate) fn explanation(&self) -> String {
        diagnostic_output(self.details.as_deref())
    }

    /// Report a shell-tool failure with the diagnostic in the payload.
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let status_text = error_chip_text(&message);
        Self {
            message: "shell-tool-failed".to_owned(),
            details: Some(Box::new(CborValue::Map(vec![(
                CborValue::Text("output".to_owned()),
                CborValue::Text(message),
            )]))),
            display: Box::new(ToolUseState {
                status: ToolUseStatus::Error,
                status_text,
                ..Default::default()
            }),
        }
    }

    /// Report an already classified failure without manufacturing a payload.
    pub fn identifier(identifier: impl Into<String>) -> Self {
        let identifier = identifier.into();
        let mut failure = Self::new(&identifier);
        failure.message = identifier;
        failure.details = None;
        failure
    }

    pub fn with_args(mut self, args: impl Into<String>) -> Self {
        self.display.args = args.into();
        self
    }

    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.display.mode = mode.into();
        self
    }

    /// Retain the diagnostic while adding structured evidence and recovery
    /// text.
    pub fn with_details(mut self, details: CborValue) -> Self {
        let mut details = details;
        if let (Some(previous), CborValue::Map(entries)) = (&self.details, &mut details)
            && let CborValue::Map(previous) = previous.as_ref()
        {
            for (key, value) in previous {
                if let Some((_, existing)) = entries.iter_mut().find(|(k, _)| k == key) {
                    if matches!(key, CborValue::Text(key) if key == "output")
                        && let (CborValue::Text(before), CborValue::Text(after)) = (value, existing)
                    {
                        if after.is_empty() {
                            *after = before.clone();
                        } else if !before.is_empty() {
                            *after = format!("{before}\n\n{after}");
                        }
                    }
                } else {
                    entries.push((key.clone(), value.clone()));
                }
            }
        }
        self.details = Some(Box::new(details));
        self
    }

    pub fn with_payload(mut self, payload: Option<ToolUsePayload>) -> Self {
        self.display.payload = payload;
        self
    }
}

impl From<String> for ToolFailure {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

/// Single-line chip label for an error. Multi-line messages are
/// collapsed to their first non-empty line; error prefixing and width
/// abbreviation belong to the renderer so tool-side text does not get
/// double-formatted.
fn error_chip_text(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Build a `ToolUseStats` from textual output: lines + bytes.
/// Empty input yields an empty stats block (nothing renders).
pub(crate) fn text_stats(text: &str) -> ToolUseStats {
    if text.is_empty() {
        return ToolUseStats::default();
    }
    ToolUseStats {
        matches: None,
        lines: Some(text.lines().count() as u64),
        bytes: Some(text.len() as u64),
    }
}

/// A standard `Success` display with `args` label and `"ok"` chip.
pub(crate) fn ok_display(args: impl Into<String>) -> ToolUseState {
    ToolUseState {
        args: args.into(),
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

/// Expose exceptional output limits and recovery facts, without copying output.
///
/// This consumes only the tool's own structured result metadata. Normal
/// untruncated displays stay unchanged, and an existing command body survives.
pub(crate) fn describe_output_limits(display: &mut ToolUseState, result: &CborValue) {
    let is_true = |key| {
        matches!(
            tau_proto::cbor_field(result, key),
            Some(CborValue::Bool(true))
        )
    };
    let truncated = is_true("truncated");
    let limited = is_true("limit_reached");
    let invalid_utf8 = matches!(
        tau_proto::cbor_field(result, "valid_utf8"),
        Some(CborValue::Bool(false))
    );
    if !truncated && !limited && !invalid_utf8 {
        return;
    }
    if truncated {
        display.info_chips.push("truncated".to_owned());
    } else if limited {
        display.info_chips.push("limit reached".to_owned());
    }
    if invalid_utf8 {
        display.info_chips.push("invalid UTF-8".to_owned());
    }
    let mut facts = Vec::new();
    for key in [
        "total_lines",
        "total_bytes",
        "full_output_path",
        "saved_output_path",
        "saved_output_truncated",
        "saved_output_bytes",
        "saved_output_unavailable",
        "limit_reached",
    ] {
        let value = match tau_proto::cbor_field(result, key) {
            Some(CborValue::Text(value)) => value.clone(),
            Some(CborValue::Integer(value)) => i128::from(*value).to_string(),
            Some(CborValue::Bool(value)) => value.to_string(),
            _ => continue,
        };
        facts.push(format!("{key}: {value}"));
    }
    if facts.is_empty() {
        return;
    }
    let facts = facts.join("\n");
    match &mut display.payload {
        Some(ToolUsePayload::Text { text }) => {
            text.push_str("\n\n");
            text.push_str(&facts);
        }
        None => display.payload = Some(ToolUsePayload::Text { text: facts }),
        // Diff payloads own their detail presentation; never replace them.
        _ => {}
    }
}

#[cfg(test)]
mod tests;
