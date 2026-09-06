//! Bounded legacy capture inventory, deliberately not an attempt ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::path::Path;

use serde_json::Value;
use tau_proto::{AgentId, AgentPromptId, SessionId};
use zstd::stream::read::Decoder;

use super::exact_geometry::{self, ExactRequest, ExactResponse, FingerprintKey};
use super::{CacheGroup, CacheOptions, CacheReport, CacheScanLimits, CacheView};
use crate::InspectError;

mod exact;

/// Stable identity of one scalar record inside one provider process run.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct RecordId {
    /// Provider instance directory, retained only for private in-memory joins.
    instance: String,
    /// Process-local random producer identity.
    run: String,
    /// Process-local sequence allocated before capture admission.
    sequence: u64,
}

/// Validated scalar diagnostic retained without provider bodies or raw IDs.
#[derive(Clone)]
struct DiagnosticRecord {
    /// Exact record identity.
    id: RecordId,
    /// Original closed-schema value after strict JSON parsing.
    value: Value,
    /// Typed prompt attribution when this is prompt-scoped.
    prompt: Option<AgentPromptId>,
    /// Typed provider-owner attribution used for selected-scope isolation.
    agent: AgentId,
    /// Random attempt identity shared by related records.
    attempt: String,
}

/// Explicit scalar dispatch attribution used for exact-capture joins.
type DispatchJoin<'a> = &'a DiagnosticRecord;

/// Counts of independently observed files, never inferred dispatch counts.
#[derive(Default, serde::Serialize)]
pub(super) struct CaptureCounts {
    /// Files containing a supported request envelope.
    pub request_files: u64,
    /// Files containing a supported successful-response envelope.
    pub response_files: u64,
    /// Files containing a supported failure envelope.
    pub failure_files: u64,
    /// Recognized scalar files, not reconstructed attempts or canonical joins.
    pub diagnostic_files: u64,
}

/// Content-free evidence collected without retaining provider payloads or IDs.
pub(super) struct Inventory {
    /// Exact typed session/prompt attribution; no terminal association implied.
    pub prompts: BTreeMap<(SessionId, AgentPromptId), CaptureCounts>,
    /// Fixed reason codes and encountered occurrence counts.
    pub gaps: BTreeMap<&'static str, u64>,
    /// Validated and deduplicated current scalar records.
    diagnostics: Vec<DiagnosticRecord>,
    /// First value observed for each stable record identity.
    record_values: BTreeMap<RecordId, usize>,
    /// Identities whose payloads conflict and cannot support derived evidence.
    conflicted_records: BTreeSet<RecordId>,
    /// Conservative retained scalar allocation charge.
    retained_diagnostic_bytes: u64,
    /// Inspection-local secret for exact equality evidence.
    exact_key: FingerprintKey,
    /// Fixed-size request evidence derived before private bodies are discarded.
    exact_requests: Vec<ExactRequest>,
    /// Fixed-size successful-response IDs for explicit chain checks.
    exact_responses: Vec<ExactResponse>,
    /// Conservative allocation charge for exact evidence.
    retained_exact_bytes: u64,
    /// Total decoded bytes consumed, including rejected files.
    decoded: u64,
}

impl Default for Inventory {
    fn default() -> Self {
        Self::new(FingerprintKey::random().expect("OS randomness is required for private hashes"))
    }
}

impl Inventory {
    /// Constructs an empty inventory using one invocation/index-local key.
    pub(super) fn new(exact_key: FingerprintKey) -> Self {
        Self {
            prompts: BTreeMap::new(),
            gaps: BTreeMap::new(),
            diagnostics: Vec::new(),
            record_values: BTreeMap::new(),
            conflicted_records: BTreeSet::new(),
            retained_diagnostic_bytes: 0,
            exact_key,
            exact_requests: Vec::new(),
            exact_responses: Vec::new(),
            retained_exact_bytes: 0,
            decoded: 0,
        }
    }

    /// Returns retained scalar rows for focused in-crate identity tests.
    #[cfg(test)]
    pub(super) fn diagnostic_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Returns stable record sequences for focused duplicate diagnostics.
    #[cfg(test)]
    pub(super) fn diagnostic_sequences(&self) -> Vec<u64> {
        self.diagnostics
            .iter()
            .map(|record| record.id.sequence)
            .collect()
    }

    /// Counts a gap without retaining error prose or a source pathname.
    pub fn gap(&mut self, reason: &'static str) {
        *self.gaps.entry(reason).or_default() += 1;
    }

    /// Scans only the selected session's existing instance directories.
    pub fn scan(&mut self, root: &Path, session: &SessionId, limits: &CacheScanLimits) {
        if root.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
            self.gap("capture_directory_not_regular");
            return;
        }
        let Ok(instances) = std::fs::read_dir(root) else {
            self.gap("capture_directory_unavailable");
            return;
        };
        for instance in instances {
            let Ok(instance) = instance else {
                self.gap("capture_directory_unreadable");
                continue;
            };
            if !instance.file_type().is_ok_and(|kind| kind.is_dir()) {
                self.gap("capture_instance_not_directory");
                continue;
            }
            let Ok(files) = std::fs::read_dir(instance.path()) else {
                self.gap("capture_directory_unreadable");
                continue;
            };
            for file in files {
                let Ok(file) = file else {
                    self.gap("capture_directory_unreadable");
                    continue;
                };
                if !file.file_name().as_encoded_bytes().ends_with(b".json.zst") {
                    continue;
                }
                if !file.file_type().is_ok_and(|kind| kind.is_file()) {
                    self.gap("capture_not_regular");
                    continue;
                }
                if self.decoded >= limits.total_decompressed_bytes {
                    self.gap("cumulative_capture_limit");
                    return;
                }
                let instance_name = instance.file_name().to_string_lossy().into_owned();
                match self.read(&file.path(), limits) {
                    Ok(value) => self.observe(session, &instance_name, value, limits),
                    Err(reason) => self.gap(reason),
                }
            }
        }
    }

    /// Opens one non-symlink file and bounds both decode and JSON allocation.
    fn read(&mut self, path: &Path, limits: &CacheScanLimits) -> Result<Value, &'static str> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file: File = options.open(path).map_err(|_| "capture_unreadable")?;
        let metadata = file.metadata().map_err(|_| "capture_unreadable")?;
        if !metadata.is_file() {
            return Err("capture_not_regular");
        }
        if metadata.len() > limits.compressed_file_bytes {
            return Err("compressed_capture_limit");
        }
        // A deliberately conservative allowance covers serde's tree, keys,
        // scalar allocations and the decoded buffer, including tiny JSON nodes.
        let retained = (self.prompts.len() as u64).saturating_mul(1024);
        let parse_budget = (limits.working_memory_bytes / 8 * 3).saturating_sub(retained) / 128;
        let cap = limits
            .decompressed_file_bytes
            .min(limits.total_decompressed_bytes.saturating_sub(self.decoded))
            .min(parse_budget);
        let mut decoder = Decoder::new(file.take(limits.compressed_file_bytes.saturating_add(1)))
            .map_err(|_| "malformed_compression")?;
        decoder
            .window_log_max((limits.working_memory_bytes / 8).max(1024).ilog2().min(23))
            .map_err(|_| "compression_window_limit")?;
        let mut bytes = Vec::new();
        let result = decoder.take(cap.saturating_add(1)).read_to_end(&mut bytes);
        self.decoded = self.decoded.saturating_add(bytes.len() as u64);
        result.map_err(|_| "truncated_or_malformed_compression")?;
        if bytes.len() as u64 > cap {
            return Err("decoded_or_memory_capture_limit");
        }
        serde_json::from_slice::<super::strict_json::StrictJson>(&bytes)
            .map(|value| value.0)
            .map_err(|_| "malformed_or_ambiguous_capture_json")
    }

    /// Projects only recognized envelopes; content and arbitrary metadata die
    /// here.
    fn observe(
        &mut self,
        session: &SessionId,
        instance: &str,
        value: Value,
        limits: &CacheScanLimits,
    ) {
        let cache_diagnostic = value.get("schema").and_then(Value::as_str)
            == Some("tau.cache_diagnostic")
            && value.get("schema_version").and_then(Value::as_u64) == Some(0);
        let known_failure = value.get("schema").is_none()
            && matches!(
                (
                    value.get("schema_version").and_then(Value::as_u64),
                    value.get("capture_kind").and_then(Value::as_str)
                ),
                (Some(1), Some("provider_attempt_failure"))
                    | (Some(0), Some("compact_http_failure"))
            );
        if !known_failure
            && !cache_diagnostic
            && (value.get("schema").is_some() || value.get("schema_version").is_some())
        {
            self.gap("unsupported_capture_schema");
            return;
        }
        let Some(captured_session) = value.get("session_id").and_then(Value::as_str) else {
            self.gap("capture_attribution_unavailable");
            return;
        };
        if cache_diagnostic
            && value.get("operation").and_then(Value::as_str) == Some("cache_refresh")
        {
            if captured_session != session.as_str() {
                self.gap("capture_session_mismatch");
                return;
            }
            let valid = cache_diagnostic_header(&value)
                && value.get("agent_prompt_id").is_some_and(Value::is_null)
                && value.get("logical_attempt").is_some_and(Value::is_null)
                && value
                    .get("harness_provider_attempt")
                    .is_some_and(Value::is_null)
                && value
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| tau_proto::AgentId::parse(id).is_ok())
                && value
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty() && id.len() <= 128);
            if !valid {
                self.gap("malformed_current_cache_diagnostic");
                return;
            }
            match diagnostic_record(instance, &value, None) {
                Ok(record) => self.admit_diagnostic(record, value, limits),
                Err(reason) => self.gap(reason),
            }
            return;
        }
        let Some(prompt) = value.get("agent_prompt_id").and_then(Value::as_str) else {
            self.gap("capture_attribution_unavailable");
            return;
        };
        let Ok(prompt) = AgentPromptId::parse(prompt) else {
            self.gap("capture_attribution_malformed");
            return;
        };
        if captured_session != session.as_str() {
            self.gap("capture_session_mismatch");
            return;
        }
        if cache_diagnostic {
            match diagnostic_record(instance, &value, Some(prompt.clone())) {
                Ok(record) => self.admit_diagnostic(record, value.clone(), limits),
                Err(reason) => {
                    self.gap(reason);
                    return;
                }
            }
        }
        if known_failure {
            let valid = match value.get("capture_kind").and_then(Value::as_str) {
                Some("compact_http_failure") => super::failure_shape::compact(&value),
                Some("provider_attempt_failure") => super::failure_shape::attempt(&value),
                _ => false,
            };
            if !valid {
                self.gap("malformed_current_failure_capture");
                return;
            }
        }
        let chat = value.get("backend").and_then(Value::as_str) == Some("chat_completions");
        let chat_response = chat
            && value.get("usage").is_some()
            && value.get("stop_reason").is_some()
            && value.get("output_items").is_some_and(Value::is_array)
            && value.get("raw_events").is_some_and(Value::is_array);
        let chat_http_failure = chat
            && value
                .get("http_status")
                .and_then(Value::as_u64)
                .is_some_and(|status| u16::try_from(status).is_ok())
            && value.get("body").is_some_and(Value::is_string);
        let kind = if cache_diagnostic {
            3
        } else if known_failure || chat_http_failure {
            2
        } else if value.get("body").is_some_and(Value::is_object) {
            0
        } else if chat_response
            || (value.get("provider_response_id").is_some() && value.get("usage").is_some())
        {
            1
        } else if value.get("error").is_some() {
            2
        } else {
            self.gap("unsupported_capture_shape");
            return;
        };
        if kind == 0 {
            match exact_geometry::request(&self.exact_key, instance, &value) {
                Ok(request) => self.admit_exact_request(request, limits),
                Err(reason) => self.gap(reason),
            }
        } else if kind == 1
            && let Some(response) = exact_geometry::response(&self.exact_key, instance, &value)
        {
            self.admit_exact_response(response, limits);
        }
        if (self.prompts.len() as u64)
            .saturating_add(1)
            .saturating_mul(1024)
            > limits.working_memory_bytes / 2
        {
            self.gap("inventory_memory_limit");
            return;
        }
        let counts = self.prompts.entry((session.clone(), prompt)).or_default();
        match kind {
            0 => counts.request_files += 1,
            1 => counts.response_files += 1,
            2 => counts.failure_files += 1,
            _ => counts.diagnostic_files += 1,
        }
        if !cache_diagnostic {
            self.gap("legacy_partial");
        }
    }

    /// Deduplicates one validated record or exposes conflicting reuse.
    fn admit_diagnostic(
        &mut self,
        record: DiagnosticRecord,
        value: Value,
        limits: &CacheScanLimits,
    ) {
        if let Some(previous) = self.record_values.get(&record.id).copied() {
            if self.diagnostics[previous].value != value {
                self.gap("conflicting_cache_diagnostic_record");
                self.conflicted_records.insert(record.id);
            }
            return;
        }
        let charge = serde_json::to_vec(&value)
            .map(|bytes| {
                (bytes.len() as u64)
                    .saturating_mul(128)
                    .saturating_add(2048)
            })
            .unwrap_or(u64::MAX);
        if self.retained_diagnostic_bytes.saturating_add(charge) > limits.working_memory_bytes / 2 {
            self.gap("diagnostic_memory_limit");
            return;
        }
        self.retained_diagnostic_bytes = self.retained_diagnostic_bytes.saturating_add(charge);
        self.record_values
            .insert(record.id.clone(), self.diagnostics.len());
        self.diagnostics.push(record);
    }

    /// Projects current scalar records without changing canonical accounting.
    pub(super) fn project(
        &mut self,
        options: &CacheOptions,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
        remaining: &mut u64,
    ) -> Result<(), InspectError> {
        if options.view == CacheView::Geometry || options.index.is_some() {
            self.qualify_exact_requests(selected_agents, report);
        }
        match options.view {
            CacheView::Summary | CacheView::Gaps => Ok(()),
            CacheView::Attribution => {
                self.project_attribution(options, selected_agents, report, remaining)
            }
            CacheView::Continuity => {
                self.project_continuity(options, selected_agents, report, remaining)
            }
            CacheView::Geometry => {
                self.project_geometry(options, selected_agents, report, remaining)
            }
        }
    }

    /// Emits provider attribution exactly as retained, including explicit
    /// unsupported, malformed, and truncated states.
    fn project_attribution(
        &self,
        options: &CacheOptions,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
        remaining: &mut u64,
    ) -> Result<(), InspectError> {
        let selected = self.selected(options, selected_agents, report);
        let attempt_ends = selected
            .iter()
            .copied()
            .filter(|record| record.value["record_kind"] == "attempt_end")
            .collect::<Vec<_>>();
        if attempt_ends.is_empty() {
            *report
                .gaps
                .entry(if selected.is_empty() {
                    "selected_cache_diagnostic_missing"
                } else {
                    "attribution_attempt_end_missing"
                })
                .or_default() += 1;
        }
        for (ordinal, record) in attempt_ends.into_iter().enumerate() {
            let label = record_label(ordinal);
            append_scalar_record(report, options, remaining, record, &label)?;
            let (entries, sanitized_partial) = safe_attribution(&record.value);
            let coverage = attribution_coverage(&record.value, sanitized_partial);
            if coverage != "reported_complete" {
                *report
                    .gaps
                    .entry("attribution_evidence_unavailable_or_partial")
                    .or_default() += 1;
            }
            super::append(
                report,
                options,
                remaining,
                "attribution",
                serde_json::json!({
                    "agent_prompt_id": record.prompt,
                    "canonical": null,
                    "reported": {
                        "status": closed(&record.value, "attribution_status",
                            &["absent", "complete", "truncated", "malformed", "unsupported_shape"]),
                        "total_check": closed(&record.value, "attribution_total_check",
                            &["matches", "mismatch", "not_checkable"]),
                        "entries": entries,
                        "omitted_entries": unsigned(&record.value, "omitted_entries"),
                        "usage": safe_usage(&record.value),
                    },
                    "derived": {
                        "method": "reported_attribution_projection_v0",
                        "input_records": [label],
                        "coverage": coverage,
                        "nested_content_not_summed": true
                    }
                }),
            )?;
        }
        Ok(())
    }

    /// Emits exact capture-local attempt continuity without timestamp or
    /// adjacency inference.
    fn project_continuity(
        &self,
        options: &CacheOptions,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
        remaining: &mut u64,
    ) -> Result<(), InspectError> {
        let mut attempts: BTreeMap<(&str, &str, &str), Vec<&DiagnosticRecord>> = BTreeMap::new();
        for record in self.selected(options, selected_agents, report) {
            attempts
                .entry((&record.id.instance, &record.id.run, &record.attempt))
                .or_default()
                .push(record);
        }
        for (attempt_ordinal, records) in attempts.values().enumerate() {
            let labels = (0..records.len())
                .map(|record_ordinal| {
                    format!(
                        "attempt-{}-record-{}",
                        attempt_ordinal.saturating_add(1),
                        record_ordinal.saturating_add(1)
                    )
                })
                .collect::<Vec<_>>();
            for (record, label) in records.iter().zip(&labels) {
                append_scalar_record(report, options, remaining, record, label)?;
            }
            let dispatches: Vec<_> = records
                .iter()
                .copied()
                .filter(|record| record.value["record_kind"] == "dispatch")
                .collect();
            let ends: Vec<_> = records
                .iter()
                .copied()
                .filter(|record| record.value["record_kind"] == "attempt_end")
                .collect();
            let coverage = if records
                .iter()
                .any(|record| !scalar_projection_valid(&record.value))
            {
                "sanitized_partial"
            } else if ends.is_empty() {
                "partial_missing_attempt_end"
            } else if ends.len() > 1 {
                "ambiguous_multiple_attempt_end"
            } else {
                match dispatch_continuity(&dispatches, ends[0]) {
                    Ok(()) => "capture_local",
                    Err(reason) => reason,
                }
            };
            if coverage != "capture_local" {
                *report
                    .gaps
                    .entry("attempt_continuity_incomplete")
                    .or_default() += 1;
            }
            let end = ends.first();
            if options.selection.require_exact_chain {
                *report
                    .exclusions
                    .entry("comparison_without_exact_chain")
                    .or_default() += 1;
                *report
                    .gaps
                    .entry("required_exact_chain_evidence_unavailable")
                    .or_default() += 1;
                continue;
            }
            super::append(
                report,
                options,
                remaining,
                "comparison",
                serde_json::json!({
                    "agent_prompt_id": records.first().and_then(|record| record.prompt.as_ref()),
                    "canonical": null,
                    "reported": {
                        "operation": closed(&records[0].value, "operation",
                            &["inference", "standalone_compaction", "cache_refresh"]),
                        "logical_attempt": unsigned(&records[0].value, "logical_attempt"),
                        "harness_provider_attempt": unsigned(&records[0].value, "harness_provider_attempt"),
                        "dispatch_count_observed": dispatches.len(),
                        "attempt_end_dispatch_count": end.and_then(|record|
                            unsigned(&record.value, "dispatch_count")),
                        "outcome": end.map(|record| closed(&record.value, "outcome",
                            &["success", "error", "canceled", "pre_dispatch_failure"])),
                        "request_forms": dispatches.iter().map(|record| closed(&record.value,
                            "request_form", &["full", "anchored_suffix", "repair_full", "other"])).collect::<Vec<_>>(),
                        "anchor_validation": dispatches.iter().map(|record| closed(&record.value,
                            "anchor_validation", &["matched", "mismatched", "unavailable", "not_applicable"])).collect::<Vec<_>>(),
                        "connection_state": dispatches.iter().map(|record| closed(&record.value,
                            "connection_state", &["new", "reused", "replaced", "not_applicable", "unknown"])).collect::<Vec<_>>(),
                        "repair_used": end.and_then(|record| record.value.get("repair_used")).and_then(Value::as_bool),
                        "chain_strip_count": end.and_then(|record| record.value.get("chain_strip_count")).and_then(Value::as_u64),
                    },
                    "derived": {
                        "method": "capture_local_attempt_continuity_v0",
                        "input_records": labels,
                        "coverage": coverage,
                        "visible_prefix_equality": "unknown",
                        "route_equality": "unknown",
                        "provider_residency": "unknown"
                    }
                }),
            )?;
        }
        Ok(())
    }

    /// Emits empirical reported-read distributions grouped by closed scalar
    /// request controls; it does not claim provider cache block geometry.
    fn project_geometry(
        &self,
        options: &CacheOptions,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
        remaining: &mut u64,
    ) -> Result<(), InspectError> {
        let selected = self.selected(options, selected_agents, report);
        let mut labels = BTreeMap::new();
        for (ordinal, record) in selected.iter().copied().enumerate() {
            let label = record_label(ordinal);
            append_scalar_record(report, options, remaining, record, &label)?;
            labels.insert(record.id.clone(), label);
        }
        let mut dispatch_by_attempt: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for record in selected
            .iter()
            .copied()
            .filter(|record| record.value["record_kind"] == "dispatch")
        {
            dispatch_by_attempt
                .entry((&record.id.instance, &record.id.run, &record.attempt))
                .or_default()
                .push(record);
        }
        let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut references: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut displayed_regimes = BTreeMap::new();
        let mut model_labels = BTreeMap::new();
        for end in selected
            .iter()
            .copied()
            .filter(|record| record.value["record_kind"] == "attempt_end")
        {
            let Some(dispatches) =
                dispatch_by_attempt.get(&(&end.id.instance, &end.id.run, &end.attempt))
            else {
                *report.gaps.entry("geometry_dispatch_missing").or_default() += 1;
                continue;
            };
            if let Err(reason) = dispatch_continuity(dispatches, end) {
                *report.gaps.entry(reason).or_default() += 1;
                continue;
            }
            let keys = dispatches
                .iter()
                .filter(|dispatch| scalar_projection_valid(&dispatch.value))
                .map(|dispatch| geometry_regime_key(&dispatch.value, &options.selection.group_by))
                .collect::<BTreeSet<_>>();
            if keys.len() != 1
                || dispatches
                    .iter()
                    .any(|record| !scalar_projection_valid(&record.value))
            {
                *report
                    .gaps
                    .entry("geometry_attempt_regime_ambiguous")
                    .or_default() += 1;
                continue;
            }
            let dispatch = dispatches[0];
            let model = dispatch
                .value
                .get("effective_model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty() && model.len() <= 128);
            if options.selection.group_by.contains(&CacheGroup::Model) && model.is_none() {
                *report.gaps.entry("geometry_model_unavailable").or_default() += 1;
                *report
                    .exclusions
                    .entry("geometry_group_value_unavailable")
                    .or_default() += 1;
                continue;
            }
            if !scalar_projection_valid(&end.value) {
                continue;
            }
            let Some(read) = end
                .value
                .pointer("/reported_usage/read_tokens")
                .and_then(Value::as_u64)
            else {
                continue;
            };
            let key = keys.into_iter().next().expect("one regime");
            let model_label = model.map(|model| {
                let next_model = model_labels.len().saturating_add(1);
                model_labels
                    .entry(model.to_owned())
                    .or_insert_with(|| format!("model-{next_model}"))
                    .clone()
            });
            displayed_regimes.entry(key.clone()).or_insert_with(|| {
                geometry_display_regime(&dispatch.value, model_label, &options.selection.group_by)
            });
            groups.entry(key.clone()).or_default().push(read);
            references
                .entry(key)
                .or_default()
                .push(labels[&end.id].clone());
        }
        for (key, mut reads) in groups {
            if options.selection.require_exact_chain {
                *report
                    .exclusions
                    .entry("comparison_without_exact_chain")
                    .or_default() += 1;
                continue;
            }
            reads.sort_unstable();
            let empirical_gcd = reads.iter().copied().reduce(gcd);
            super::append(
                report,
                options,
                remaining,
                "comparison",
                serde_json::json!({
                    "canonical": null,
                    "reported": {"read_tokens": reads},
                    "derived": {
                        "method": "empirical_reported_read_distribution_v0",
                        "input_records": references.remove(&key).unwrap_or_default(),
                        "coverage": "scalar_regime_only",
                        "regime": displayed_regimes.remove(&key).expect("display regime"),
                        "observed_max_read_tokens": reads.last(),
                        "empirical_gcd_read_tokens": empirical_gcd,
                        "visible_prefix_equality": "unknown",
                        "exact_cache_geometry": "unknown"
                    }
                }),
            )?;
        }
        self.project_exact_geometry(options, selected_agents, report, remaining)?;
        if !selected.is_empty() && self.exact_requests.is_empty() {
            *report
                .gaps
                .entry("exact_request_geometry_unavailable")
                .or_default() += 1;
        }
        Ok(())
    }

    /// Iterates records selected by shared prompt, time, model, operation, and
    /// attempt filters.
    fn selected<'a>(
        &'a self,
        options: &'a CacheOptions,
        selected_agents: &'a BTreeSet<AgentId>,
        report: &mut CacheReport,
    ) -> Vec<&'a DiagnosticRecord> {
        let candidates = self
            .diagnostics
            .iter()
            .filter(move |record| {
                selected_agents.contains(&record.agent)
                    && !self.conflicted_records.contains(&record.id)
                    && options
                        .prompt
                        .as_ref()
                        .is_none_or(|prompt| record.prompt.as_ref() == Some(prompt))
            })
            .collect::<Vec<_>>();
        let mut selected = Vec::new();
        for record in candidates {
            match self.scalar_selected(record, options) {
                Ok(true) => selected.push(record),
                Ok(false) => {
                    *report
                        .exclusions
                        .entry("scalar_filter_mismatch")
                        .or_default() += 1;
                }
                Err(reason) => {
                    *report.exclusions.entry(reason).or_default() += 1;
                    *report
                        .gaps
                        .entry("selection_evidence_unavailable")
                        .or_default() += 1;
                }
            }
        }
        selected.sort_by(|left, right| left.id.cmp(&right.id));
        selected
    }

    /// Applies shared scalar filters using capture-local attempt peers rather
    /// than inventing missing values.
    fn scalar_selected(
        &self,
        record: &DiagnosticRecord,
        options: &CacheOptions,
    ) -> Result<bool, &'static str> {
        let selection = &options.selection;
        if selection.since_unix_micros.is_none()
            && selection.until_unix_micros.is_none()
            && selection.model.is_none()
            && selection.operation.is_none()
            && selection.attempt.is_none()
        {
            return Ok(true);
        }
        if selection.since_unix_micros.is_some() || selection.until_unix_micros.is_some() {
            let recorded = record
                .value
                .get("recorded_at_unix_micros")
                .and_then(Value::as_u64)
                .ok_or("scalar_time_unavailable")?;
            if selection
                .since_unix_micros
                .is_some_and(|since| recorded < since)
                || selection
                    .until_unix_micros
                    .is_some_and(|until| until < recorded)
            {
                return Ok(false);
            }
        }
        if let Some(operation) = selection.operation {
            let observed = record
                .value
                .get("operation")
                .and_then(Value::as_str)
                .filter(|value| {
                    matches!(
                        *value,
                        "inference" | "standalone_compaction" | "cache_refresh"
                    )
                })
                .ok_or("scalar_operation_unavailable")?;
            if observed != operation.as_str() {
                return Ok(false);
            }
        }
        if let Some(attempt) = selection.attempt {
            let observed = observed_attempt(&record.value)?;
            if observed != attempt {
                return Ok(false);
            }
        }
        if let Some(model) = selection.model.as_deref() {
            let mut observed = BTreeSet::new();
            for peer in self.diagnostics.iter().filter(|peer| {
                peer.id.instance == record.id.instance
                    && peer.id.run == record.id.run
                    && peer.attempt == record.attempt
                    && peer.value["record_kind"] == "dispatch"
            }) {
                if self.conflicted_records.contains(&peer.id) {
                    return Err("scalar_model_conflicted");
                }
                let value = peer
                    .value
                    .get("effective_model")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .ok_or("scalar_model_unavailable")?;
                observed.insert(value);
            }
            let observed = match observed.len() {
                0 => return Err("scalar_model_unavailable"),
                1 => *observed.iter().next().expect("one observed model"),
                _ => return Err("scalar_model_ambiguous"),
            };
            if observed != model {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Reads the logical ordinal, falling back to the harness attempt only when the
/// logical field is explicitly null.
fn observed_attempt(value: &Value) -> Result<u64, &'static str> {
    match value.get("logical_attempt") {
        Some(Value::Number(number)) => number
            .as_u64()
            .filter(|attempt| *attempt != 0)
            .ok_or("scalar_attempt_unavailable"),
        Some(Value::Null) => match value.get("harness_provider_attempt") {
            Some(Value::Number(number)) => number
                .as_u64()
                .filter(|attempt| *attempt != 0)
                .ok_or("scalar_attempt_unavailable"),
            _ => Err("scalar_attempt_unavailable"),
        },
        _ => Err("scalar_attempt_unavailable"),
    }
}

/// Parses the strict current scalar identity and closed record kind.
fn diagnostic_record(
    instance: &str,
    value: &Value,
    prompt: Option<AgentPromptId>,
) -> Result<DiagnosticRecord, &'static str> {
    if !matches!(
        value.get("record_kind").and_then(Value::as_str),
        Some("dispatch" | "attempt_end")
    ) {
        return Err("malformed_current_cache_diagnostic");
    }
    let sequence = value
        .get("record_seq")
        .and_then(Value::as_u64)
        .ok_or("malformed_current_cache_diagnostic")?;
    let run = diagnostic_id(value, "producer_run_id")?;
    let attempt = diagnostic_id(value, "attempt_id")?;
    let agent = value
        .get("agent_id")
        .and_then(Value::as_str)
        .and_then(|id| AgentId::parse(id).ok())
        .ok_or("malformed_current_cache_diagnostic")?;
    Ok(DiagnosticRecord {
        id: RecordId {
            instance: instance.to_owned(),
            run,
            sequence,
        },
        value: value.clone(),
        prompt,
        agent,
        attempt,
    })
}

/// Recognizes the common scalar identity needed by operation-scoped records.
fn cache_diagnostic_header(value: &Value) -> bool {
    matches!(
        value.get("record_kind").and_then(Value::as_str),
        Some("dispatch" | "attempt_end")
    ) && value.get("record_seq").and_then(Value::as_u64).is_some()
        && diagnostic_id(value, "producer_run_id").is_ok()
        && diagnostic_id(value, "attempt_id").is_ok()
}

/// Reads one lowercase 128-bit diagnostic identity.
fn diagnostic_id(value: &Value, field: &str) -> Result<String, &'static str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .map(str::to_owned)
        .ok_or("malformed_current_cache_diagnostic")
}

/// Produces a report-local content-free source label.
fn record_label(ordinal: usize) -> String {
    format!("scalar-record-{}", ordinal.saturating_add(1))
}

/// Emits one sanitized scalar source row addressable by later derived records.
fn append_scalar_record(
    report: &mut CacheReport,
    options: &CacheOptions,
    remaining: &mut u64,
    record: &DiagnosticRecord,
    label: &str,
) -> Result<(), InspectError> {
    let kind = match record.value["record_kind"].as_str() {
        Some("dispatch") => "dispatch",
        Some("attempt_end") => "attempt_end",
        _ => unreachable!("validated scalar record kind"),
    };
    let reported = if kind == "dispatch" {
        serde_json::json!({
            "operation": closed(&record.value, "operation",
                &["inference", "standalone_compaction", "cache_refresh"]),
            "logical_attempt": unsigned(&record.value, "logical_attempt"),
            "harness_provider_attempt": unsigned(&record.value, "harness_provider_attempt"),
            "wire_dispatch_index": unsigned(&record.value, "wire_dispatch_index"),
            "backend": closed(&record.value, "backend", &["responses", "chat_completions"]),
            "transport": closed(&record.value, "transport",
                &["websocket", "http_sse", "http-sse"]),
            "model_observed": record.value.get("effective_model")
                .is_some_and(|model| model.as_str().is_some_and(|model| model.len() <= 128)),
            "request_form": closed(&record.value, "request_form",
                &["full", "anchored_suffix", "repair_full", "other"]),
            "previous_response_present": record.value.get("previous_response_present")
                .and_then(Value::as_bool),
            "anchor_validation": closed(&record.value, "anchor_validation",
                &["matched", "mismatched", "unavailable", "not_applicable"]),
            "connection_state": closed(&record.value, "connection_state",
                &["new", "reused", "replaced", "not_applicable", "unknown"]),
            "repair_used": record.value.get("repair_used").and_then(Value::as_bool),
        })
    } else {
        serde_json::json!({
            "operation": closed(&record.value, "operation",
                &["inference", "standalone_compaction", "cache_refresh"]),
            "logical_attempt": unsigned(&record.value, "logical_attempt"),
            "harness_provider_attempt": unsigned(&record.value, "harness_provider_attempt"),
            "dispatch_count": unsigned(&record.value, "dispatch_count"),
            "successful_dispatch_index": unsigned(&record.value, "successful_dispatch_index"),
            "outcome": closed(&record.value, "outcome",
                &["success", "error", "canceled", "pre_dispatch_failure"]),
            "semantic_progress": record.value.get("semantic_progress").and_then(Value::as_bool),
            "repair_used": record.value.get("repair_used").and_then(Value::as_bool),
            "reconnect_count": unsigned(&record.value, "reconnect_count"),
            "chain_strip_count": unsigned(&record.value, "chain_strip_count"),
            "usage": safe_usage(&record.value),
            "attribution_status": closed(&record.value, "attribution_status",
                &["absent", "complete", "truncated", "malformed", "unsupported_shape"]),
        })
    };
    let coverage = if scalar_projection_valid(&record.value) {
        "capture_local"
    } else {
        *report
            .gaps
            .entry("malformed_cache_diagnostic_projection")
            .or_default() += 1;
        "sanitized_partial"
    };
    super::append(
        report,
        options,
        remaining,
        kind,
        serde_json::json!({
            "agent_prompt_id": record.prompt,
            "canonical": null,
            "reported": reported,
            "derived": {
                "method": "sanitized_scalar_capture_v0",
                "record_label": label,
                "coverage": coverage
            }
        }),
    )
}

/// Retains one closed string value or projects unknown/malformed as null.
fn closed(value: &Value, field: &str, allowed: &[&str]) -> Value {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|candidate| allowed.contains(candidate))
        .map_or(Value::Null, |candidate| candidate.into())
}

/// Produces an internal equality key; it is never emitted.
fn geometry_regime_key(value: &Value, group_by: &[CacheGroup]) -> String {
    let mut key = serde_json::Map::new();
    if group_by.contains(&CacheGroup::Model) {
        key.insert(
            "model".into(),
            value
                .get("effective_model")
                .and_then(Value::as_str)
                .filter(|model| model.len() <= 128)
                .map_or(Value::Null, Into::into),
        );
    }
    if group_by.contains(&CacheGroup::Backend) {
        key.insert(
            "backend".into(),
            serde_json::json!({
                "adapter": closed(value, "backend", &["responses", "chat_completions"]),
                "transport": closed(value, "transport", &["websocket", "http_sse", "http-sse"]),
            }),
        );
    }
    if group_by.contains(&CacheGroup::Controls) {
        key.insert(
            "controls".into(),
            serde_json::json!({
                "reasoning": closed(value, "reasoning_selector",
                    &["none", "minimal", "low", "medium", "high", "xhigh"]),
                "tool_choice": closed(value, "tool_choice", &["auto", "none", "required"]),
                "service_tier": closed(value, "service_tier",
                    &["auto", "default", "flex", "priority", "scale"]),
                "cache_mode": closed(value, "cache_mode", &["none", "ephemeral", "retained"]),
                "cache_ttl_seconds": unsigned(value, "cache_ttl_seconds"),
            }),
        );
    }
    serde_json::to_string(&key).expect("closed scalar regime serializes")
}

/// Produces the public regime with a report-local model label.
fn geometry_display_regime(
    value: &Value,
    model_label: Option<String>,
    group_by: &[CacheGroup],
) -> Value {
    let mut regime = serde_json::Map::new();
    if group_by.contains(&CacheGroup::Model) {
        regime.insert("model".into(), model_label.into());
    }
    if group_by.contains(&CacheGroup::Backend) {
        regime.insert(
            "backend".into(),
            serde_json::json!({
                "adapter": closed(value, "backend", &["responses", "chat_completions"]),
                "transport": closed(value, "transport", &["websocket", "http_sse", "http-sse"]),
            }),
        );
    }
    if group_by.contains(&CacheGroup::Controls) {
        regime.insert(
            "controls".into(),
            serde_json::json!({
                "reasoning": closed(value, "reasoning_selector",
                    &["none", "minimal", "low", "medium", "high", "xhigh"]),
                "tool_choice": closed(value, "tool_choice", &["auto", "none", "required"]),
                "service_tier": closed(value, "service_tier",
                    &["auto", "default", "flex", "priority", "scale"]),
                "cache_mode": closed(value, "cache_mode", &["none", "ephemeral", "retained"]),
                "cache_ttl_seconds": unsigned(value, "cache_ttl_seconds"),
            }),
        );
    }
    Value::Object(regime)
}

/// Retains one optional unsigned counter.
fn unsigned(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

/// Projects only the approved provider usage counter names and unsigned values.
fn safe_usage(value: &Value) -> Value {
    let usage = value.get("reported_usage").unwrap_or(&Value::Null);
    serde_json::json!({
        "input_tokens": unsigned(usage, "input_tokens"),
        "read_tokens": unsigned(usage, "read_tokens"),
        "write_tokens": unsigned(usage, "write_tokens"),
        "output_tokens": unsigned(usage, "output_tokens"),
        "reasoning_output_tokens": unsigned(usage, "reasoning_output_tokens"),
        "miss_tokens": unsigned(usage, "miss_tokens"),
        "storage_token_micros": unsigned(usage, "storage_token_micros"),
    })
}

/// Projects bounded structural attribution and counters without arbitrary
/// provider keys, strings, or nested payloads.
fn safe_attribution(value: &Value) -> (Vec<Value>, bool) {
    let Some(entries) = value.get("attribution").and_then(Value::as_array) else {
        return (
            Vec::new(),
            value
                .get("attribution")
                .is_some_and(|value| !value.is_null()),
        );
    };
    let mut sanitized_partial = entries.len() > 4096;
    let retained = entries
        .iter()
        .take(4096)
        .filter_map(|entry| {
            let scope = closed(entry, "scope", &["item", "request_field", "content"]);
            if scope.is_null() || !entry.is_object() {
                sanitized_partial = true;
                None
            } else {
                for field in [
                    "parent_index",
                    "observed_ordinal",
                    "input_tokens",
                    "read_tokens",
                    "write_tokens",
                    "output_tokens",
                ] {
                    if entry
                        .get(field)
                        .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
                    {
                        sanitized_partial = true;
                    }
                }
                if entry.get("mapping").is_some_and(|value| {
                    !value.is_null() && closed(entry, "mapping", &["exact", "unresolved"]).is_null()
                }) {
                    sanitized_partial = true;
                }
                Some(serde_json::json!({
                    "scope": scope,
                    "parent_index": unsigned(entry, "parent_index"),
                    "observed_ordinal": unsigned(entry, "observed_ordinal"),
                    "mapping": closed(entry, "mapping", &["exact", "unresolved"]),
                    "input_tokens": unsigned(entry, "input_tokens"),
                    "read_tokens": unsigned(entry, "read_tokens"),
                    "write_tokens": unsigned(entry, "write_tokens"),
                    "output_tokens": unsigned(entry, "output_tokens"),
                }))
            }
        })
        .collect();
    (retained, sanitized_partial)
}

/// Preserves the producer's explicit attribution evidence grade.
fn attribution_coverage(value: &Value, sanitized_partial: bool) -> &'static str {
    if sanitized_partial {
        return "sanitized_partial";
    }
    if value
        .get("malformed_fields")
        .and_then(Value::as_array)
        .is_some_and(|fields| !fields.is_empty())
    {
        return "reported_malformed";
    }
    match value.get("attribution_status").and_then(Value::as_str) {
        Some("complete") => "reported_complete",
        Some("truncated") => "reported_truncated",
        Some("malformed") => "reported_malformed",
        Some("unsupported_shape") => "reported_unsupported",
        _ => "reported_absent",
    }
}

/// Checks the fields consumed by sanitized projections without requiring
/// unavailable optional evidence.
fn scalar_projection_valid(value: &Value) -> bool {
    let operation = closed(
        value,
        "operation",
        &["inference", "standalone_compaction", "cache_refresh"],
    );
    if operation.is_null() {
        return false;
    }
    match value.get("record_kind").and_then(Value::as_str) {
        Some("dispatch") => {
            unsigned(value, "wire_dispatch_index").is_some()
                && !closed(value, "backend", &["responses", "chat_completions"]).is_null()
                && !closed(value, "transport", &["websocket", "http_sse", "http-sse"]).is_null()
        }
        Some("attempt_end") => {
            unsigned(value, "dispatch_count").is_some()
                && !closed(
                    value,
                    "outcome",
                    &["success", "error", "canceled", "pre_dispatch_failure"],
                )
                .is_null()
                && !closed(
                    value,
                    "attribution_status",
                    &[
                        "absent",
                        "complete",
                        "truncated",
                        "malformed",
                        "unsupported_shape",
                    ],
                )
                .is_null()
        }
        _ => false,
    }
}

/// Greatest common divisor for empirical reported-token samples.
fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

/// Requires one complete, unique `1..=dispatch_count` capture-local dispatch
/// set before continuity or geometry can claim complete evidence.
fn dispatch_continuity(
    dispatches: &[&DiagnosticRecord],
    attempt_end: &DiagnosticRecord,
) -> Result<(), &'static str> {
    let count =
        unsigned(&attempt_end.value, "dispatch_count").ok_or("attempt_dispatch_count_malformed")?;
    let mut indices = dispatches
        .iter()
        .map(|record| unsigned(&record.value, "wire_dispatch_index"))
        .collect::<Option<Vec<_>>>()
        .ok_or("attempt_dispatch_index_malformed")?;
    indices.sort_unstable();
    if indices.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("attempt_dispatch_index_duplicate");
    }
    if indices.len() as u64 != count || indices.iter().copied().ne((1..=count).collect::<Vec<_>>())
    {
        return Err("attempt_dispatch_evidence_incomplete");
    }
    Ok(())
}
