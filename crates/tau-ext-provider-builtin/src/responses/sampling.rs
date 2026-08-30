//! Extension-owned public response sampling and delta emission.

use std::collections::BTreeMap;
use std::time as path_std_time;

use crate::report_sink::ProviderReportSink;

pub(super) const RESPONSE_UPDATE_INTERVAL: std::time::Duration =
    path_std_time::Duration::from_secs(1);

/// Prompt-local cadence and append-delta state for public transient events.
pub(super) struct ResponsesResponseSampler {
    /// Dispatch-origin clock used for elapsed response stats.
    pub(super) dispatch_origin: std::time::Instant,
    /// Last successfully written transient sample.
    pub(super) last_emitted_at: Option<std::time::Instant>,
    /// Stats baseline from the last successfully written sample.
    pub(super) last_sample: tau_proto::ProviderResponseStatsSample,
    /// Whether the first non-empty byte sample already bypassed cadence.
    pub(super) emitted_non_empty: bool,
    /// Immutable elapsed duration captured at the first qualifying parser
    /// state.
    first_semantic_output_elapsed: Option<std::time::Duration>,
    /// Stable indexed semantic items materialized only when sampling is due.
    pub(super) latest_items: Vec<tau_provider_responses::AttemptOutputItem>,
    /// Cumulative provider response bytes for `latest_items`.
    pub(super) latest_bytes: u64,
    /// Previously emitted assistant text keyed by stable output index.
    pub(super) emitted_text: BTreeMap<usize, String>,
    /// Previously emitted reasoning text keyed by stable output index.
    pub(super) emitted_reasoning: BTreeMap<usize, String>,
}

impl ResponsesResponseSampler {
    pub(super) fn new() -> Self {
        Self::new_at(path_std_time::Instant::now())
    }

    /// Construct a sampler at an explicit clock instant.
    pub(super) fn new_at(dispatch_origin: std::time::Instant) -> Self {
        Self {
            dispatch_origin,
            last_emitted_at: None,
            last_sample: Default::default(),
            emitted_non_empty: false,
            first_semantic_output_elapsed: None,
            latest_items: Vec::new(),
            latest_bytes: 0,
            emitted_text: BTreeMap::new(),
            emitted_reasoning: BTreeMap::new(),
        }
    }

    /// Re-anchor sampling and first-semantic timing at the finite backend
    /// request's actual send or frame-enqueue boundary.
    pub(super) fn mark_dispatched(&mut self, dispatched_at: std::time::Instant) {
        if self.last_emitted_at.is_none() {
            self.dispatch_origin = dispatched_at;
        }
    }

    pub(super) fn emit_if_due<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        progress: tau_provider_responses::AttemptProgressRef<'_>,
        writer: &mut S,
    ) {
        let now = path_std_time::Instant::now();
        self.observe_progress(now, progress.has_timed_semantic_output());
        let bytes = progress.response_bytes_received();
        if !self.is_due(now, bytes, false) {
            return;
        }
        self.latest_items = progress.materialize_output();
        self.latest_bytes = bytes;
        self.emit_at(apid, prompt, writer, now, false);
    }

    /// Capture the first qualifying synchronous state observation before
    /// publication cadence filtering.
    fn observe_progress(&mut self, now: std::time::Instant, has_timed_semantic_output: bool) {
        if self.first_semantic_output_elapsed.is_none() && has_timed_semantic_output {
            self.first_semantic_output_elapsed =
                Some(now.saturating_duration_since(self.dispatch_origin));
        }
    }

    /// Observe semantic state at an explicit instant for deterministic timing
    /// tests without a wall-clock sleep.
    #[cfg(test)]
    pub(super) fn observe_progress_at(
        &mut self,
        now: std::time::Instant,
        has_timed_semantic_output: bool,
    ) {
        self.observe_progress(now, has_timed_semantic_output);
    }

    pub(super) fn flush<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        writer: &mut S,
    ) {
        self.emit_at(apid, prompt, writer, path_std_time::Instant::now(), true);
    }

    pub(super) fn emit_at<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        writer: &mut S,
        now: std::time::Instant,
        terminal: bool,
    ) {
        let current = tau_proto::ProviderResponseStatsSample {
            response_bytes_received: self.latest_bytes,
            elapsed_micros: now
                .saturating_duration_since(self.dispatch_origin)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        };
        if !self.is_due(now, current.response_bytes_received, terminal) {
            return;
        }
        let deltas = if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
            Vec::new()
        } else {
            self.deltas()
        };
        if deltas.is_empty() && current == self.last_sample {
            return;
        }
        let event = tau_proto::ProviderResponseUpdated {
            agent_prompt_id: apid.clone(),
            agent_id: prompt.agent_id.clone(),
            deltas,
            compaction: None,
            status: None,
            response_stats: Some(tau_proto::ProviderResponseStats {
                current,
                previous: self.last_sample,
                first_semantic_output_elapsed_micros: self
                    .first_semantic_output_elapsed
                    .map(duration_micros),
            }),
            originator: prompt.originator.clone(),
        };
        if writer
            .send_report(tau_proto::HarnessInputMessage::emit_transient(
                tau_proto::Event::ProviderResponseUpdatedReported(event),
            ))
            .is_ok()
        {
            self.last_sample = current;
            self.last_emitted_at = Some(now);
            self.emitted_non_empty |= current.response_bytes_received > 0;
        }
    }

    fn is_due(&self, now: std::time::Instant, bytes: u64, terminal: bool) -> bool {
        terminal
            || !self.emitted_non_empty && 0 < bytes
            || self.last_emitted_at.map_or(
                now.saturating_duration_since(self.dispatch_origin) >= RESPONSE_UPDATE_INTERVAL,
                |last| now.saturating_duration_since(last) >= RESPONSE_UPDATE_INTERVAL,
            )
    }

    pub(super) fn deltas(&mut self) -> Vec<tau_proto::ProviderResponseTextDelta> {
        let mut out = Vec::new();
        for output in &self.latest_items {
            let index = output.output_index as usize;
            let (map, text, reasoning) = match &output.item {
                tau_proto::ContextItem::Message(message) => (
                    &mut self.emitted_text,
                    message
                        .content
                        .iter()
                        .map(|part| match part {
                            tau_proto::ContentPart::Text { text }
                            | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                            | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                        })
                        .collect::<String>(),
                    false,
                ),
                tau_proto::ContextItem::ReasoningText(reasoning) => {
                    (&mut self.emitted_reasoning, reasoning.text.clone(), true)
                }
                _ => continue,
            };
            let previous = map.entry(index).or_default();
            if let Some(suffix) = text
                .strip_prefix(previous.as_str())
                .filter(|suffix| !suffix.is_empty())
            {
                let suffix = suffix.to_owned();
                previous.push_str(&suffix);
                out.push(if reasoning {
                    tau_proto::ProviderResponseTextDelta::ReasoningText {
                        output_index: output.output_index,
                        kind: tau_proto::ReasoningTextKind::Full,
                        text: suffix,
                    }
                } else {
                    tau_proto::ProviderResponseTextDelta::Message {
                        output_index: output.output_index,
                        text: suffix,
                        phase: None,
                    }
                });
            }
        }
        out
    }
}

fn duration_micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}
