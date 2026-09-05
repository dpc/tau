//! Model-visible previews for canonical background-tool completions.

use std::fmt;

use tau_proto::{
    CborValue, ProviderToolResultStatus, ShellProcessOutcome, ShellProcessOutcomeSource,
    ShellTerminationReason, ToolBackgroundError, ToolBackgroundResult,
};

/// Maximum producer-body bytes admitted across one preview-publication group.
pub(crate) const BACKGROUND_PREVIEW_GROUP_BODY_BYTES: usize = 8 * 1024;

const ERROR_SUMMARY_MESSAGE_BYTES: usize = 512;

/// Borrowed canonical data used to render one background completion preview.
///
/// Queue-owned prompts retain only exact source identities. This projection is
/// built lazily at ownership transfer and cannot duplicate the retained
/// payload.
pub(crate) struct BackgroundCompletionPreview<'a> {
    /// Provider-visible source call identity.
    call_id: &'a tau_proto::ToolCallId,
    /// Provider-visible source tool name.
    tool_name: &'a tau_proto::ToolName,
    /// Logical tool terminal outcome.
    outcome: BackgroundCompletionOutcome,
    /// Original typed payload used by canonical provider rendering.
    value: &'a CborValue,
    /// Borrowed status used by canonical provider rendering.
    status: ProviderToolResultStatus<'a>,
    /// Raw error message used by bounded summary delivery.
    error_message: Option<&'a str>,
    /// Strict process outcome for recognized shell tools.
    process_outcome: ProcessOutcomeProjection,
}

/// Remaining producer-body allowance for one publication group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackgroundPreviewBudget {
    remaining: usize,
}

impl Default for BackgroundPreviewBudget {
    fn default() -> Self {
        Self {
            remaining: BACKGROUND_PREVIEW_GROUP_BODY_BYTES,
        }
    }
}

/// Logical terminal lifecycle encoded in the preview envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundCompletionOutcome {
    /// The tool returned a logical result.
    Result,
    /// The tool failed independently of cancellation.
    Error,
    /// A typed cancellation request caused the terminal.
    Cancelled,
}

/// Typed logical outcome for one canonical background error terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackgroundErrorOutcome {
    /// The tool failed independently of cancellation.
    Error,
    /// A typed cancellation request caused the terminal.
    Cancelled,
}

/// Strict shell-process projection state encoded only in summary metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessOutcomeProjection {
    /// Canonical CBOR supplied a coherent process outcome.
    Available(ShellProcessOutcome),
    /// A recognized shell tool supplied absent or incoherent process fields.
    Unavailable,
    /// The source tool is not a recognized shell tool.
    NotApplicable,
}

impl<'a> BackgroundCompletionPreview<'a> {
    /// Build a preview from one canonical successful background terminal.
    #[must_use]
    pub(crate) fn from_result(result: &'a ToolBackgroundResult) -> Self {
        Self::from_result_parts(&result.call_id, &result.tool_name, &result.result)
    }

    /// Build a success preview directly from retained generation fields.
    #[must_use]
    pub(crate) fn from_result_parts(
        call_id: &'a tau_proto::ToolCallId,
        tool_name: &'a tau_proto::ToolName,
        value: &'a CborValue,
    ) -> Self {
        Self {
            call_id,
            tool_name,
            outcome: BackgroundCompletionOutcome::Result,
            value,
            status: ProviderToolResultStatus::Success,
            error_message: None,
            process_outcome: process_outcome_for_result(tool_name, value),
        }
    }

    /// Build a preview from one canonical failed or cancelled background
    /// terminal.
    #[must_use]
    pub(crate) fn from_error(
        error: &'a ToolBackgroundError,
        outcome: BackgroundErrorOutcome,
    ) -> Self {
        Self::from_error_parts(
            &error.call_id,
            &error.tool_name,
            &error.message,
            error.details.as_ref(),
            outcome,
        )
    }

    /// Build an error preview directly from retained generation fields.
    #[must_use]
    pub(crate) fn from_error_parts(
        call_id: &'a tau_proto::ToolCallId,
        tool_name: &'a tau_proto::ToolName,
        message: &'a str,
        details: Option<&'a CborValue>,
        outcome: BackgroundErrorOutcome,
    ) -> Self {
        let status = match outcome {
            BackgroundErrorOutcome::Error => ProviderToolResultStatus::Error { message },
            BackgroundErrorOutcome::Cancelled => {
                ProviderToolResultStatus::Cancelled { reason: message }
            }
        };
        Self {
            call_id,
            tool_name,
            outcome: match outcome {
                BackgroundErrorOutcome::Error => BackgroundCompletionOutcome::Error,
                BackgroundErrorOutcome::Cancelled => BackgroundCompletionOutcome::Cancelled,
            },
            value: details.unwrap_or(&CborValue::Null),
            status,
            error_message: (outcome == BackgroundErrorOutcome::Error).then_some(message),
            process_outcome: process_outcome_for_error(tool_name, details),
        }
    }

    /// Render the complete registered envelope and debit its delivered body.
    pub(crate) fn render(&self, budget: &mut BackgroundPreviewBudget) -> String {
        let measurement = tau_proto::measure_provider_tool_result_text(self.value, self.status);
        if measurement.rendered_bytes <= budget.remaining {
            let mut body = BoundedEnvelopeBody::new(budget.remaining);
            tau_proto::write_provider_tool_result_text(self.value, self.status, &mut body)
                .expect("bounded preview renderer cannot reject canonical text");
            if let Some(full_body) = body.finish() {
                budget.remaining -= body.escaped_bytes;
                return self.render_envelope("full", measurement.rendered_bytes, &full_body, None);
            }
        }

        let summary = self.summary_body(budget.remaining);
        budget.remaining -= summary.escaped_body_bytes;
        self.render_envelope(
            "summary",
            measurement.rendered_bytes,
            &summary.body,
            Some(&summary.attributes),
        )
    }

    fn summary_body(&self, remaining: usize) -> SummaryBody {
        let Some(message) = self.error_message else {
            return SummaryBody {
                body: String::new(),
                escaped_body_bytes: 0,
                attributes: self.summary_attributes(None),
            };
        };
        let mut normalized = BoundedTextPrefix::new(ERROR_SUMMARY_MESSAGE_BYTES);
        tau_proto::write_provider_tool_header_text(message, &mut normalized)
            .expect("bounded error-message renderer cannot reject canonical text");
        let mut body = normalized.prefix;
        let envelope = tau_proto::TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE;
        let escaped_body_bytes = loop {
            let escaped_bytes = envelope.escape_body(&body).len();
            if escaped_bytes <= remaining || body.is_empty() {
                break escaped_bytes;
            }
            body.pop();
        };
        let message_truncated = body.len() < normalized.total_bytes;
        SummaryBody {
            body,
            escaped_body_bytes,
            attributes: self.summary_attributes(Some((normalized.total_bytes, message_truncated))),
        }
    }

    fn summary_attributes(&self, message: Option<(usize, bool)>) -> Vec<(&'static str, String)> {
        let mut attributes = Vec::new();
        match self.process_outcome {
            ProcessOutcomeProjection::Available(outcome) => {
                attributes.push(("process_outcome", "available".to_owned()));
                attributes.push((
                    "process_source",
                    match outcome.source() {
                        ShellProcessOutcomeSource::ToolResult => "tool_result",
                        ShellProcessOutcomeSource::ToolErrorDetails => "tool_error_details",
                    }
                    .to_owned(),
                ));
                attributes.push(("process_success", outcome.success().to_string()));
                attributes.push((
                    "termination_reason",
                    match outcome.termination_reason() {
                        ShellTerminationReason::Exit => "exit",
                        ShellTerminationReason::Timeout => "timeout",
                        ShellTerminationReason::Signal => "signal",
                        ShellTerminationReason::StartError => "start_error",
                        ShellTerminationReason::Unknown => "unknown",
                    }
                    .to_owned(),
                ));
                if let Some(exit_code) = outcome.exit_code() {
                    attributes.push(("exit_code", exit_code.to_string()));
                }
                if let Some(signal) = outcome.signal() {
                    attributes.push(("signal", signal.to_string()));
                }
                if outcome.timed_out() {
                    attributes.push(("timed_out", "true".to_owned()));
                }
            }
            ProcessOutcomeProjection::Unavailable => {
                attributes.push(("process_outcome", "unavailable".to_owned()));
            }
            ProcessOutcomeProjection::NotApplicable => {
                attributes.push(("process_outcome", "not_applicable".to_owned()));
            }
        }
        if let Some((bytes, truncated)) = message {
            attributes.push(("message_bytes", bytes.to_string()));
            if truncated {
                attributes.push(("message_truncated", "true".to_owned()));
            }
        }
        attributes
    }

    fn render_envelope(
        &self,
        delivery: &'static str,
        rendered_bytes: usize,
        body: &str,
        optional: Option<&[(&'static str, String)]>,
    ) -> String {
        let mut attributes = vec![
            ("call_id", self.call_id.to_string()),
            ("tool", self.tool_name.to_string()),
            (
                "tool_outcome",
                match self.outcome {
                    BackgroundCompletionOutcome::Result => "result",
                    BackgroundCompletionOutcome::Error => "error",
                    BackgroundCompletionOutcome::Cancelled => "cancelled",
                }
                .to_owned(),
            ),
            ("delivery", delivery.to_owned()),
            ("rendered_bytes", rendered_bytes.to_string()),
            ("retrieval", "wait".to_owned()),
        ];
        if let Some(optional) = optional {
            attributes.extend(optional.iter().cloned());
        }
        tau_proto::TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE
            .render_attributed(&attributes, body)
            .expect("preview attributes follow the registered envelope contract")
    }
}

/// Canonical body sink that retains at most one publication group's remaining
/// body allowance while still counting the complete rendered output.
struct BoundedEnvelopeBody {
    /// Retained canonical body while escaped bytes fit the limit.
    body: String,
    /// Complete canonical rendered-body byte count.
    rendered_bytes: usize,
    /// Retained body size after exact-close escaping.
    escaped_bytes: usize,
    /// Matched prefix length of the registered exact close sentinel.
    close_prefix_bytes: usize,
    /// Maximum escaped body bytes retained.
    limit: usize,
    /// Whether escaped output exceeded the limit.
    overflowed: bool,
}

impl BoundedEnvelopeBody {
    fn new(limit: usize) -> Self {
        Self {
            body: String::with_capacity(limit),
            rendered_bytes: 0,
            escaped_bytes: 0,
            close_prefix_bytes: 0,
            limit,
            overflowed: false,
        }
    }

    fn finish(&mut self) -> Option<String> {
        (!self.overflowed).then(|| std::mem::take(&mut self.body))
    }

    fn observe_char(&mut self, ch: char) {
        self.rendered_bytes += ch.len_utf8();
        if self.overflowed {
            return;
        }
        self.escaped_bytes += ch.len_utf8();

        let envelope = tau_proto::TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE;
        let close = envelope.exact_close.as_bytes();
        let expected = char::from(close[self.close_prefix_bytes]);
        if ch == expected {
            self.close_prefix_bytes += 1;
            if self.close_prefix_bytes == close.len() {
                self.escaped_bytes += envelope.visible_close.len() - envelope.exact_close.len();
                self.close_prefix_bytes = 0;
            }
        } else {
            self.close_prefix_bytes = usize::from(ch == char::from(close[0]));
        }
        if self.escaped_bytes > self.limit {
            self.body.clear();
            self.overflowed = true;
            return;
        }
        self.body.push(ch);
    }
}

impl fmt::Write for BoundedEnvelopeBody {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for ch in text.chars() {
            self.observe_char(ch);
        }
        Ok(())
    }
}

/// UTF-8-safe canonical text prefix that also counts the complete input.
struct BoundedTextPrefix {
    /// Longest complete-character prefix within the byte limit.
    prefix: String,
    /// Complete canonical input size in bytes.
    total_bytes: usize,
    /// Maximum retained prefix size in bytes.
    limit: usize,
}

impl BoundedTextPrefix {
    fn new(limit: usize) -> Self {
        Self {
            prefix: String::with_capacity(limit),
            total_bytes: 0,
            limit,
        }
    }
}

impl fmt::Write for BoundedTextPrefix {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for ch in text.chars() {
            let bytes = ch.len_utf8();
            self.total_bytes += bytes;
            if self.prefix.len() + bytes <= self.limit {
                self.prefix.push(ch);
            }
        }
        Ok(())
    }
}

/// Bounded summary body plus authenticated envelope metadata.
struct SummaryBody {
    /// Untrusted body prefix selected for summary delivery.
    body: String,
    /// Body size after exact-close escaping.
    escaped_body_bytes: usize,
    /// Additional authenticated summary attributes.
    attributes: Vec<(&'static str, String)>,
}

fn recognized_process_tool(tool_name: &tau_proto::ToolName) -> bool {
    matches!(tool_name.as_str(), "shell" | "shell_command" | "gpt_shell")
}

fn process_outcome_for_result(
    tool_name: &tau_proto::ToolName,
    result: &CborValue,
) -> ProcessOutcomeProjection {
    if !recognized_process_tool(tool_name) {
        return ProcessOutcomeProjection::NotApplicable;
    }
    ShellProcessOutcome::from_result(result).map_or(
        ProcessOutcomeProjection::Unavailable,
        ProcessOutcomeProjection::Available,
    )
}

fn process_outcome_for_error(
    tool_name: &tau_proto::ToolName,
    details: Option<&CborValue>,
) -> ProcessOutcomeProjection {
    if !recognized_process_tool(tool_name) {
        return ProcessOutcomeProjection::NotApplicable;
    }
    details
        .and_then(ShellProcessOutcome::from_error_details)
        .map_or(
            ProcessOutcomeProjection::Unavailable,
            ProcessOutcomeProjection::Available,
        )
}

#[cfg(test)]
mod tests;
