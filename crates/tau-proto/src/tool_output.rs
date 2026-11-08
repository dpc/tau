//! Ordered text output, independent of provider tool-round closure.

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
/// publication custody. It is source text, never a second native tool payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFinalOutput {
    /// Already sanitized provider-facing text, before outer-envelope escaping.
    pub text: String,
    /// Rendered bytes omitted while retaining prefix and final diagnostics.
    pub omitted_bytes: u64,
}

impl ToolFinalOutput {
    /// Capture a terminal without allocating its complete textual form.
    /// Cancellation keeps its status prefix and any available diagnostic body.
    #[must_use]
    pub fn capture(value: &CborValue, status: crate::ProviderToolResultStatus<'_>) -> Self {
        let mut output = BoundedToolOutput::render(value, status);
        if matches!(status, crate::ProviderToolResultStatus::Cancelled { .. })
            && !matches!(value, CborValue::Null)
        {
            crate::write_provider_tool_result_text(
                value,
                crate::ProviderToolResultStatus::Success,
                &mut output,
            )
            .expect("bounded output sink cannot reject canonical text");
        }
        let (text, omitted_bytes) = output.finish();
        Self {
            text,
            omitted_bytes,
        }
    }
}

/// One explicitly requested page of accepted output. This does not claim that
/// omitted process bytes or temporary saved-output files remain available.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputPage {
    /// Exact declaration whose output was requested.
    pub call: ToolCallRef,
    /// Canonical retained text, bounded independently of the aggregate.
    pub text: String,
    /// Requested UTF-8 byte offset.
    pub offset: usize,
    /// Continuation cursor, absent when the current retained output is
    /// exhausted.
    pub next_offset: Option<usize>,
    /// Currently retained rendered bytes across all accepted occurrences.
    pub retained_bytes: usize,
    /// Whether the invocation has a canonical terminal.
    pub terminal: bool,
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
pub fn render_output(
    call_id: &ToolCallId,
    _tool_name: &ToolName,
    _sequence: u64,
    terminal: bool,
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
    crate::TAU_TOOL_OUTPUT_PAYLOAD_ENVELOPE
        .render_attributed(&attributes, text)
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
            CborValue::Text("original_tool_call_id".to_owned()),
            CborValue::Text(call_id.to_string()),
        ),
        (
            CborValue::Text("target_outcome".to_owned()),
            CborValue::Text(outcome.as_str().to_owned()),
        ),
    ])
}
