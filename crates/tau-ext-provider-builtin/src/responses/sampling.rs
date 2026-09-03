//! Extension-owned public response sampling and delta emission.

use std::collections::BTreeMap;
use std::time as path_std_time;

use crate::output_cost_observation::SamplerObservation;
use crate::report_sink::ProviderReportSink;

/// Borrowed cadence and display inputs consumed by the sampler.
pub(super) trait SamplingProgress {
    /// Return cumulative provider response bytes.
    fn response_bytes_received(&self) -> u64;
    /// Return whether semantic timing has qualified.
    fn has_timed_semantic_output(&self) -> bool;
    /// Visit accepted display channels.
    fn visit_display_output(
        &self,
        visit: &mut dyn FnMut(
            u32,
            tau_provider_responses::DisplayOutputKind,
            &str,
            tau_provider_responses::DisplayGeneration,
        ),
    );
}

impl SamplingProgress for tau_provider_responses::AttemptProgressRef<'_> {
    fn response_bytes_received(&self) -> u64 {
        self.response_bytes_received()
    }

    fn has_timed_semantic_output(&self) -> bool {
        self.has_timed_semantic_output()
    }

    fn visit_display_output(
        &self,
        visit: &mut dyn FnMut(
            u32,
            tau_provider_responses::DisplayOutputKind,
            &str,
            tau_provider_responses::DisplayGeneration,
        ),
    ) {
        self.visit_display_output(|output| {
            visit(
                output.output_index,
                output.kind,
                output.text,
                output.generation,
            );
        });
    }
}

impl SamplingProgress for tau_provider_responses::AttemptSuccess {
    fn response_bytes_received(&self) -> u64 {
        self.response_bytes_received
    }

    fn has_timed_semantic_output(&self) -> bool {
        self.has_timed_semantic_output()
    }

    fn visit_display_output(
        &self,
        visit: &mut dyn FnMut(
            u32,
            tau_provider_responses::DisplayOutputKind,
            &str,
            tau_provider_responses::DisplayGeneration,
        ),
    ) {
        self.visit_display_output(|output| {
            visit(
                output.output_index,
                output.kind,
                output.text,
                output.generation,
            );
        });
    }
}

/// Emission cursor for one stable display channel.
struct DisplayCursor {
    /// Accepted replacement generation.
    generation: tau_provider_responses::DisplayGeneration,
    /// UTF-8 byte offset already emitted.
    bytes: usize,
    /// Exact emitted prefix retained because public Responses permits
    /// cumulative replacements that can later recover this prefix.
    emitted_prefix: String,
}

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
    /// Stable indexed semantic items installed only for terminal fallback.
    pub(super) latest_items: Vec<tau_provider_responses::AttemptOutputItem>,
    /// Latest cumulative provider response bytes observed by the sampler.
    pub(super) latest_bytes: u64,
    /// Assistant-text emission cursors keyed by stable output index.
    emitted_text: BTreeMap<usize, DisplayCursor>,
    /// Reasoning-text emission cursors keyed by stable output index.
    emitted_reasoning: BTreeMap<usize, DisplayCursor>,
    /// Deltas derived from the current borrowed progress view.
    pending_deltas: Vec<tau_proto::ProviderResponseTextDelta>,
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
            pending_deltas: Vec::new(),
        }
    }

    /// Re-anchor sampling and first-semantic timing at the finite backend
    /// request's actual send or frame-enqueue boundary.
    pub(super) fn mark_dispatched(&mut self, dispatched_at: std::time::Instant) {
        if self.last_emitted_at.is_none() {
            self.dispatch_origin = dispatched_at;
        }
    }

    pub(super) fn emit_if_due_from<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        progress: &impl SamplingProgress,
        writer: &mut S,
    ) {
        let now = path_std_time::Instant::now();
        self.observe_progress(now, progress.has_timed_semantic_output());
        let bytes = progress.response_bytes_received();
        if !self.is_due(now, bytes, false) {
            return;
        }
        self.latest_bytes = bytes;
        if prompt.operation != tau_proto::PromptOperation::StandaloneCompaction {
            progress.visit_display_output(&mut |output_index, kind, text, generation| {
                let map = match kind {
                    tau_provider_responses::DisplayOutputKind::Message => &mut self.emitted_text,
                    tau_provider_responses::DisplayOutputKind::Reasoning => {
                        &mut self.emitted_reasoning
                    }
                };
                append_delta(
                    &mut self.pending_deltas,
                    map,
                    output_index,
                    text,
                    generation,
                    kind,
                );
            });
        }
        self.emit_at(apid, prompt, writer, now, false);
    }

    pub(super) fn emit_if_due<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        progress: tau_provider_responses::AttemptProgressRef<'_>,
        writer: &mut S,
    ) {
        self.emit_if_due_from(apid, prompt, &progress, writer);
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

    /// Borrow terminal output for the final delta before durable items move.
    pub(super) fn flush_from<S: ProviderReportSink>(
        &mut self,
        apid: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        progress: &impl SamplingProgress,
        writer: &mut S,
    ) {
        let now = path_std_time::Instant::now();
        self.observe_progress(now, progress.has_timed_semantic_output());
        self.latest_bytes = progress.response_bytes_received();
        if prompt.operation != tau_proto::PromptOperation::StandaloneCompaction {
            progress.visit_display_output(&mut |output_index, kind, text, generation| {
                let map = match kind {
                    tau_provider_responses::DisplayOutputKind::Message => &mut self.emitted_text,
                    tau_provider_responses::DisplayOutputKind::Reasoning => {
                        &mut self.emitted_reasoning
                    }
                };
                append_delta(
                    &mut self.pending_deltas,
                    map,
                    output_index,
                    text,
                    generation,
                    kind,
                );
            });
        }
        self.emit_at(apid, prompt, writer, now, true);
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
        let mut output_cost = SamplerObservation::enabled(terminal);
        let deltas = if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
            Vec::new()
        } else {
            let mut deltas = std::mem::take(&mut self.pending_deltas);
            deltas.extend(self.deltas());
            deltas
        };
        if deltas.is_empty() && current == self.last_sample {
            if let Some(observation) = &mut output_cost {
                observation.count_deltas(&deltas);
            }
            if let Some(observation) = output_cost {
                observation.finish("unchanged");
            }
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
        if let Some(observation) = &mut output_cost {
            observation.count_deltas(&event.deltas);
        }
        let written = writer
            .send_sampled_report(
                tau_proto::HarnessInputMessage::emit_transient(
                    tau_proto::Event::ProviderResponseUpdatedReported(event),
                ),
                output_cost,
            )
            .is_ok();
        if written {
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
            let (map, text, kind) = match &output.item {
                tau_proto::ContextItem::Message(message) => (
                    &mut self.emitted_text,
                    message
                        .content
                        .iter()
                        .map(|part| match part {
                            tau_proto::ContentPart::Text { text }
                            | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                            | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                            tau_proto::ContentPart::UrlCitation { .. }
                            | tau_proto::ContentPart::CitationMetadataInvalid => "",
                        })
                        .collect::<String>(),
                    tau_provider_responses::DisplayOutputKind::Message,
                ),
                tau_proto::ContextItem::ReasoningText(reasoning) => (
                    &mut self.emitted_reasoning,
                    reasoning.text.clone(),
                    tau_provider_responses::DisplayOutputKind::Reasoning,
                ),
                _ => continue,
            };
            append_delta(
                &mut out,
                map,
                output.output_index,
                &text,
                output.display_generation,
                kind,
            );
        }
        out
    }
}

fn append_delta(
    out: &mut Vec<tau_proto::ProviderResponseTextDelta>,
    cursors: &mut BTreeMap<usize, DisplayCursor>,
    output_index: u32,
    text: &str,
    generation: tau_provider_responses::DisplayGeneration,
    kind: tau_provider_responses::DisplayOutputKind,
) {
    let cursor = cursors
        .entry(output_index as usize)
        .or_insert_with(|| DisplayCursor {
            generation,
            bytes: 0,
            emitted_prefix: String::new(),
        });
    if cursor.generation != generation {
        if !text.starts_with(&cursor.emitted_prefix) {
            return;
        }
        cursor.generation = generation;
        cursor.bytes = cursor.emitted_prefix.len();
    }
    if cursor.bytes > text.len() {
        return;
    }
    let Some(suffix) = text.get(cursor.bytes..).filter(|suffix| !suffix.is_empty()) else {
        return;
    };
    cursor.bytes = text.len();
    cursor.emitted_prefix.push_str(suffix);
    out.push(match kind {
        tau_provider_responses::DisplayOutputKind::Reasoning => {
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index,
                kind: tau_proto::ReasoningTextKind::Full,
                text: suffix.to_owned(),
            }
        }
        tau_provider_responses::DisplayOutputKind::Message => {
            tau_proto::ProviderResponseTextDelta::Message {
                output_index,
                text: suffix.to_owned(),
                phase: None,
            }
        }
    });
}

fn duration_micros(duration: std::time::Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}
