//! TOON serialization for the compact explicit-observation projection.

use base64::Engine as _;
use serde::Serialize;

use super::{
    ActivationRecord, CallLifecycleRecord, CallOutputRecord, CallRecord, CallStatus, Header,
    IncompleteCallStatus, LocalResolution, Record, RelationshipRecord, UnavailableResolution,
};
use crate::InspectError;

/// Writes one TOON document containing the same semantic records as JSONL.
pub(super) fn write(
    header: &Header<'_>,
    records: Vec<Record>,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    writeln!(
        output,
        "{}",
        serde_toon::to_string(header).map_err(toon_error)?
    )?;
    writeln!(output, "records[{}]:", records.len())?;
    for record in records {
        let encoded = serde_toon::to_string(&ToonRecord::from(record)).map_err(toon_error)?;
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

/// Typed TOON record set with explicit direct/Base64 payload variants.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonRecord {
    /// A provider-declared tool call with TOON-safe content fields.
    Call(ToonCallRecord),
    /// A content-free activation record shared with JSONL.
    Activation(ActivationRecord),
    /// A content-free relationship record shared with JSONL.
    Relationship(RelationshipRecord),
}

impl From<Record> for ToonRecord {
    fn from(record: Record) -> Self {
        match record {
            Record::Call(call) => Self::Call(call.into()),
            Record::Activation(activation) => Self::Activation(activation),
            Record::Relationship(relationship) => Self::Relationship(relationship),
        }
    }
}

/// Call record adapted structurally for TOON-safe payload framing.
#[derive(Serialize)]
struct ToonCallRecord {
    /// Fixed `call` discriminator.
    record_type: &'static str,
    /// Journal-local agent owner.
    agent_id: tau_proto::AgentId,
    /// Exact provider declaration occurrence.
    call: tau_proto::ToolCallRef,
    /// Direct or Base64 display call ID.
    #[serde(flatten)]
    call_id: ToonCallId,
    /// Provider-declared tool name.
    tool: tau_proto::ToolName,
    /// Direct JSON or whole-JSON Base64 arguments.
    #[serde(flatten)]
    arguments: ToonArguments,
    /// Optional direct or Base64 command.
    #[serde(flatten)]
    command: ToonCommand,
    /// Qualified declaration-to-dispatch interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    declaration_to_dispatch_us: Option<u64>,
    /// Qualified dispatch-to-background interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch_to_backgrounded_us: Option<u64>,
    /// Semantic lifecycle and output variant.
    #[serde(flatten)]
    lifecycle: ToonCallLifecycle,
}

impl From<CallRecord> for ToonCallRecord {
    fn from(call: CallRecord) -> Self {
        let CallRecord {
            record_type,
            agent_id,
            call,
            call_id,
            tool,
            arguments,
            command,
            declaration_to_dispatch_us,
            dispatch_to_backgrounded_us,
            lifecycle,
        } = call;
        Self {
            record_type,
            agent_id,
            call,
            call_id: ToonCallId::new(call_id),
            tool,
            arguments: ToonArguments::new(arguments),
            command: ToonCommand::new(command),
            declaration_to_dispatch_us,
            dispatch_to_backgrounded_us,
            lifecycle: lifecycle.into(),
        }
    }
}

/// TOON-safe representation of the provider display call ID.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCallId {
    /// Grammar-safe value under its canonical key.
    Direct {
        /// Provider display call ID.
        call_id: String,
    },
    /// Whole UTF-8 value encoded as standard padded Base64.
    Base64 {
        /// Base64-encoded provider display call ID.
        call_id_base64: String,
    },
}

impl ToonCallId {
    fn new(value: String) -> Self {
        if contains_unsafe_string(&value) {
            Self::Base64 {
                call_id_base64: encode_bytes(value.as_bytes()),
            }
        } else {
            Self::Direct { call_id: value }
        }
    }
}

/// TOON-safe representation of an optional extracted command.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCommand {
    /// No extracted command.
    None {},
    /// Grammar-safe command.
    Direct {
        /// Extracted command.
        command: String,
    },
    /// Whole UTF-8 command encoded as standard padded Base64.
    Base64 {
        /// Base64-encoded extracted command.
        command_base64: String,
    },
}

impl ToonCommand {
    fn new(value: Option<String>) -> Self {
        match value {
            None => Self::None {},
            Some(value) if contains_unsafe_string(&value) => Self::Base64 {
                command_base64: encode_bytes(value.as_bytes()),
            },
            Some(command) => Self::Direct { command },
        }
    }
}

/// TOON-safe representation of the complete argument value.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonArguments {
    /// Arguments whose strings are all grammar-safe.
    Direct {
        /// Complete JSON/tagged-CBOR argument value.
        arguments: serde_json::Value,
    },
    /// Complete compact JSON argument value encoded as Base64.
    Base64 {
        /// Base64-encoded compact JSON argument value.
        arguments_json_base64: String,
    },
}

impl ToonArguments {
    fn new(arguments: serde_json::Value) -> Self {
        if arguments.get("type").is_some() || contains_unsafe_toon_string(&arguments) {
            Self::Base64 {
                arguments_json_base64: encode_bytes(
                    &serde_json::to_vec(&arguments)
                        .expect("serde_json::Value serialization is infallible"),
                ),
            }
        } else {
            Self::Direct { arguments }
        }
    }
}

/// Semantic call lifecycle adapted only where output strings need framing.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCallLifecycle {
    /// No selected canonical terminal.
    Incomplete {
        /// Fixed incomplete status.
        status: IncompleteCallStatus,
    },
    /// Terminal reference exists but is unavailable to this journal.
    Unresolved {
        /// Fixed incomplete status.
        status: IncompleteCallStatus,
        /// Referenced terminal identity.
        terminal: tau_proto::ObservationId,
        /// Producer-classified cause.
        cause: tau_proto::ToolTerminalCause,
        /// Fixed unavailable marker.
        terminal_resolution: UnavailableResolution,
    },
    /// Fully selected local canonical terminal.
    Resolved {
        /// Terminal status.
        status: CallStatus,
        /// Canonical terminal identity.
        terminal: tau_proto::ObservationId,
        /// Producer-classified cause.
        cause: tau_proto::ToolTerminalCause,
        /// Fixed local marker.
        terminal_resolution: LocalResolution,
        /// Qualified dispatch-to-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        dispatch_to_terminal_us: Option<u64>,
        /// Qualified background-to-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        backgrounded_to_terminal_us: Option<u64>,
        /// Mode-specific record-owned output.
        #[serde(flatten)]
        output: ToonCallOutput,
    },
}

impl From<CallLifecycleRecord> for ToonCallLifecycle {
    fn from(lifecycle: CallLifecycleRecord) -> Self {
        match lifecycle {
            CallLifecycleRecord::Incomplete { status } => Self::Incomplete { status },
            CallLifecycleRecord::Unresolved {
                status,
                terminal,
                cause,
                terminal_resolution,
            } => Self::Unresolved {
                status,
                terminal,
                cause,
                terminal_resolution,
            },
            CallLifecycleRecord::Resolved {
                status,
                terminal,
                cause,
                terminal_resolution,
                dispatch_to_terminal_us,
                backgrounded_to_terminal_us,
                output,
            } => Self::Resolved {
                status,
                terminal,
                cause,
                terminal_resolution,
                dispatch_to_terminal_us,
                backgrounded_to_terminal_us,
                output: output.into(),
            },
        }
    }
}

/// Mode-specific output ownership and clipping metadata.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonCallOutput {
    /// No record-owned projected output.
    None {},
    /// Bounded output with complete counts.
    Lite {
        /// Complete byte count.
        output_bytes: usize,
        /// Complete line count.
        output_lines: usize,
        /// Direct or Base64 bounded output.
        #[serde(flatten)]
        output: ToonOutput,
        /// Whether bounded output is complete.
        output_complete: bool,
    },
    /// Complete output.
    Full {
        /// Direct or Base64 complete output.
        #[serde(flatten)]
        output: ToonOutput,
        /// Fixed `true` completeness marker.
        output_complete: super::CompleteOutput,
    },
}

impl From<CallOutputRecord> for ToonCallOutput {
    fn from(output: CallOutputRecord) -> Self {
        match output {
            CallOutputRecord::None {} => Self::None {},
            CallOutputRecord::Lite {
                output_bytes,
                output_lines,
                output,
                output_complete,
            } => Self::Lite {
                output_bytes,
                output_lines,
                output: ToonOutput::new(output),
                output_complete,
            },
            CallOutputRecord::Full {
                output,
                output_complete,
            } => Self::Full {
                output: ToonOutput::new(output),
                output_complete,
            },
        }
    }
}

/// Direct or whole-field Base64 terminal output.
#[derive(Serialize)]
#[serde(untagged)]
enum ToonOutput {
    /// Grammar-safe output.
    Direct {
        /// Rendered terminal output.
        output: String,
    },
    /// Whole UTF-8 output encoded as standard padded Base64.
    Base64 {
        /// Base64-encoded rendered terminal output.
        output_base64: String,
    },
}

impl ToonOutput {
    fn new(output: String) -> Self {
        if contains_unsafe_string(&output) {
            Self::Base64 {
                output_base64: encode_bytes(output.as_bytes()),
            }
        } else {
            Self::Direct { output }
        }
    }
}

fn contains_unsafe_string(value: &str) -> bool {
    value.chars().any(is_unsafe_toon_char)
}

fn encode_bytes(value: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

/// Returns whether a JSON payload contains a string that TOON cannot emit
/// safely and round-trip exactly.
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

/// TOON escapes line-layout controls but requires Base64 framing for every
/// other C0/C1 control.
fn is_unsafe_toon_char(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

fn toon_error(error: serde_toon::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize compact agent tool TOON: {error}"
    )))
}
