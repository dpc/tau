//! Canonical lossless Tau JSON Lines projection.

use serde::Serialize;
use serde_json::Value;
use tau_core::{AgentJournalSnapshot, PersistedAgentEvent};
use tau_proto::AgentId;

use crate::InspectError;
use crate::lossless_json::event_json;

const SCHEMA: &str = "tau.agent_trace";

/// Serialized first-line metadata for one native trace artifact.
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
    /// Deterministically grouped journal identities.
    included_agent_ids: Vec<&'a AgentId>,
    /// Meaning of exported timestamps.
    timing: &'static str,
}

/// Lossless JSON representation of one durable journal occurrence.
#[derive(Serialize)]
struct Occurrence<'a> {
    /// Stable schema identifier.
    schema: &'static str,
    /// Initial internal schema revision.
    schema_version: u32,
    /// Discriminator for journal lines.
    record_type: &'static str,
    /// Journal owning this occurrence.
    agent_id: &'a AgentId,
    /// Journal-local authoritative order.
    seq: tau_core::PersistedAgentEventSeq,
    /// Durable wall-clock append timestamp.
    recorded_at_unix_micros: tau_proto::UnixMicros,
    /// Publishing connection, when known.
    source: &'a Option<tau_proto::ConnectionId>,
    /// Explicit transcript fold parent.
    parent: &'a tau_core::AgentEventParent,
    /// Complete typed durable event in lossless tagged-CBOR JSON form.
    event: Value,
}

/// Writes the complete canonical native artifact without retaining its
/// payloads.
pub(super) fn write_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    serde_json::to_writer(
        &mut *output,
        &Header {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id,
            included_agent_ids: snapshot.agent_ids().collect(),
            timing: "journal_wall_clock",
        },
    )
    .map_err(json_error)?;
    writeln!(output)?;
    for agent_id in snapshot.agent_ids() {
        for record in snapshot.records(agent_id)? {
            let record = record?;
            writeln!(output, "{}", occurrence_json(agent_id, &record)?)?;
        }
    }
    Ok(())
}

/// Serializes one independently parseable native occurrence.
pub(super) fn occurrence_json(
    agent_id: &AgentId,
    record: &PersistedAgentEvent,
) -> Result<String, InspectError> {
    serde_json::to_string(&Occurrence {
        schema: SCHEMA,
        schema_version: 0,
        record_type: "event",
        agent_id,
        seq: record.seq,
        recorded_at_unix_micros: record.recorded_at,
        source: &record.source,
        parent: &record.parent,
        event: event_json(&record.event)?,
    })
    .map_err(json_error)
}

fn json_error(error: serde_json::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize native trace: {error}"
    )))
}
