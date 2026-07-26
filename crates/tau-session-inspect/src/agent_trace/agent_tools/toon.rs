//! Strict TOON serialization and field-level lossless framing.

use base64::Engine as _;
use serde::Serialize;
use tau_core::AgentJournalSnapshot;
use tau_proto::{AgentId, ToolCallId, ToolName};

use super::super::AgentTraceMode;
use super::{
    CallRecord, Header, OutputProjection, PayloadStore, Status, collect_calls, json_error,
    sort_calls, trace_origin,
};
use crate::InspectError;

/// Readable TOON envelope with exceptional payloads framed independently.
#[derive(Serialize)]
pub(super) struct ToonCallRecord<'a> {
    /// Discriminator for a call item.
    record_type: &'static str,
    /// Trace-relative start time.
    at_us: u64,
    /// Owning agent.
    agent_id: &'a AgentId,
    /// Logical call identifier framing.
    #[serde(flatten)]
    call_id: ToonCallId<'a>,
    /// Model-visible tool name.
    tool: &'a ToolName,
    /// Optional command framing.
    #[serde(flatten)]
    command: ToonCommand<'a>,
    /// Argument framing.
    #[serde(flatten)]
    arguments: ToonArguments<'a>,
    /// Terminal state.
    status: Status,
    /// Relative terminal duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_us: Option<u64>,
    /// Mode-specific output framing.
    #[serde(flatten)]
    output: ToonOutput<'a>,
}

/// Mutually exclusive TOON call-ID fields.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCallId<'a> {
    /// Direct safe call ID.
    Direct {
        /// Provider-assigned logical call identifier.
        call_id: &'a ToolCallId,
    },
    /// Base64 UTF-8 call ID containing unsafe controls.
    Base64 {
        /// Base64-encoded UTF-8 call identifier.
        call_id_base64: String,
    },
}

/// Mutually exclusive optional TOON command fields.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCommand<'a> {
    /// No shell command.
    Absent {},
    /// Direct safe command.
    Direct {
        /// Direct shell command.
        command: &'a str,
    },
    /// Base64 UTF-8 command containing unsafe controls.
    Base64 {
        /// Base64-encoded UTF-8 shell command.
        command_base64: String,
    },
}

/// Mutually exclusive TOON argument fields.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonArguments<'a> {
    /// Direct ordinary safe arguments.
    Direct {
        /// Complete ordinary argument value.
        arguments: &'a serde_json::Value,
    },
    /// Base64 compact JSON for tagged-CBOR or control-bearing arguments.
    JsonBase64 {
        /// Base64-encoded compact JSON argument value.
        arguments_json_base64: String,
    },
}

/// Mutually exclusive TOON output fields.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonOutput<'a> {
    /// Incomplete full-mode call without output.
    Absent {},
    /// Lite output counters.
    Counts {
        /// Rendered UTF-8 byte count.
        output_bytes: usize,
        /// Rendered logical-line count.
        output_lines: usize,
    },
    /// Direct safe full output.
    Direct {
        /// Complete normalized output.
        output: &'a str,
    },
    /// Base64 UTF-8 full output containing unsafe controls.
    Base64 {
        /// Base64-encoded UTF-8 normalized output.
        output_base64: String,
    },
}
impl<'a> CallRecord<'a> {
    /// Preserves the readable call envelope while independently framing
    /// payloads that direct TOON cannot represent safely and exactly.
    pub(super) fn toon_projection(&'a self) -> Result<ToonCallRecord<'a>, InspectError> {
        let CallRecord {
            record_type,
            at_us,
            agent_id,
            call_id,
            tool,
            command,
            arguments,
            status,
            duration_us,
            output,
        } = self;
        let call_id = if call_id.as_str().chars().any(is_unsafe_toon_char) {
            ToonCallId::Base64 {
                call_id_base64: base64::engine::general_purpose::STANDARD
                    .encode(call_id.as_str().as_bytes()),
            }
        } else {
            ToonCallId::Direct { call_id }
        };
        let command = match command.as_deref() {
            None => ToonCommand::Absent {},
            Some(command) if command.chars().any(is_unsafe_toon_char) => ToonCommand::Base64 {
                command_base64: base64::engine::general_purpose::STANDARD
                    .encode(command.as_bytes()),
            },
            Some(command) => ToonCommand::Direct { command },
        };
        let arguments_value = arguments.value();
        let arguments = if arguments.is_tagged() || contains_unsafe_toon_string(arguments_value) {
            ToonArguments::JsonBase64 {
                arguments_json_base64: base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(arguments_value).map_err(json_error)?),
            }
        } else {
            ToonArguments::Direct {
                arguments: arguments_value,
            }
        };
        let output = match output {
            OutputProjection::Counts {
                output_bytes,
                output_lines,
            } => ToonOutput::Counts {
                output_bytes: *output_bytes,
                output_lines: *output_lines,
            },
            OutputProjection::Full { output } if output.chars().any(is_unsafe_toon_char) => {
                ToonOutput::Base64 {
                    output_base64: base64::engine::general_purpose::STANDARD
                        .encode(output.as_bytes()),
                }
            }
            OutputProjection::Full { output } => ToonOutput::Direct { output },
            OutputProjection::Absent {} => ToonOutput::Absent {},
        };
        Ok(ToonCallRecord {
            record_type,
            at_us: *at_us,
            agent_id,
            call_id,
            tool,
            command,
            arguments,
            status: *status,
            duration_us: *duration_us,
            output,
        })
    }
}

/// Writes one strict TOON document with a counted `calls` array.
pub(super) fn write(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: AgentTraceMode,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    let origin = trace_origin(snapshot)?;
    let header = Header::new(root_agent_id, snapshot, mode);
    writeln!(
        output,
        "{}",
        serde_toon::to_string(&header).map_err(toon_error)?
    )?;

    let mut payloads = PayloadStore::new()?;
    let mut calls = collect_calls(snapshot, mode, &mut payloads)?;
    sort_calls(&mut calls);
    writeln!(output, "calls[{}]:", calls.len())?;
    for call in &calls {
        let record = call.project(origin, mode, &mut payloads)?;
        let encoded = serde_toon::to_string(&record.toon_projection()?).map_err(toon_error)?;
        for (index, line) in encoded.lines().enumerate() {
            writeln!(
                output,
                "{}{}",
                if index == 0 { "  - " } else { "    " },
                line
            )?;
        }
    }
    Ok(())
}

/// Returns whether direct TOON would emit a payload control byte raw.
fn contains_unsafe_toon_string(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => value.chars().any(is_unsafe_toon_char),
        serde_json::Value::Array(values) => values.iter().any(contains_unsafe_toon_string),
        serde_json::Value::Object(entries) => entries.iter().any(|(key, value)| {
            key.chars().any(is_unsafe_toon_char) || contains_unsafe_toon_string(value)
        }),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

/// TOON safely escapes these three controls; every other C0/C1 control requires
/// Base64 field framing.
fn is_unsafe_toon_char(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

/// Wraps TOON serialization failures as trace projection errors.
fn toon_error(error: serde_toon::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize compact agent tool TOON: {error}"
    )))
}
