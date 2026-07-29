//! Structured shell outcomes projected from canonical terminal CBOR.

use std::collections::HashSet;

use serde::Serialize;
use tau_proto::{CborValue, Event, ToolResultKind};

/// Source of an authoritative structured shell process outcome.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShellOutcomeSource {
    /// Successful logical tool result payload.
    ToolResult,
    /// Structured details attached to a logical tool error.
    ToolErrorDetails,
}

/// Machine-readable process termination classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShellTerminationReason {
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

/// Fixed true marker for a recorded shell timeout.
#[derive(Clone, Copy)]
struct TimedOut;

impl Serialize for TimedOut {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(true)
    }
}

/// Bounded shell outcome projected directly from a canonical terminal payload.
#[derive(Clone, Serialize)]
pub(super) struct ShellOutcome {
    /// Canonical terminal field that owns the structured payload.
    source: ShellOutcomeSource,
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
    /// Present only as true when the recorded deadline terminated the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    timed_out: Option<TimedOut>,
}

impl ShellOutcome {
    /// Reads a coherent outcome directly from a canonical terminal event.
    pub(super) fn from_terminal_event(event: &Event) -> Option<Self> {
        let (source, value) = match event {
            Event::ProviderToolResult(result) if result.kind == ToolResultKind::Final => {
                (ShellOutcomeSource::ToolResult, &result.result)
            }
            Event::ToolBackgroundResult(result) => (ShellOutcomeSource::ToolResult, &result.result),
            Event::ProviderToolError(error) => (
                ShellOutcomeSource::ToolErrorDetails,
                error.details.as_ref()?,
            ),
            Event::ToolBackgroundError(error) => (
                ShellOutcomeSource::ToolErrorDetails,
                error.details.as_ref()?,
            ),
            _ => return None,
        };
        Self::from_cbor(source, value)
    }

    fn from_cbor(source: ShellOutcomeSource, value: &CborValue) -> Option<Self> {
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
                    // Preserve this behavior; the structural alternative is not semantics-neutral
                    // here. ast-grep-ignore: stringly-typed-match
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
            None if matches!(source, ShellOutcomeSource::ToolResult)
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
                matches!(source, ShellOutcomeSource::ToolErrorDetails)
            }
            ShellTerminationReason::Unknown => true,
        };
        if !coherent {
            return None;
        }
        Some(Self {
            source,
            success: reason == ShellTerminationReason::Exit && exit_code == Some(0),
            termination_reason: reason,
            exit_code,
            signal,
            timed_out: did_time_out.then_some(TimedOut),
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
