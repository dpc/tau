//! Strict process outcomes projected from canonical shell terminal CBOR.
//!
//! This parser owns payload and terminal-family validation only. Callers own
//! tool-name eligibility; the harness invokes it only for recognized shell
//! tools.

use std::collections::HashSet;

use serde::Serialize;

use crate::{CborValue, Event, ToolResultKind};

/// Canonical terminal field that supplied a coherent process outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellProcessOutcomeSource {
    /// Successful logical tool result payload.
    ToolResult,
    /// Structured details attached to a logical tool error.
    ToolErrorDetails,
}

/// Machine-readable process termination classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTerminationReason {
    /// A process exited with an exit code.
    Exit,
    /// The command deadline terminated the process.
    Timeout,
    /// A platform signal terminated the process.
    Signal,
    /// The extension could not start the process.
    StartError,
    /// The producer explicitly could not classify termination.
    Unknown,
}

/// Bounded process outcome read strictly from canonical shell terminal fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShellProcessOutcome {
    /// Canonical terminal field that supplied the structured payload.
    source: ShellProcessOutcomeSource,
    /// Whether the process is known to have exited normally with code zero.
    success: bool,
    /// Machine-readable process termination classification.
    termination_reason: ShellTerminationReason,
    /// Exact process exit code, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// Exact platform signal number, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    /// True only when the recorded deadline terminated the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    timed_out: Option<bool>,
}

impl ShellProcessOutcome {
    /// Canonical terminal field that supplied this validated projection.
    #[must_use]
    pub const fn source(self) -> ShellProcessOutcomeSource {
        self.source
    }

    /// Whether the process is known to have exited normally with code zero.
    #[must_use]
    pub const fn success(self) -> bool {
        self.success
    }

    /// Validated machine-readable termination classification.
    #[must_use]
    pub const fn termination_reason(self) -> ShellTerminationReason {
        self.termination_reason
    }

    /// Exact process exit code, when coherent for this classification.
    #[must_use]
    pub const fn exit_code(self) -> Option<i32> {
        self.exit_code
    }

    /// Exact platform signal, when recorded coherently.
    #[must_use]
    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    /// True only when the recorded deadline terminated the process.
    #[must_use]
    pub fn timed_out(self) -> bool {
        self.timed_out == Some(true)
    }

    /// Read a coherent outcome directly from one canonical terminal event.
    ///
    /// This validates the canonical terminal family and structured field, not
    /// whether the event's tool name denotes a shell implementation.
    #[must_use]
    pub fn from_terminal_event(event: &Event) -> Option<Self> {
        match event {
            Event::ProviderToolResult(result) if result.kind == ToolResultKind::Final => {
                Self::from_result(&result.result)
            }
            Event::ToolBackgroundResult(result) => Self::from_result(&result.result),
            Event::ProviderToolError(error) => Self::from_error_details(error.details.as_ref()?),
            Event::ToolBackgroundError(error) => Self::from_error_details(error.details.as_ref()?),
            _ => None,
        }
    }

    /// Read a coherent outcome from a canonical logical-result payload.
    #[must_use]
    pub fn from_result(value: &CborValue) -> Option<Self> {
        Self::from_cbor(ShellProcessOutcomeSource::ToolResult, value)
    }

    /// Read a coherent outcome from canonical logical-error details.
    #[must_use]
    pub fn from_error_details(value: &CborValue) -> Option<Self> {
        Self::from_cbor(ShellProcessOutcomeSource::ToolErrorDetails, value)
    }

    /// Read a coherent outcome from the selected canonical terminal field.
    ///
    /// Recognized fields are `status` and `signal` as signed 32-bit integers,
    /// `timed_out` as a boolean, and `termination_reason` as one of `exit`,
    /// `timeout`, `signal`, `start_error`, or `unknown`. Unknown fields are
    /// ignored. A duplicate or malformed recognized field, or a contradictory
    /// combination, makes the projection unavailable.
    ///
    /// For legacy logical-result payloads only, a lone `status` infers `exit`.
    /// Every error-details payload and every other result shape must carry an
    /// explicit coherent `termination_reason`.
    #[must_use]
    pub fn from_cbor(source: ShellProcessOutcomeSource, value: &CborValue) -> Option<Self> {
        let CborValue::Map(entries) = value else {
            return None;
        };
        let mut exit_code = None;
        let mut signal = None;
        let mut timed_out = None;
        let mut reason = None;
        let mut recognized = HashSet::with_capacity(4);
        for (key, value) in entries {
            let CborValue::Text(key) = key else {
                continue;
            };
            let key = key.as_str();
            if matches!(
                key,
                "status" | "signal" | "timed_out" | "termination_reason"
            ) && !recognized.insert(key)
            {
                return None;
            }
            match key {
                "status" => exit_code = Some(cbor_i32(value)?),
                "signal" => signal = Some(cbor_i32(value)?),
                "timed_out" => {
                    let CborValue::Bool(value) = value else {
                        return None;
                    };
                    timed_out = Some(*value);
                }
                "termination_reason" => {
                    let CborValue::Text(value) = value else {
                        return None;
                    };
                    reason = Some(match value.as_str() {
                        "exit" => ShellTerminationReason::Exit,
                        "timeout" => ShellTerminationReason::Timeout,
                        "signal" => ShellTerminationReason::Signal,
                        "start_error" => ShellTerminationReason::StartError,
                        "unknown" => ShellTerminationReason::Unknown,
                        _ => return None,
                    });
                }
                _ => {}
            }
        }
        let reason = match reason {
            Some(reason) => reason,
            None if source == ShellProcessOutcomeSource::ToolResult
                && exit_code.is_some()
                && timed_out.is_none()
                && signal.is_none() =>
            {
                ShellTerminationReason::Exit
            }
            None => return None,
        };
        let did_time_out = timed_out == Some(true);
        let coherent = match reason {
            ShellTerminationReason::Exit => {
                exit_code.is_some() && signal.is_none() && !did_time_out
            }
            ShellTerminationReason::Timeout => did_time_out,
            ShellTerminationReason::Signal => signal.is_some() && !did_time_out,
            ShellTerminationReason::StartError => {
                source == ShellProcessOutcomeSource::ToolErrorDetails
            }
            ShellTerminationReason::Unknown => true,
        };
        coherent.then_some(Self {
            source,
            success: reason == ShellTerminationReason::Exit && exit_code == Some(0),
            termination_reason: reason,
            exit_code,
            signal,
            timed_out: did_time_out.then_some(true),
        })
    }
}

fn cbor_i32(value: &CborValue) -> Option<i32> {
    let CborValue::Integer(value) = value else {
        return None;
    };
    let value: i128 = (*value).into();
    i32::try_from(value).ok()
}

#[cfg(test)]
mod tests;
