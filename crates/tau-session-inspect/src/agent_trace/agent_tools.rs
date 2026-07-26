//! Compact model-visible tool-call trace projection.

mod payload_store;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use payload_store::{Endpoint, PayloadStore};
use serde::Serialize;
use tau_core::{AgentJournalSnapshot, PersistedAgentEventSeq};
use tau_proto::{AgentId, CborValue, ContextItem, Event, ToolCallId, ToolName, UnixMicros};

use crate::InspectError;

const SCHEMA: &str = "tau.agent_tools";

/// Output payload policy for a compact agent-tool trace.
#[derive(Clone, Copy)]
enum OutputMode {
    /// Emit byte and logical-line counts only.
    Counts,
    /// Emit complete rendered output text.
    Full,
}

impl OutputMode {
    /// Returns the stable header label.
    fn label(self) -> &'static str {
        match self {
            Self::Counts => "counts",
            Self::Full => "full",
        }
    }
}

/// First-line metadata for a compact tool trace.
#[derive(Serialize)]
struct Header<'a> {
    /// Stable schema identifier.
    schema: &'static str,
    /// Initial internal schema revision.
    schema_version: u32,
    /// Discriminator for the first line.
    record_type: &'static str,
    /// Requested workflow root.
    root_agent_id: &'a AgentId,
    /// Deterministically selected journal identities.
    included_agent_ids: Vec<&'a AgentId>,
    /// Whether call records contain output text or counts.
    output: &'static str,
    /// Unit used by all relative timing fields.
    time_unit: &'static str,
}

/// Closed terminal state labels in the stable output schema.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    /// No durable terminal fact was observed.
    Incomplete,
    /// The call completed successfully.
    Ok,
    /// The call failed.
    Error,
    /// The call was cancelled.
    Cancelled,
}

/// Mutually exclusive output fields for one call record.
#[derive(Serialize)]
#[serde(untagged)]
enum OutputProjection {
    /// Lite output counters.
    Counts {
        /// Rendered UTF-8 byte count.
        output_bytes: usize,
        /// Rendered logical-line count.
        output_lines: usize,
    },
    /// Complete full output.
    Full {
        /// Provider-facing rendered text.
        output: String,
    },
    /// An incomplete call in full mode has no output field.
    Absent {},
}

/// One model-visible tool call and its eventual durable outcome.
#[derive(Serialize)]
struct CallRecord<'a> {
    /// Discriminator for a call line.
    record_type: &'static str,
    /// Offset from the earliest included journal occurrence.
    at_us: u64,
    /// Owning durable agent.
    agent_id: &'a AgentId,
    /// Provider-assigned logical call identifier.
    call_id: &'a ToolCallId,
    /// Model-visible tool name.
    tool: &'a ToolName,
    /// Direct command projection for shell-like tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// Complete provider-declared arguments.
    arguments: serde_json::Value,
    /// Terminal state.
    status: Status,
    /// Time from provider declaration to the durable terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_us: Option<u64>,
    /// Mode-specific output projection.
    #[serde(flatten)]
    output: OutputProjection,
}

/// Correlated metadata retained in heap until chronological serialization.
struct Call {
    /// Owning durable agent.
    agent_id: AgentId,
    /// Provider-assigned logical call identifier.
    call_id: ToolCallId,
    /// Model-visible tool name.
    tool: ToolName,
    /// Staged shell command, when this is a shell surface.
    command: Option<Endpoint>,
    /// Staged concise/lossless JSON arguments.
    arguments: Endpoint,
    /// Declaration timestamp.
    started_at: UnixMicros,
    /// Declaration sequence for deterministic timestamp ties.
    started_seq: PersistedAgentEventSeq,
    /// Provider item position for calls sharing a journal occurrence.
    item_index: usize,
    /// Latest eligible terminal outcome.
    terminal: Option<Terminal>,
}

/// One durable terminal outcome.
struct Terminal {
    /// Terminal timestamp.
    at: UnixMicros,
    /// Closed status label.
    status: Status,
    /// Mode-specific staged output.
    output: TerminalOutput,
}

/// Terminal output retained without lite payload bodies.
enum TerminalOutput {
    /// Lite output statistics.
    Counts { bytes: usize, lines: usize },
    /// Full output text location.
    Full(Endpoint),
}

/// Writes the lite compact model-visible tool-call overview.
pub(super) fn write_lite_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    write_jsonl(root_agent_id, snapshot, OutputMode::Counts, output)
}

/// Writes the full compact model-visible tool-call overview.
pub(super) fn write_full_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    write_jsonl(root_agent_id, snapshot, OutputMode::Full, output)
}

/// Writes one selected compact JSON Lines projection.
fn write_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: OutputMode,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    let origin = trace_origin(snapshot)?;
    serde_json::to_writer(
        &mut *output,
        &Header {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id,
            included_agent_ids: snapshot.agent_ids().iter().collect(),
            output: mode.label(),
            time_unit: "microseconds",
        },
    )
    .map_err(json_error)?;
    writeln!(output)?;

    let mut payloads = PayloadStore::new()?;
    let mut calls = collect_calls(snapshot, mode, &mut payloads)?;
    calls.sort_by(|left, right| {
        (
            left.started_at,
            &left.agent_id,
            left.started_seq.get(),
            left.item_index,
        )
            .cmp(&(
                right.started_at,
                &right.agent_id,
                right.started_seq.get(),
                right.item_index,
            ))
    });
    for call in &calls {
        let arguments: serde_json::Value = payloads.load(call.arguments)?;
        let command = call
            .command
            .map(|endpoint| payloads.load(endpoint))
            .transpose()?;
        let (status, duration_us, projected_output) = match &call.terminal {
            Some(terminal) => {
                let output = match terminal.output {
                    TerminalOutput::Counts { bytes, lines } => OutputProjection::Counts {
                        output_bytes: bytes,
                        output_lines: lines,
                    },
                    TerminalOutput::Full(endpoint) => OutputProjection::Full {
                        output: payloads.load(endpoint)?,
                    },
                };
                (
                    terminal.status,
                    terminal.at.get().checked_sub(call.started_at.get()),
                    output,
                )
            }
            None => (
                Status::Incomplete,
                None,
                match mode {
                    OutputMode::Counts => OutputProjection::Counts {
                        output_bytes: 0,
                        output_lines: 0,
                    },
                    OutputMode::Full => OutputProjection::Absent {},
                },
            ),
        };
        serde_json::to_writer(
            &mut *output,
            &CallRecord {
                record_type: "call",
                at_us: call.started_at.get().saturating_sub(origin.get()),
                agent_id: &call.agent_id,
                call_id: &call.call_id,
                tool: &call.tool,
                command,
                arguments,
                status,
                duration_us,
                output: projected_output,
            },
        )
        .map_err(json_error)?;
        writeln!(output)?;
    }
    Ok(())
}

/// Finds the earliest included journal timestamp.
fn trace_origin(snapshot: &AgentJournalSnapshot) -> Result<UnixMicros, InspectError> {
    let mut origin = None;
    for agent_id in snapshot.agent_ids() {
        for record in snapshot.records(agent_id)? {
            let recorded_at = record?.recorded_at;
            origin = Some(origin.map_or(recorded_at, |current: UnixMicros| {
                std::cmp::min(current, recorded_at)
            }));
        }
    }
    Ok(origin.expect("validated snapshots contain nonempty journals"))
}

/// Collects provider-declared calls and correlates eligible durable terminals.
fn collect_calls(
    snapshot: &AgentJournalSnapshot,
    mode: OutputMode,
    payloads: &mut PayloadStore,
) -> Result<Vec<Call>, InspectError> {
    let mut calls = Vec::new();
    for agent_id in snapshot.agent_ids() {
        let mut foreground = BTreeMap::new();
        let mut background = BTreeMap::new();
        for record in snapshot.records(agent_id)? {
            let record = record?;
            if let Event::ProviderResponseFinished(finished) = &record.event {
                for (item_index, item) in finished.output_items.iter().enumerate() {
                    let ContextItem::ToolCall(call) = item else {
                        continue;
                    };
                    if foreground.contains_key(&call.call_id) {
                        continue;
                    }
                    let index = calls.len();
                    foreground.insert(call.call_id.clone(), index);
                    calls.push(Call {
                        agent_id: agent_id.clone(),
                        call_id: call.call_id.clone(),
                        tool: call.name.clone(),
                        command: shell_command(&call.name, &call.arguments)
                            .as_ref()
                            .map(|command| payloads.append(command))
                            .transpose()?,
                        arguments: payloads.append(&faithful_arguments(&call.arguments))?,
                        started_at: record.recorded_at,
                        started_seq: record.seq,
                        item_index,
                        terminal: None,
                    });
                }
                continue;
            }
            apply_terminal(
                &record.event,
                record.recorded_at,
                mode,
                &mut foreground,
                &mut background,
                &mut calls,
                payloads,
            )?;
        }
    }
    Ok(calls)
}

/// Applies one eligible durable terminal and closes the active ID when final.
fn apply_terminal(
    event: &Event,
    at: UnixMicros,
    mode: OutputMode,
    foreground: &mut BTreeMap<ToolCallId, usize>,
    background: &mut BTreeMap<ToolCallId, usize>,
    calls: &mut [Call],
    payloads: &mut PayloadStore,
) -> Result<(), InspectError> {
    let (index, status, rendered) = match event {
        Event::ProviderToolResult(result) => {
            if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                if background.contains_key(&result.call_id) {
                    return Err(InspectError::Trace(crate::AgentTraceError::Projection(
                        format!(
                            "ambiguous concurrent background tool call ID `{}`",
                            result.call_id
                        ),
                    )));
                }
                if let Some(index) = foreground.remove(&result.call_id) {
                    background.insert(result.call_id.clone(), index);
                }
                return Ok(());
            }
            (
                foreground.remove(&result.call_id),
                Status::Ok,
                tau_proto::ToolResponse::from_cbor(&result.result).render(),
            )
        }
        Event::ToolBackgroundResult(result) => (
            background.remove(&result.call_id),
            Status::Ok,
            tau_proto::ToolResponse::from_cbor(&result.result).render(),
        ),
        Event::ProviderToolError(error) => (
            foreground.remove(&error.call_id),
            Status::Error,
            render_error(&error.message, error.details.as_ref()),
        ),
        Event::ToolBackgroundError(error) => (
            background.remove(&error.call_id),
            Status::Error,
            render_error(&error.message, error.details.as_ref()),
        ),
        Event::ToolCancelled(cancelled) => (
            foreground.remove(&cancelled.call_id),
            Status::Cancelled,
            render_cancelled(),
        ),
        _ => return Ok(()),
    };
    let Some(index) = index else {
        return Ok(());
    };
    let output = match mode {
        OutputMode::Counts => TerminalOutput::Counts {
            bytes: rendered.len(),
            lines: rendered.lines().count(),
        },
        OutputMode::Full => TerminalOutput::Full(payloads.append(&rendered)?),
    };
    calls[index].terminal = Some(Terminal { at, status, output });
    Ok(())
}

/// Renders an error exactly like provider replay: an `error` header followed by
/// normalized structured details.
fn render_error(message: &str, details: Option<&CborValue>) -> String {
    let mut response = tau_proto::ToolResponse::from_cbor(details.unwrap_or(&CborValue::Null));
    response.headers.insert(
        0,
        tau_proto::ToolResponseHeader {
            key: "error".to_owned(),
            value: message.to_owned(),
        },
    );
    response.render()
}

/// Renders the durable cancellation reason used by transcript reconstruction.
fn render_cancelled() -> String {
    tau_proto::ToolResponse {
        raw: CborValue::Null,
        headers: vec![tau_proto::ToolResponseHeader {
            key: "cancelled".to_owned(),
            value: "cancelled".to_owned(),
        }],
        body: String::new(),
    }
    .render()
}

/// Extracts a direct shell command while retaining complete arguments.
fn shell_command(tool: &ToolName, arguments: &CborValue) -> Option<String> {
    if !matches!(tool.as_str(), "shell" | "shell_command" | "gpt_shell") {
        return None;
    }
    let CborValue::Text(command) = tau_proto::cbor_field(arguments, "command")? else {
        return None;
    };
    Some(command.clone())
}

/// Uses concise JSON only when every nested CBOR value is represented
/// faithfully; otherwise uses Tau's lossless tagged-CBOR JSON for the complete
/// argument value.
fn faithful_arguments(value: &CborValue) -> serde_json::Value {
    faithful_json(value).unwrap_or_else(|| crate::lossless_json::typed_cbor(value))
}

/// Recursively converts the JSON-compatible subset without coercion or loss.
fn faithful_json(value: &CborValue) -> Option<serde_json::Value> {
    match value {
        CborValue::Null => Some(serde_json::Value::Null),
        CborValue::Bool(value) => Some((*value).into()),
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            if let Ok(value) = i64::try_from(value) {
                Some(value.into())
            } else {
                u64::try_from(value).ok().map(Into::into)
            }
        }
        CborValue::Float(value) if value.is_finite() => {
            serde_json::Number::from_f64(*value).map(Into::into)
        }
        CborValue::Text(value) => Some(value.clone().into()),
        CborValue::Array(values) => values
            .iter()
            .map(faithful_json)
            .collect::<Option<Vec<_>>>()
            .map(Into::into),
        CborValue::Map(entries) => {
            let mut object = serde_json::Map::new();
            for (key, value) in entries {
                let CborValue::Text(key) = key else {
                    return None;
                };
                if object.contains_key(key) {
                    return None;
                }
                object.insert(key.clone(), faithful_json(value)?);
            }
            Some(object.into())
        }
        CborValue::Float(_) | CborValue::Bytes(_) | CborValue::Tag(_, _) | _ => None,
    }
}

/// Wraps JSON serialization failures as trace projection errors.
fn json_error(error: serde_json::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize compact agent tool trace: {error}"
    )))
}
