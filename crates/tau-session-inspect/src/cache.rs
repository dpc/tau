//! Offline cache evidence: canonical facts and explicitly partial legacy files.
//!
//! This first delivery deliberately does not reconstruct chains, infer
//! attempts, expose provider IDs, or turn normalized accounting into raw
//! provider evidence.

mod failure_shape;
mod inventory;
mod options;
mod strict_json;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use ciborium::de::Error as CborDecodeError;
use inventory::Inventory;
pub use options::{CacheOptions, CacheScanLimits, CacheScope};
use serde_json::{Value, json};
use tau_core::{AgentJournalSnapshot, SessionStore};
use tau_proto::{AgentId, Event};

use crate::{DescendantSelection, InspectError};

/// An in-memory, content-free report; preparation never writes inspection
/// state.
pub struct CacheReport {
    /// Serialized JSON Lines, bounded before admission.
    lines: Vec<String>,
    /// Whether useful output lacks required evidence.
    partial: bool,
    /// Number of accepted canonical response occurrences selected.
    responses: u64,
    /// Fixed gap codes and counts.
    gaps: BTreeMap<&'static str, u64>,
    /// Whether strict canonical snapshot validation was admitted.
    inspected: bool,
}

impl CacheReport {
    /// Returns whether the CLI must signal useful but partial analysis.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    /// Writes internal version-zero JSON Lines without bodies or provider IDs.
    pub fn write_jsonl(&self, output: &mut (impl io::Write + ?Sized)) -> io::Result<()> {
        for line in &self.lines {
            writeln!(output, "{line}")?;
        }
        Ok(())
    }

    /// Writes a compact summary safe from arbitrary provider display strings.
    pub fn write_summary(&self, output: &mut (impl io::Write + ?Sized)) -> io::Result<()> {
        if self.inspected {
            writeln!(output, "Canonical responses included: {}", self.responses)?;
        } else {
            writeln!(output, "Canonical snapshot: not inspected")?;
        }
        writeln!(
            output,
            "Evidence: {}",
            if self.partial {
                "partial"
            } else {
                "complete for requested scope"
            }
        )?;
        writeln!(
            output,
            "Eligibility and residency: unknown unless independently reported."
        )?;
        writeln!(
            output,
            "Legacy capture files are not dispatch counts or exact terminal joins."
        )?;
        for (reason, count) in &self.gaps {
            writeln!(output, "  {reason}: {count}")?;
        }
        writeln!(
            output,
            "Content-free, not public-safe; keep this workload report private."
        )
    }
}

/// Selects canonical journals and inventories existing captures without
/// provider contact.
pub fn read_cache_report(options: &CacheOptions) -> Result<CacheReport, InspectError> {
    match prepare_cache_report(options) {
        Err(InspectError::Io(error)) if error.kind() != io::ErrorKind::InvalidData => {
            unavailable_report(options, "canonical_source_unavailable")
        }
        Err(InspectError::AgentStore(error)) => match error {
            // Ciborium's semantic decode failure includes unsupported enum and
            // field shapes. Do not turn revision skew into empty partial success.
            tau_core::AgentStoreError::Decode {
                source: CborDecodeError::Semantic(..),
                ..
            } => Err(InspectError::AgentStore(error)),
            _ => unavailable_report(options, "canonical_source_corrupt_or_unavailable"),
        },
        Err(InspectError::SessionStore(error)) => match error {
            tau_core::SessionStoreError::Decode {
                source: CborDecodeError::Semantic(..),
                ..
            } => Err(InspectError::SessionStore(error)),
            _ => unavailable_report(options, "session_membership_corrupt_or_unavailable"),
        },
        result => result,
    }
}

/// Performs strict preparation before the public error-to-coverage projection.
fn prepare_cache_report(options: &CacheOptions) -> Result<CacheReport, InspectError> {
    if options.producer_build.len() > 128
        || options.limits.working_memory_bytes < 65536
        || options.limits.compressed_file_bytes == 0
        || options.limits.decompressed_file_bytes == 0
        || options.limits.total_decompressed_bytes == 0
    {
        return Err(invalid("invalid cache inspection build identity or limits"));
    }
    let agents_dir = options.state_dir.join("agents");
    let membership_charge = match &options.scope {
        CacheScope::Session(session) => {
            let length = std::fs::metadata(
                options
                    .state_dir
                    .join("sessions")
                    .join(session.as_str())
                    .join("events.cbor"),
            )?
            .len();
            journal_memory_charge(0, length)
        }
        CacheScope::Agent { .. } => 0,
    };
    if membership_charge > options.limits.working_memory_bytes / 4 {
        return unavailable_report(options, "journal_memory_preflight_limit");
    }
    let agents = select_agents(options)?;
    // Stage-1 admission is a conservative byte-charge proxy, not measured RSS.
    // Preserve the existing strict replay rather than adding a weaker reader.
    let mut journal_charge = membership_charge;
    for agent in &agents {
        let length = std::fs::metadata(agents_dir.join(agent.as_str()).join("events.cbor"))?.len();
        journal_charge = journal_memory_charge(journal_charge, length);
    }
    if journal_charge > options.limits.working_memory_bytes / 4 {
        return unavailable_report(options, "journal_memory_preflight_limit");
    }
    let snapshot = AgentJournalSnapshot::capture(&agents_dir, agents.iter().cloned())?;
    if agents != select_agents(options)? {
        return Err(invalid("cache scope changed during snapshot capture"));
    }
    let mut sessions = BTreeSet::new();
    let mut boundaries = Vec::new();
    for agent in snapshot.agent_ids() {
        let mut last_seq = None;
        for record in snapshot.records(agent)? {
            let record = record?;
            last_seq = Some(record.seq);
            if let Event::AgentPromptStarted(started) = record.event {
                if matches!(&options.scope, CacheScope::Session(id) if id != &started.session_id) {
                    continue;
                }
                sessions.insert(started.session_id);
            }
        }
        boundaries.push(json!({"agent_id": agent, "last_journal_seq": last_seq}));
    }
    if let CacheScope::Session(session) = &options.scope {
        sessions.insert(session.clone());
    }
    let mut inventory = Inventory::default();
    for session in sessions {
        inventory.scan(
            &options
                .state_dir
                .join("sessions")
                .join(session.as_str())
                .join("debug/provider-requests"),
            &session,
            &options.limits,
        );
    }
    if inventory.gaps.contains_key("unsupported_capture_schema") {
        return Err(invalid(
            "unsupported cache capture schema; use a matching-build inspector",
        ));
    }
    let mut report = CacheReport {
        lines: Vec::new(),
        partial: false,
        responses: 0,
        gaps: inventory.gaps.clone(),
        inspected: true,
    };
    let mut remaining = options.limits.working_memory_bytes / 4;
    append(
        &mut report,
        options,
        &mut remaining,
        "header",
        json!({
            "canonical": {"prefix_boundaries": boundaries},
            "reported": null,
            "derived": {
                "scope": match &options.scope {
                    CacheScope::Agent { agent_id, include_descendants } =>
                        json!({"agent_id": agent_id, "include_descendants": include_descendants}),
                    CacheScope::Session(session) => json!({"session_id": session}),
                },
                "prompt": options.prompt,
                "limits": options.limits,
                "content_policy": "content_free_not_public_safe",
                "capture_policy": "capture_inventory_no_terminal_or_dispatch_join",
                "cache_diagnostic_support": {
                    "codex_inference": "metadata",
                    "other_adapters": "unavailable",
                    "standalone_compaction": "unavailable",
                    "cache_refresh": "unavailable",
                    "raw_attribution": "unavailable"
                },
                "snapshot_policy": "strict_finite_agent_prefix_membership_rechecked"
                ,"memory_policy": "conservative_journal_byte_charge_not_measured_peak_memory"
            }
        }),
    )?;
    for agent in snapshot.agent_ids() {
        project_agent(
            &snapshot,
            agent,
            options,
            &inventory,
            &mut report,
            &mut remaining,
        )?;
    }
    if options.prompt.is_some() && report.responses == 0 {
        *report
            .gaps
            .entry("selected_prompt_without_canonical_response")
            .or_default() += 1;
    }
    report.partial = !report.gaps.is_empty();
    for (reason, count) in report.gaps.clone() {
        append(
            &mut report,
            options,
            &mut remaining,
            "gap",
            json!({
                "canonical": null, "reported": null,
                "derived": {
                    "method": "encountered_evidence_gap_v0",
                    "input_records": [],
                    "coverage": "partial",
                    "reason": reason,
                    "count": count
                }
            }),
        )?;
    }
    let summary = json!({
        "canonical": {"response_count": report.responses},
        "reported": null,
        "derived": {
            "method": "canonical_occurrence_count_v0",
            "input_records": "canonical_response_rows",
            "coverage": if report.partial { "partial" } else { "selected_canonical_prefix" },
            "residency_miss": "unknown",
            "failed_attempt_billing": "unknown",
            "cost_policy": "recorded_per_response_rates_and_increment_only"
        }
    });
    append(&mut report, options, &mut remaining, "summary", summary)?;
    Ok(report)
}

/// Returns useful explicit unavailability without claiming journal validation.
fn unavailable_report(
    options: &CacheOptions,
    reason: &'static str,
) -> Result<CacheReport, InspectError> {
    let mut report = CacheReport {
        lines: Vec::new(),
        partial: true,
        responses: 0,
        gaps: BTreeMap::from([(reason, 1)]),
        inspected: false,
    };
    let mut remaining = options.limits.working_memory_bytes;
    append(
        &mut report,
        options,
        &mut remaining,
        "header",
        json!({
            "canonical": null, "reported": null,
            "derived": {"coverage": "unavailable", "snapshot_inspected": false,
                "limits": options.limits, "content_policy": "content_free_not_public_safe"}
        }),
    )?;
    append(
        &mut report,
        options,
        &mut remaining,
        "gap",
        json!({
            "canonical": null, "reported": null,
            "derived": {"method": "journal_byte_charge_preflight_v0", "input_records": [],
                "coverage": "unavailable", "reason": reason, "count": 1}
        }),
    )?;
    append(
        &mut report,
        options,
        &mut remaining,
        "summary",
        json!({
            "canonical": null, "reported": null,
            "derived": {"coverage": "unavailable", "snapshot_inspected": false,
                "residency_miss": "unknown"}
        }),
    )?;
    Ok(report)
}

/// Selects authenticated agent edges or exact durable session membership.
fn select_agents(options: &CacheOptions) -> Result<BTreeSet<AgentId>, InspectError> {
    match &options.scope {
        CacheScope::Agent {
            agent_id,
            include_descendants,
        } => crate::agent_trace::discover_agents(
            &options.state_dir.join("agents"),
            agent_id,
            if *include_descendants {
                DescendantSelection::Include
            } else {
                DescendantSelection::RootOnly
            },
        ),
        CacheScope::Session(session) => {
            let root = options.state_dir.join("sessions");
            if !root.join(session.as_str()).join("events.cbor").is_file() {
                return Err(invalid("selected session journal unavailable"));
            }
            let store = SessionStore::open_lazy(&root)?;
            let mut agents = BTreeSet::new();
            for record in store.session_events(session.as_str())? {
                match record.event {
                    Event::SessionAgentLoaded(value) if value.session_id == *session => {
                        agents.insert(value.agent_id);
                    }
                    Event::SessionAgentUnloaded(value) if value.session_id == *session => {
                        agents.insert(value.agent_id);
                    }
                    _ => {}
                }
            }
            Ok(agents)
        }
    }
}

/// Emits only canonical terminals, never incoming provider reports.
fn project_agent(
    snapshot: &AgentJournalSnapshot,
    agent: &AgentId,
    options: &CacheOptions,
    inventory: &Inventory,
    report: &mut CacheReport,
    remaining: &mut u64,
) -> Result<(), InspectError> {
    let mut prompts = BTreeMap::new();
    for record in snapshot.records(agent)? {
        let record = record?;
        match record.event {
            Event::AgentPromptStarted(started) => {
                prompts.insert(started.agent_prompt_id.clone(), started);
            }
            Event::ProviderResponseFinished(response) => {
                if options
                    .prompt
                    .as_ref()
                    .is_some_and(|id| id != &response.agent_prompt_id)
                {
                    continue;
                }
                let Some(prompt) = prompts.get(&response.agent_prompt_id) else {
                    *report
                        .gaps
                        .entry("canonical_prompt_attribution_unavailable")
                        .or_default() += 1;
                    continue;
                };
                if matches!(&options.scope, CacheScope::Session(id) if id != &prompt.session_id) {
                    continue;
                }
                let counts = inventory
                    .prompts
                    .get(&(prompt.session_id.clone(), response.agent_prompt_id.clone()));
                let reason = match counts {
                    None => "capture_missing",
                    Some(counts) if counts.request_files > 1 || counts.response_files > 1 => {
                        "capture_terminal_join_ambiguous"
                    }
                    Some(_) => "legacy_terminal_join_unavailable",
                };
                *report.gaps.entry(reason).or_default() += 1;
                let reference = json!({"agent_id": agent, "journal_seq": record.seq});
                let usage = response.usage.as_ref();
                let metrics = usage.map(|usage| {
                    metrics(
                        usage.prompt_sent_tokens,
                        usage.prompt_cached_tokens,
                        usage.prompt_cache_read_ceiling_tokens,
                    )
                });
                let row = json!({
                    "agent_id": agent,
                    "agent_prompt_id": response.agent_prompt_id,
                    "canonical": {
                        "record": reference,
                        "recorded_at_unix_micros": record.recorded_at,
                        "operation": prompt.operation,
                        "model": prompt.model,
                        "provider_attempt": response.provider_attempt,
                        "backend": response.backend.as_ref().map(|backend| json!({
                            "kind": backend.kind,
                            "transport": backend.transport,
                            "stale_chain_fallback": backend.stale_chain_fallback,
                        })),
                        "ws_pool_delta": response.ws_pool_delta,
                        "input_tokens": usage.map(|u| u.prompt_sent_tokens),
                        "read_tokens": usage.map(|u| u.prompt_cached_tokens),
                        "output_tokens": usage.map(|u| u.response_received_tokens),
                        "eligible_ceiling_tokens": usage.and_then(|u| u.prompt_cache_read_ceiling_tokens),
                        "cache": usage.and_then(|u| u.cache.as_ref()),
                        "cost_rates": response.estimated_api_cost_rates,
                        "cost_increment": response.estimated_api_cost_increment,
                        "cost_qualification": "recorded_api_equivalent_estimate_not_invoice"
                    },
                    "reported": null,
                    "derived": {
                        "method": "normalized_canonical_cache_metrics_v0",
                        "input_records": [reference],
                        "coverage": reason,
                        "metrics": metrics,
                        "capture_files_for_prompt": counts,
                        "capture_completeness": "unknown",
                        "control_equality": "unknown",
                        "visible_prefix_equality": "unknown",
                        "chain_continuity": "unknown",
                        "route_equality": "unknown",
                        "residency_miss": "unknown"
                    }
                });
                if (row.to_string().len() as u64).saturating_add(16384) > *remaining {
                    *report.gaps.entry("report_memory_limit").or_default() += 1;
                    return Ok(());
                }
                append(report, options, remaining, "canonical_response", row)?;
                report.responses += 1;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Computes arithmetic without repairing invalid counters or synthesizing a
/// ceiling.
fn metrics(input: u64, reads: u64, ceiling: Option<u64>) -> Value {
    let valid = reads <= input;
    let eligible_valid = ceiling.is_some_and(|value| reads <= value && value <= input);
    json!({
        "share_of_input": (input != 0 && valid).then(|| reads as f64 / input as f64),
        "non_read_input": input.checked_sub(reads),
        "input_read_evidence": if valid { "valid" } else { "invalid" },
        "eligibility_evidence": match ceiling {
            None => "unknown",
            Some(_) if eligible_valid => "valid",
            Some(_) => "invalid",
        },
        "eligibility_utilization": ceiling.filter(|value| eligible_valid && *value != 0)
            .map(|value| reads as f64 / value as f64),
    })
}

/// Bounds retained output before admitting each private JSON line.
fn append(
    report: &mut CacheReport,
    options: &CacheOptions,
    remaining: &mut u64,
    kind: &'static str,
    mut row: Value,
) -> Result<(), InspectError> {
    row["schema"] = "tau.cache_diagnostic".into();
    row["schema_version"] = 0.into();
    row["producer_build"] = options.producer_build.clone().into();
    row["record_kind"] = kind.into();
    let line =
        serde_json::to_string(&row).map_err(|_| invalid("cache report serialization failed"))?;
    let bytes = (line.len() as u64).saturating_add(128);
    if bytes > *remaining {
        return Err(invalid(
            "cache report memory limit exceeded; narrow the selected scope",
        ));
    }
    *remaining -= bytes;
    report.lines.push(line);
    Ok(())
}

/// Returns a content-free failure rather than exposing source errors or paths.
fn invalid(message: &'static str) -> InspectError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

/// Saturates overflow so enormous or sparse journals cannot bypass admission.
fn journal_memory_charge(current: u64, length: u64) -> u64 {
    current.saturating_add(length.saturating_mul(128))
}
