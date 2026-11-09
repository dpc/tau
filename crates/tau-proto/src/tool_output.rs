//! Ordered text output, independent of provider tool-round closure.

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::{CborValue, ToolCallId, ToolCallRef, ToolName};

/// Opt-in registration tag for tools that report incremental text output.
pub const TOOL_OUTPUT_STREAM_TAG: &str = "tool:output-stream";
/// Maximum UTF-8 payload in one incremental output report.
pub const MAX_TOOL_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
/// Maximum incremental text accepted for one invocation, excluding its final
/// tail.
pub const MAX_TOOL_OUTPUT_LIVE_BYTES: usize = 64 * 1024;
/// Maximum updates accepted for one invocation, including empty gap notices.
pub const MAX_TOOL_OUTPUT_UPDATES: u64 = 64;
/// Final diagnostic allowance, independent of the incremental allowance.
pub const MAX_TOOL_OUTPUT_FINAL_BYTES: usize = 8 * 1024;

/// Harness-owned bounded rendering of an opted-in terminal, captured before
/// publication custody. Its carrier is selected by the canonical terminal.
/// Text serialization follows `SPEC-context-output-formatting`; capture and
/// delivery follow `SPEC-streamed-tool-output`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFinalOutput {
    /// Logical terminal status for current source projections. Historical
    /// captures omit it and retain their committed body unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ToolCompletionOutcome>,
    /// Already sanitized provider-facing text, before outer-envelope escaping.
    pub text: String,
    /// Rendered bytes omitted while retaining prefix and final diagnostics.
    pub omitted_bytes: u64,
}

impl ToolFinalOutput {
    /// Capture foreground cancellation with its available tail and one status
    /// header. Other outcomes use the same bounded rendering as background
    /// capture; no accepted incremental bytes consume this final allowance.
    #[must_use]
    pub fn capture_native(value: &CborValue, status: crate::ProviderToolResultStatus<'_>) -> Self {
        if let crate::ProviderToolResultStatus::Cancelled { reason } = status {
            let mut output = BoundedToolOutput {
                prefix: String::new(),
                tail: String::new(),
                total: 0,
                prefix_closed: false,
            };
            crate::context::write_cancelled_tool_output(value, reason, &mut output)
                .expect("bounded output sink cannot reject canonical text");
            let (text, omitted_bytes) = output.finish();
            Self {
                outcome: Some(ToolCompletionOutcome::Cancelled),
                text,
                omitted_bytes,
            }
        } else {
            Self::capture(value, status)
        }
    }

    /// Capture a terminal without allocating its complete textual form.
    /// Cancellation keeps available output, or the unavailable-capture reason.
    /// Its logical status is separate from the payload.
    #[must_use]
    pub fn capture(value: &CborValue, status: crate::ProviderToolResultStatus<'_>) -> Self {
        let outcome = match status {
            crate::ProviderToolResultStatus::Success => ToolCompletionOutcome::Completed,
            crate::ProviderToolResultStatus::Error { .. } => ToolCompletionOutcome::Error,
            crate::ProviderToolResultStatus::Cancelled { .. } => ToolCompletionOutcome::Cancelled,
        };
        let output = if let crate::ProviderToolResultStatus::Cancelled { reason } = status {
            let mut output = BoundedToolOutput {
                prefix: String::new(),
                tail: String::new(),
                total: 0,
                prefix_closed: false,
            };
            if matches!(value, CborValue::Null) {
                use std::fmt::Write as _;
                output.write_str("reason: ").expect("bounded sink");
                crate::write_provider_tool_header_text(reason, &mut output).expect("bounded sink");
            } else {
                crate::write_provider_tool_result_text(
                    value,
                    crate::ProviderToolResultStatus::Success,
                    &mut output,
                )
                .expect("bounded sink");
            }
            output
        } else {
            BoundedToolOutput::render(value, status)
        };
        let (text, omitted_bytes) = output.finish();
        Self {
            outcome: Some(outcome),
            text,
            omitted_bytes,
        }
    }
}

impl crate::ToolResponse {
    /// Compose accepted batches and the separately bounded final capture once,
    /// without reapplying the final cap. Explicit batch headers distinguish
    /// partial lines and per-batch diagnostics from source in adjacent batches.
    #[must_use]
    pub fn from_captured_output(
        updates: &[ToolOutputUpdate],
        final_output: &ToolFinalOutput,
    ) -> Self {
        let mut body = String::new();
        for update in updates {
            let mut headers = vec![crate::ToolResponseHeader {
                key: "output_batch".to_owned(),
                value: update.output.sequence.to_string(),
            }];
            if update.output.omitted_bytes != 0 {
                headers.push(crate::ToolResponseHeader {
                    key: "omitted_bytes".to_owned(),
                    value: update.output.omitted_bytes.to_string(),
                });
            }
            let response = Self {
                raw: CborValue::Null,
                headers,
                body: update.output.text.clone(),
            };
            body.push_str(&response.render());
            body.push_str("\n\n");
        }
        let mut headers = Vec::new();
        if !updates.is_empty() {
            headers.push(crate::ToolResponseHeader {
                key: "final_output".to_owned(),
                value: "true".to_owned(),
            });
        }
        if final_output.omitted_bytes != 0 {
            headers.push(crate::ToolResponseHeader {
                key: "omitted_bytes".to_owned(),
                value: final_output.omitted_bytes.to_string(),
            });
        }
        body.push_str(
            &Self {
                raw: CborValue::Null,
                headers,
                body: final_output.text.clone(),
            }
            .render(),
        );
        Self {
            raw: CborValue::Null,
            headers: Vec::new(),
            body,
        }
    }
}

/// A producer-owned immutable output batch. Sequence numbers count reports, not
/// reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputReport {
    /// Display identifier of the currently routed invocation.
    pub call_id: ToolCallId,
    /// Registered name of the producer.
    pub tool_name: ToolName,
    /// Zero-based sequence within this invocation.
    pub sequence: u64,
    /// Newly available text, never a cumulative snapshot.
    pub text: String,
    /// Bytes deliberately omitted while forming this batch.
    pub omitted_bytes: u64,
}

/// Harness-owned accepted output occurrence; final output belongs to its
/// terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputUpdate {
    /// Exact provider declaration occurrence, independent of display-ID reuse.
    pub call: ToolCallRef,
    /// Validated producer report.
    pub output: ToolOutputReport,
}

impl ToolOutputUpdate {
    /// Project untrusted bytes through the shared envelope registry.
    pub fn render(&self) -> String {
        let mut text = String::new();
        crate::write_provider_tool_body_text(&self.output.text, &mut text)
            .expect("writing output text to String cannot fail");
        render_output(
            &self.output.call_id,
            &self.output.tool_name,
            self.output.sequence,
            false,
            None,
            self.output.omitted_bytes,
            &text,
        )
    }
}

/// Canonically rendered final text with a bounded prefix and recent
/// diagnostics. Omissions describe rendered UTF-8 bytes, not raw process bytes.
pub struct BoundedToolOutput {
    /// Retained text, excluding the gap marker.
    prefix: String,
    /// Most recent rendered bytes following the prefix.
    tail: String,
    /// Complete rendered size, including bytes no longer retained.
    total: usize,
    /// Once a fragment no longer fits, later fragments belong only to the tail.
    prefix_closed: bool,
}

impl BoundedToolOutput {
    /// Render a native result without allocating its complete textual form.
    #[must_use]
    pub fn render(value: &CborValue, status: crate::ProviderToolResultStatus<'_>) -> Self {
        let mut output = Self {
            prefix: String::new(),
            tail: String::new(),
            total: 0,
            prefix_closed: false,
        };
        crate::write_provider_tool_result_text(value, status, &mut output)
            .expect("bounded output sink cannot reject canonical text");
        output
    }

    /// Return the bounded text and the count of omitted rendered bytes.
    #[must_use]
    pub fn finish(self) -> (String, u64) {
        let omitted = self
            .total
            .saturating_sub(self.prefix.len() + self.tail.len());
        let text = if omitted == 0 {
            self.prefix + &self.tail
        } else {
            format!(
                "{}\n[... {omitted} rendered bytes omitted ...]\n{}",
                self.prefix, self.tail
            )
        };
        (text, omitted as u64)
    }
}

impl std::fmt::Write for BoundedToolOutput {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        // Leave room for the gap marker inside the final allowance.
        let part = (MAX_TOOL_OUTPUT_FINAL_BYTES - 128) / 2;
        self.total = self.total.saturating_add(text.len());
        let mut prefix_end = if self.prefix_closed {
            0
        } else {
            part.saturating_sub(self.prefix.len()).min(text.len())
        };
        while !text.is_char_boundary(prefix_end) {
            prefix_end -= 1;
        }
        self.prefix_closed |= prefix_end < text.len();
        self.prefix.push_str(&text[..prefix_end]);
        let rest = &text[prefix_end..];
        if rest.len() >= part {
            let mut start = rest.len() - part;
            while !rest.is_char_boundary(start) {
                start += 1;
            }
            self.tail.clear();
            self.tail.push_str(&rest[start..]);
        } else {
            let mut remove = (self.tail.len() + rest.len()).saturating_sub(part);
            while !self.tail.is_char_boundary(remove) {
                remove += 1;
            }
            self.tail.drain(..remove);
            self.tail.push_str(rest);
        }
        Ok(())
    }
}

/// Render one ordered tool-origin occurrence, without interpreting its payload.
/// Tool identity and sequence remain canonical metadata; the model sees only
/// call correlation, terminal status, and nonzero omission counts.
/// Framing and the separate `ToolResponse` text contract follow
/// `SPEC-context-output-formatting`.
pub fn render_output(
    call_id: &ToolCallId,
    _tool_name: &ToolName,
    _sequence: u64,
    terminal: bool,
    outcome: Option<ToolCompletionOutcome>,
    omitted_bytes: u64,
    text: &str,
) -> String {
    let mut attributes = vec![
        ("call_id", call_id.to_string()),
        ("terminal", terminal.to_string()),
    ];
    if omitted_bytes != 0 {
        attributes.push(("omitted_bytes", omitted_bytes.to_string()));
    }
    // Cancellation status was historically supplied by the envelope, not the
    // stored capture. Project it as a header for old captures as well as new
    // available-empty and unavailable-capture terminals.
    let cancelled = (terminal && outcome == Some(ToolCompletionOutcome::Cancelled)).then(|| {
        crate::ToolResponse {
            raw: CborValue::Null,
            headers: vec![crate::ToolResponseHeader {
                key: "status".into(),
                value: "canceled".into(),
            }],
            body: text.to_owned(),
        }
        .render()
    });
    crate::TAU_TOOL_OUTPUT_PAYLOAD_ENVELOPE
        .render_attributed(&attributes, cancelled.as_deref().unwrap_or(text))
        .expect("tool-output attributes follow the shared registry")
}

/// Logical target outcome, distinct from successful synchronization by `wait`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCompletionOutcome {
    /// The tool returned a result (a process exit code can still be nonzero).
    Completed,
    /// The tool failed.
    Error,
    /// The tool was cancelled.
    Cancelled,
}

impl ToolCompletionOutcome {
    /// Stable wire spelling used by completion receipts and compact UI labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Successful synchronization receipt; target failure is not a failure to wait.
pub fn tool_completion_receipt(call_id: &ToolCallId, outcome: ToolCompletionOutcome) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("cause".to_owned()),
            CborValue::Text(call_id.to_string()),
        ),
        (
            CborValue::Text("outcome".to_owned()),
            CborValue::Text(outcome.as_str().to_owned()),
        ),
    ])
}

/// Close the original streamed call without redundantly naming itself as its
/// cause.
pub fn tool_call_completion_receipt(outcome: ToolCompletionOutcome) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("outcome".to_owned()),
        CborValue::Text(outcome.as_str().to_owned()),
    )])
}
