//! Content-free provider accounting and recorded-at wall timing projection.

use std::collections::btree_map as path_std_collections_btree_map;

mod agent_summary;
mod orchestration;
mod provider_prompt;
mod provider_prompt_record;
mod summary;
mod usage;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use provider_prompt::ProviderPrompt;
use serde::Serialize;
use summary::Summary;
use tau_core::AgentJournalSnapshot;
use tau_proto::{AgentId, AgentPromptId, Event, UnixMicros};

use crate::InspectError;

const SCHEMA: &str = "tau.agent_performance";

/// First-line metadata describing the projection's deliberately limited timing.
#[derive(Serialize)]
struct Header<'a> {
    /// Stable schema identifier.
    schema: &'static str,
    /// Initial schema revision.
    schema_version: u32,
    /// Row discriminator.
    record_type: &'static str,
    /// Requested workflow root.
    root_agent_id: &'a AgentId,
    /// Deterministically selected journal identities.
    included_agent_ids: Vec<&'a AgentId>,
    /// Unit used by relative timing fields.
    time_unit: &'static str,
    /// Clock and boundary represented by elapsed intervals.
    timing_fidelity: &'static str,
    /// Confirms that provider and tool payload bodies are absent.
    content_included: bool,
}

/// Writes content-free provider-prompt occurrences and per-agent summaries.
pub(super) fn write_jsonl(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    let origin = trace_origin(snapshot)?;
    write_row(
        output,
        &Header {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id,
            included_agent_ids: snapshot.agent_ids().collect(),
            time_unit: "microseconds",
            timing_fidelity: "recorded_at_wall_clock_append_invocation_interval",
            content_included: false,
        },
    )?;

    for agent_id in snapshot.agent_ids() {
        let provider_prompts = collect_agent(snapshot, agent_id)?;
        let mut summary = Summary::default();
        let mut rows = Vec::new();
        for (prompt_id, prompt) in &provider_prompts {
            let value =
                serde_json::to_value(prompt.project(agent_id, prompt_id, origin, &mut summary)?)
                    .map_err(|error| {
                        projection_error(format!("failed to serialize performance trace: {error}"))
                    })?;
            rows.push(orchestration::OrderedRow {
                journal_seq: prompt.journal_seq(),
                family: 0,
                key: prompt_id.to_string(),
                value,
            });
        }
        rows.extend(orchestration::collect(snapshot, agent_id, origin)?);
        rows.sort_by(|left, right| {
            left.journal_seq
                .cmp(&right.journal_seq)
                .then_with(|| left.family.cmp(&right.family))
                .then_with(|| left.key.cmp(&right.key))
        });
        for row in rows {
            write_row(output, &row.value)?;
        }
        write_row(output, &summary.project(agent_id))?;
    }
    Ok(())
}

/// Collects lifecycle evidence while rejecting ambiguous duplicate facts.
fn collect_agent(
    snapshot: &AgentJournalSnapshot,
    agent_id: &AgentId,
) -> Result<BTreeMap<AgentPromptId, ProviderPrompt>, InspectError> {
    let mut prompts = BTreeMap::<AgentPromptId, ProviderPrompt>::new();
    let mut excluded = BTreeSet::<AgentPromptId>::new();
    let mut previous_recorded_at = None;
    let mut clock_regressions = 0_u64;
    for record in snapshot.records(agent_id)? {
        let record = record?;
        observe_clock(
            &mut previous_recorded_at,
            &mut clock_regressions,
            record.recorded_at,
        );
        let (prompt_id, accepted, label) = match record.event {
            Event::AgentPromptStarted(value) => {
                let prompt_id = value.agent_prompt_id;
                if !value.operation.is_inference() {
                    excluded.insert(prompt_id.clone());
                    prompts.remove(&prompt_id);
                    continue;
                }
                let accepted = match prompts.entry(prompt_id.clone()) {
                    path_std_collections_btree_map::Entry::Vacant(entry) => {
                        entry.insert(ProviderPrompt::new(
                            record.seq,
                            record.recorded_at,
                            clock_regressions,
                            value.model,
                        ));
                        true
                    }
                    path_std_collections_btree_map::Entry::Occupied(_) => false,
                };
                (prompt_id, accepted, "agent.prompt_started")
            }
            Event::ProviderResponseFinished(value) => {
                let prompt_id = value.agent_prompt_id.clone();
                if excluded.contains(&prompt_id) {
                    continue;
                }
                let Some(prompt) = prompts.get_mut(&prompt_id) else {
                    continue;
                };
                let accepted =
                    prompt.set_terminal(record.seq, record.recorded_at, clock_regressions, value);
                (prompt_id, accepted, "provider.response_finished")
            }
            _ => continue,
        };
        if !accepted {
            return Err(ambiguous(agent_id, &prompt_id, label));
        }
    }
    Ok(prompts)
}

/// Finds the earliest available included journal timestamp.
fn trace_origin(snapshot: &AgentJournalSnapshot) -> Result<Option<UnixMicros>, InspectError> {
    let mut origin = None;
    for agent_id in snapshot.agent_ids() {
        for record in snapshot.records(agent_id)? {
            let recorded_at = record?.recorded_at;
            if recorded_at.get() != 0 {
                origin = Some(origin.map_or(recorded_at, |current: UnixMicros| {
                    std::cmp::min(current, recorded_at)
                }));
            }
        }
    }
    Ok(origin)
}

fn ambiguous(agent_id: &AgentId, prompt_id: &AgentPromptId, event: &str) -> InspectError {
    projection_error(format!(
        "agent `{agent_id}` prompt `{prompt_id}` has multiple `{event}` facts"
    ))
}

fn write_row(output: &mut impl std::io::Write, row: &impl Serialize) -> Result<(), InspectError> {
    serde_json::to_writer(&mut *output, row).map_err(|error| {
        projection_error(format!("failed to serialize performance trace: {error}"))
    })?;
    writeln!(output)?;
    Ok(())
}

fn projection_error(message: String) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(message))
}

/// Tracks comparable nonzero wall samples without letting unavailable samples
/// hide a later regression.
fn observe_clock(previous: &mut Option<UnixMicros>, regressions: &mut u64, current: UnixMicros) {
    if current.get() == 0 {
        return;
    }
    if previous.is_some_and(|previous| current < previous) {
        *regressions = regressions.saturating_add(1);
    }
    *previous = Some(current);
}
