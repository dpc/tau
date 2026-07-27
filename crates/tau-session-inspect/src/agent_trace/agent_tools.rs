//! Compact model-visible tool-call trace projection.

mod payload_store;
mod toon;

// Keep test-only modules after every production module.
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use payload_store::{Endpoint, PayloadStore};
use serde::{Deserialize, Serialize};
use tau_core::{AgentJournalSnapshot, PersistedAgentEventSeq};
use tau_proto::{AgentId, CborValue, ContextItem, Event, ToolCallId, ToolName, UnixMicros};

use crate::InspectError;

const SCHEMA: &str = "tau.agent_tools";
/// Maximum event-native output context retained by one lite call.
const LITE_OUTPUT_BYTES: usize = 4 * 1024;

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
    /// Whether call records contain bounded or complete output text.
    output: &'static str,
    /// Unit used by all relative timing fields.
    time_unit: &'static str,
}

impl<'a> Header<'a> {
    /// Builds shared metadata for either compact encoding.
    fn new(
        root_agent_id: &'a AgentId,
        snapshot: &'a AgentJournalSnapshot,
        mode: super::AgentTraceMode,
    ) -> Self {
        Self {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id,
            included_agent_ids: snapshot.agent_ids().collect(),
            output: mode.label(),
            time_unit: "microseconds",
        }
    }
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
    /// A terminal call's rendered output and exact metrics.
    Present {
        /// Complete rendered projection's UTF-8 byte count.
        output_bytes: usize,
        /// Complete rendered projection's logical-line count.
        output_lines: usize,
        /// Complete or bounded provider-facing rendered text.
        output: String,
        /// Whether `output` contains the complete rendered projection.
        output_complete: bool,
    },
    /// A call without a durable terminal has no output or metrics.
    Absent {
        /// Incomplete calls cannot contain complete terminal output.
        output_complete: bool,
    },
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
    arguments: ArgumentsProjection,
    /// Terminal state.
    status: Status,
    /// Time from provider declaration to the durable terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_us: Option<u64>,
    /// Mode-specific output projection.
    #[serde(flatten)]
    output: OutputProjection,
}

/// Semantic argument projection used by JSONL and TOON.
#[derive(Serialize)]
#[serde(untagged)]
enum ArgumentsProjection {
    /// Concise ordinary JSON value.
    Ordinary(serde_json::Value),
    /// Complete Tau tagged-CBOR JSON value.
    TaggedCbor(serde_json::Value),
}

/// Correlation-staging form that preserves the semantic argument variant.
#[derive(Deserialize, Serialize)]
enum StagedArguments {
    /// Concise ordinary JSON value.
    Ordinary(serde_json::Value),
    /// Complete Tau tagged-CBOR JSON value.
    TaggedCbor(serde_json::Value),
}

impl From<ArgumentsProjection> for StagedArguments {
    fn from(value: ArgumentsProjection) -> Self {
        match value {
            ArgumentsProjection::Ordinary(value) => Self::Ordinary(value),
            ArgumentsProjection::TaggedCbor(value) => Self::TaggedCbor(value),
        }
    }
}

impl From<StagedArguments> for ArgumentsProjection {
    fn from(value: StagedArguments) -> Self {
        match value {
            StagedArguments::Ordinary(value) => Self::Ordinary(value),
            StagedArguments::TaggedCbor(value) => Self::TaggedCbor(value),
        }
    }
}

impl ArgumentsProjection {
    /// Projects CBOR as concise JSON or complete tagged-CBOR.
    fn from_cbor(value: &CborValue) -> Self {
        match faithful_json(value) {
            Some(value) => Self::Ordinary(value),
            None => Self::TaggedCbor(crate::lossless_json::typed_cbor(value)),
        }
    }

    /// Returns the projected JSON value.
    fn value(&self) -> &serde_json::Value {
        match self {
            Self::Ordinary(value) | Self::TaggedCbor(value) => value,
        }
    }

    /// Returns whether the projection uses complete tagged-CBOR.
    fn is_tagged(&self) -> bool {
        matches!(self, Self::TaggedCbor(_))
    }
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

/// Rendered output statistics and staged complete or bounded text.
#[derive(Clone, Copy)]
struct TerminalOutput {
    /// Complete rendered projection's byte count.
    bytes: usize,
    /// Complete rendered projection's line count.
    lines: usize,
    /// Staged complete or bounded rendered text.
    output: Endpoint,
    /// Whether staged text contains the complete rendered projection.
    complete: bool,
}

/// Writes one selected compact JSON Lines projection.
pub(super) fn write_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: super::AgentTraceMode,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    let origin = trace_origin(snapshot)?;
    serde_json::to_writer(&mut *output, &Header::new(root_agent_id, snapshot, mode))
        .map_err(json_error)?;
    writeln!(output)?;

    let mut payloads = PayloadStore::new()?;
    let mut calls = collect_calls(snapshot, mode, &mut payloads)?;
    sort_calls(&mut calls);
    for call in &calls {
        let record = call.project(origin, &mut payloads)?;
        serde_json::to_writer(&mut *output, &record).map_err(json_error)?;
        writeln!(output)?;
    }
    Ok(())
}

/// Writes one strict TOON document with a counted calls array.
pub(super) fn write_toon(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: super::AgentTraceMode,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    toon::write(root_agent_id, snapshot, mode, output)
}

/// Sorts projected calls by relative journal wall-clock and deterministic ties.
fn sort_calls(calls: &mut [Call]) {
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
}

impl Call {
    /// Loads one call's staged payloads into its selected output record.
    fn project<'a>(
        &'a self,
        origin: UnixMicros,
        payloads: &mut PayloadStore,
    ) -> Result<CallRecord<'a>, InspectError> {
        let arguments =
            ArgumentsProjection::from(payloads.load::<StagedArguments>(self.arguments)?);
        let command = self
            .command
            .map(|endpoint| payloads.load(endpoint))
            .transpose()?;
        let (status, duration_us, output) = match &self.terminal {
            Some(terminal) => {
                let TerminalOutput {
                    bytes,
                    lines,
                    output,
                    complete,
                } = terminal.output;
                let output = OutputProjection::Present {
                    output_bytes: bytes,
                    output_lines: lines,
                    output: payloads.load(output)?,
                    output_complete: complete,
                };
                (
                    terminal.status,
                    terminal.at.get().checked_sub(self.started_at.get()),
                    output,
                )
            }
            None => (
                Status::Incomplete,
                None,
                OutputProjection::Absent {
                    output_complete: false,
                },
            ),
        };
        Ok(CallRecord {
            record_type: "call",
            at_us: self.started_at.get().saturating_sub(origin.get()),
            agent_id: &self.agent_id,
            call_id: &self.call_id,
            tool: &self.tool,
            command,
            arguments,
            status,
            duration_us,
            output,
        })
    }
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
    mode: super::AgentTraceMode,
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
                        arguments: payloads.append(&StagedArguments::from(
                            ArgumentsProjection::from_cbor(&call.arguments),
                        ))?,
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
    mode: super::AgentTraceMode,
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
    let bytes = rendered.len();
    let lines = rendered.lines().count();
    let (projected, complete) = match mode {
        super::AgentTraceMode::Lite => lite_output(&rendered),
        super::AgentTraceMode::Full => (rendered.as_str(), true),
    };
    let output = TerminalOutput {
        bytes,
        lines,
        output: payloads.append(&projected)?,
        complete,
    };
    calls[index].terminal = Some(Terminal { at, status, output });
    Ok(())
}

/// Returns the first 4 KiB of rendered output without splitting UTF-8.
fn lite_output(output: &str) -> (&str, bool) {
    if output.len() <= LITE_OUTPUT_BYTES {
        return (output, true);
    }
    let mut end = LITE_OUTPUT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    (&output[..end], false)
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
        CborValue::Float(_) => None,
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
        CborValue::Bytes(_) | CborValue::Tag(_, _) | _ => None,
    }
}

/// Wraps JSON serialization failures as trace projection errors.
fn json_error(error: serde_json::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize compact agent tool trace: {error}"
    )))
}
