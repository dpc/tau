//! Exact-capture admission, correlation, projection, and index evidence.

use std::collections::{BTreeMap, BTreeSet};

use tau_proto::AgentId;

use super::{DispatchJoin, Inventory};
use crate::InspectError;
use crate::cache::exact_geometry::{self, ExactRequest, ExactResponse};
use crate::cache::{CacheOptions, CacheReport, CacheScanLimits, CacheScope, append};

impl Inventory {
    /// Adds previously indexed fixed-size evidence without restoring bodies.
    pub(in crate::cache) fn extend_index(
        &mut self,
        requests: Vec<ExactRequest>,
        responses: Vec<ExactResponse>,
    ) {
        self.exact_requests.extend(requests);
        self.exact_responses.extend(responses);
    }

    /// Returns qualified request evidence for optional index replacement.
    pub(in crate::cache) fn indexable_exact_requests(&self) -> Vec<ExactRequest> {
        self.exact_requests
            .iter()
            .filter(|request| !request.agent.is_empty() && request.complete)
            .cloned()
            .collect()
    }

    /// Returns correlated response evidence for optional index replacement.
    pub(in crate::cache) fn indexable_exact_responses(&self) -> Vec<ExactResponse> {
        self.exact_responses
            .iter()
            .filter(|response| response.attempt.is_some())
            .cloned()
            .collect()
    }

    /// Returns whether bounded capture scanning completed far enough to replace
    /// an index without silently forgetting evidence.
    pub(in crate::cache) fn index_input_complete(&self) -> bool {
        self.gaps.keys().all(|reason| *reason == "legacy_partial")
    }

    /// Admits one body-free request fingerprint set under the exact-evidence
    /// budget.
    pub(super) fn admit_exact_request(&mut self, request: ExactRequest, limits: &CacheScanLimits) {
        if self.exact_requests.iter().any(|existing| {
            existing.instance == request.instance
                && existing.attempt == request.attempt
                && existing.dispatch == request.dispatch
                && existing.body == request.body
        }) {
            return;
        }
        let charge = 2048_u64.saturating_add((request.items.len() as u64).saturating_mul(160));
        if self.retained_exact_bytes.saturating_add(charge) > limits.working_memory_bytes / 4 {
            self.gap("exact_geometry_memory_limit");
            return;
        }
        self.retained_exact_bytes = self.retained_exact_bytes.saturating_add(charge);
        self.exact_requests.push(request);
    }

    /// Admits one body-free response identity under the exact-evidence budget.
    pub(super) fn admit_exact_response(
        &mut self,
        response: ExactResponse,
        limits: &CacheScanLimits,
    ) {
        if self.exact_responses.iter().any(|existing| {
            existing.instance == response.instance
                && existing.attempt == response.attempt
                && existing.dispatch == response.dispatch
                && existing.response == response.response
        }) {
            return;
        }
        let charge = 512;
        if self.retained_exact_bytes.saturating_add(charge) > limits.working_memory_bytes / 4 {
            self.gap("exact_geometry_memory_limit");
            return;
        }
        self.retained_exact_bytes = self.retained_exact_bytes.saturating_add(charge);
        self.exact_responses.push(response);
    }

    /// Associates current request captures only through explicit
    /// attempt/dispatch identities; null-index public Responses requests
    /// require one unique dispatch.
    pub(super) fn qualify_exact_requests(
        &mut self,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
    ) {
        let mut exact_dispatches: BTreeMap<(String, String, u64), DispatchJoin<'_>> =
            BTreeMap::new();
        let mut attempt_dispatches: BTreeMap<(String, String), Vec<DispatchJoin<'_>>> =
            BTreeMap::new();
        for diagnostic in self.diagnostics.iter().filter(|record| {
            record.value["record_kind"] == "dispatch" && selected_agents.contains(&record.agent)
        }) {
            let Some(index) = diagnostic.value["wire_dispatch_index"].as_u64() else {
                continue;
            };
            let Some(prompt) = diagnostic.prompt.as_ref() else {
                continue;
            };
            let instance = exact_geometry::identity(
                &self.exact_key,
                b"provider-instance",
                &diagnostic.id.instance,
            );
            let attempt =
                exact_geometry::identity(&self.exact_key, b"attempt-id", &diagnostic.attempt);
            let recorded = diagnostic.value["recorded_at_unix_micros"]
                .as_u64()
                .unwrap_or_default();
            exact_dispatches.insert(
                (instance.clone(), attempt.clone(), index),
                (&diagnostic.agent, prompt, recorded),
            );
            attempt_dispatches
                .entry((instance, attempt))
                .or_default()
                .push((&diagnostic.agent, prompt, recorded));
        }
        for request in &mut self.exact_requests {
            if request.indexed {
                continue;
            }
            let Some(attempt) = request.attempt.as_ref() else {
                *report
                    .gaps
                    .entry("exact_request_attempt_identity_missing")
                    .or_default() += 1;
                continue;
            };
            let joined = if let Some(index) = request.dispatch {
                exact_dispatches
                    .get(&(request.instance.clone(), attempt.clone(), index))
                    .copied()
            } else {
                let candidates =
                    attempt_dispatches.get(&(request.instance.clone(), attempt.clone()));
                match candidates.map(Vec::as_slice) {
                    Some([candidate]) => Some(*candidate),
                    Some(_) => {
                        *report
                            .gaps
                            .entry("exact_request_dispatch_ambiguous")
                            .or_default() += 1;
                        None
                    }
                    None => None,
                }
            };
            let Some((agent, prompt, recorded)) = joined else {
                *report
                    .gaps
                    .entry("exact_request_dispatch_missing")
                    .or_default() += 1;
                continue;
            };
            if request.prompt != prompt.as_str() {
                *report
                    .gaps
                    .entry("exact_request_prompt_mismatch")
                    .or_default() += 1;
                continue;
            }
            request.agent = agent.as_str().to_owned();
            request.recorded_at_unix_micros = Some(recorded);
            let diagnostic = self.diagnostics.iter().find(|diagnostic| {
                diagnostic.agent == *agent
                    && diagnostic.prompt.as_ref() == Some(prompt)
                    && diagnostic.value["record_kind"] == "dispatch"
                    && diagnostic.value["recorded_at_unix_micros"].as_u64() == Some(recorded)
            });
            request.request_form = diagnostic
                .and_then(|record| record.value["request_form"].as_str())
                .map(str::to_owned);
        }
    }

    /// Emits bounded structural and ordered-prefix comparisons without exposing
    /// fingerprints or claiming wire bytes, tokenization, or cache residency.
    pub(super) fn project_exact_geometry(
        &self,
        options: &CacheOptions,
        selected_agents: &BTreeSet<AgentId>,
        report: &mut CacheReport,
        remaining: &mut u64,
    ) -> Result<(), InspectError> {
        let mut requests = self
            .exact_requests
            .iter()
            .filter(|request| {
                selected_agents
                    .iter()
                    .any(|agent| agent.as_str() == request.agent)
                    && match &options.scope {
                        CacheScope::Session(session) => request.session == session.as_str(),
                        CacheScope::Agent { .. } => true,
                    }
                    && options
                        .prompt
                        .as_ref()
                        .is_none_or(|prompt| prompt.as_str() == request.prompt)
            })
            .cloned()
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| {
            (
                left.recorded_at_unix_micros,
                &left.agent,
                &left.prompt,
                left.dispatch,
                &left.body,
            )
                .cmp(&(
                    right.recorded_at_unix_micros,
                    &right.agent,
                    &right.prompt,
                    right.dispatch,
                    &right.body,
                ))
        });
        let mut identities: BTreeMap<_, usize> = BTreeMap::new();
        let mut rejected = BTreeSet::new();
        let mut unique: Vec<ExactRequest> = Vec::new();
        for request in requests {
            let identity = (
                request.instance.clone(),
                request.attempt.clone(),
                request.dispatch,
            );
            if let Some(previous) = identities.get(&identity).copied() {
                if unique[previous].body != request.body {
                    rejected.insert(identity);
                    *report
                        .gaps
                        .entry("exact_request_capture_conflict")
                        .or_default() += 1;
                }
                continue;
            }
            identities.insert(identity, unique.len());
            unique.push(request);
        }
        unique.retain(|request| {
            !rejected.contains(&(
                request.instance.clone(),
                request.attempt.clone(),
                request.dispatch,
            ))
        });
        if unique.is_empty() {
            if !self.exact_requests.is_empty() {
                *report
                    .gaps
                    .entry("selected_exact_request_unavailable")
                    .or_default() += 1;
            }
            return Ok(());
        }
        let mut body_labels = BTreeMap::new();
        let mut controls_labels = BTreeMap::new();
        let mut route_labels = BTreeMap::new();
        let labels = (0..unique.len())
            .map(|ordinal| format!("exact-request-{}", ordinal.saturating_add(1)))
            .collect::<Vec<_>>();
        for (request, label) in unique.iter().zip(&labels) {
            let body_label = local_label(&mut body_labels, "shape", &request.body);
            let controls_label = local_label(&mut controls_labels, "controls", &request.controls);
            let route_label = local_label(&mut route_labels, "route", &request.route);
            append(
                report,
                options,
                remaining,
                "dispatch",
                serde_json::json!({
                    "agent_prompt_id": request.prompt,
                    "canonical": null,
                    "reported": null,
                    "derived": {
                        "method": "exact_captured_request_shape_v0",
                        "input_records": [label],
                        "coverage": if request.complete { "captured_complete" } else { "partial" },
                        "adapter": request.adapter,
                        "captured_body_shape": body_label,
                        "controls": controls_label,
                        "route": route_label,
                        "input_item_count": request.items.len(),
                        "request_form": request.request_form,
                        "source": if request.indexed { "private_index" } else { "exact_capture" },
                        "raw_wire_byte_equality": "unavailable",
                        "provider_tokenization": "unknown",
                        "provider_residency": "unknown"
                    }
                }),
            )?;
        }
        for (ordinal, pair) in unique.windows(2).enumerate() {
            let left = &pair[0];
            let right = &pair[1];
            let full_prefix_comparable = matches!(
                (left.request_form.as_deref(), right.request_form.as_deref()),
                (Some("full" | "repair_full"), Some("full" | "repair_full"))
            );
            let common_prefix = full_prefix_comparable
                .then(|| exact_geometry::common_prefix(left, right))
                .flatten();
            let visible_prefix_equality = common_prefix.map_or("unknown", |count| {
                if count == left.items.len() && count == right.items.len() {
                    "equal"
                } else {
                    "different"
                }
            });
            let chain = self.chain_continuity(left, right);
            append(
                report,
                options,
                remaining,
                "comparison",
                serde_json::json!({
                    "agent_prompt_id": right.prompt,
                    "canonical": null,
                    "reported": null,
                    "derived": {
                        "method": "exact_captured_request_comparison_v0",
                        "input_records": [&labels[ordinal], &labels[ordinal + 1]],
                        "coverage": if left.complete && right.complete {
                            "captured_structural"
                        } else {
                            "partial"
                        },
                        "captured_body_equality": exact_geometry::equality(
                            Some(&left.body), Some(&right.body)),
                        "control_equality": exact_geometry::equality(
                            Some(&left.controls), Some(&right.controls)),
                        "tools_equality": exact_geometry::equality(
                            Some(&left.tools), Some(&right.tools)),
                        "instructions_equality": exact_geometry::equality(
                            left.instructions.as_ref(), right.instructions.as_ref()),
                        "other_fields_equality": exact_geometry::equality(
                            Some(&left.other), Some(&right.other)),
                        "cache_key_equality": exact_geometry::equality(
                            left.cache_key.as_ref(), right.cache_key.as_ref()),
                        "route_equality": exact_geometry::equality(
                            Some(&left.route), Some(&right.route)),
                        "visible_prefix_equality": visible_prefix_equality,
                        "common_captured_input_items": common_prefix,
                        "chain_continuity": chain,
                        "capture_completeness": if left.complete && right.complete {
                            "complete_bodies"
                        } else {
                            "missing_or_truncated"
                        },
                        "raw_wire_byte_equality": "unavailable",
                        "exact_cache_geometry": "unknown",
                        "provider_tokenization": "unknown",
                        "provider_residency": "unknown"
                    }
                }),
            )?;
            if visible_prefix_equality == "unknown" || chain == "unknown" {
                *report
                    .gaps
                    .entry("exact_comparison_evidence_partial")
                    .or_default() += 1;
            }
        }
        Ok(())
    }

    /// Qualifies one explicit previous-response edge from complete captured
    /// IDs.
    fn chain_continuity(&self, left: &ExactRequest, right: &ExactRequest) -> &'static str {
        let Some(previous) = right.previous_response.as_ref() else {
            return if matches!(right.request_form.as_deref(), Some("full" | "repair_full")) {
                "not_applicable"
            } else {
                "unknown"
            };
        };
        let candidates = self
            .exact_responses
            .iter()
            .filter(|response| {
                response.instance == left.instance
                    && response.attempt == left.attempt
                    && response.dispatch == left.dispatch
            })
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [response] if &response.response == previous => "equal",
            [..] if candidates.len() == 1 => "different",
            _ => "unknown",
        }
    }
}

/// Assigns a report-local ordinal to a private keyed equality class.
fn local_label(labels: &mut BTreeMap<String, String>, prefix: &str, digest: &str) -> String {
    let next = labels.len().saturating_add(1);
    labels
        .entry(digest.to_owned())
        .or_insert_with(|| format!("{prefix}-{next}"))
        .clone()
}
