//! Extension-owned public response sampling and delta emission.

use std::time as path_std_time;

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
            tau_provider_chat_completions::DisplayOutputKind,
            &str,
            tau_provider_chat_completions::DisplayGeneration,
        ),
    );
}

impl SamplingProgress for tau_provider_chat_completions::AttemptProgress<'_> {
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
            tau_provider_chat_completions::DisplayOutputKind,
            &str,
            tau_provider_chat_completions::DisplayGeneration,
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

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

/// Emission cursor for one stable display channel.
struct DisplayCursor {
    /// Accepted replacement generation.
    generation: tau_provider_chat_completions::DisplayGeneration,
    /// UTF-8 byte offset already emitted.
    bytes: usize,
}

pub(super) const RESPONSE_UPDATE_INTERVAL: std::time::Duration =
    path_std_time::Duration::from_secs(1);

/// Prompt-local cadence and append-delta state for public transient events.
pub(super) struct ResponseSampler {
    /// Attempt start used for elapsed response stats.
    pub(super) started_at: std::time::Instant,
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
    pub(super) latest_items: Vec<tau_provider_chat_completions::AttemptOutputItem>,
    /// Latest cumulative provider response bytes observed by the sampler.
    pub(super) latest_bytes: u64,
    /// Assistant-text emission cursors keyed by stable output index.
    emitted_text: BTreeMap<usize, DisplayCursor>,
    /// Reasoning-text emission cursors keyed by stable output index.
    emitted_reasoning: BTreeMap<usize, DisplayCursor>,
    /// Deltas derived from the current borrowed progress view.
    pending_deltas: Vec<tau_proto::ProviderResponseTextDelta>,
}

impl ResponseSampler {
    pub(super) fn new() -> Self {
        Self {
            started_at: path_std_time::Instant::now(),
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

    /// Align the response-stat clock to the backend's first send/poll boundary.
    pub(super) fn mark_dispatched(&mut self, dispatched_at: std::time::Instant) {
        if self.last_emitted_at.is_none() {
            self.started_at = dispatched_at;
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
                    tau_provider_chat_completions::DisplayOutputKind::Message => {
                        &mut self.emitted_text
                    }
                    tau_provider_chat_completions::DisplayOutputKind::Reasoning => {
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
        progress: tau_provider_chat_completions::AttemptProgress<'_>,
        writer: &mut S,
    ) {
        self.emit_if_due_from(apid, prompt, &progress, writer);
    }

    /// Capture the first qualifying synchronous state observation before
    /// publication cadence filtering.
    fn observe_progress(&mut self, now: std::time::Instant, has_timed_semantic_output: bool) {
        if self.first_semantic_output_elapsed.is_none() && has_timed_semantic_output {
            self.first_semantic_output_elapsed =
                Some(now.saturating_duration_since(self.started_at));
        }
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
                .saturating_duration_since(self.started_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        };
        if !self.is_due(now, current.response_bytes_received, terminal) {
            return;
        }
        let deltas = if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
            Vec::new()
        } else {
            let mut deltas = std::mem::take(&mut self.pending_deltas);
            deltas.extend(self.deltas());
            deltas
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
                now.saturating_duration_since(self.started_at) >= RESPONSE_UPDATE_INTERVAL,
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
                        })
                        .collect::<String>(),
                    tau_provider_chat_completions::DisplayOutputKind::Message,
                ),
                tau_proto::ContextItem::ReasoningText(reasoning) => (
                    &mut self.emitted_reasoning,
                    reasoning.text.clone(),
                    tau_provider_chat_completions::DisplayOutputKind::Reasoning,
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
    generation: tau_provider_chat_completions::DisplayGeneration,
    kind: tau_provider_chat_completions::DisplayOutputKind,
) {
    let cursor = cursors
        .entry(output_index as usize)
        .or_insert(DisplayCursor {
            generation,
            bytes: 0,
        });
    if cursor.generation != generation || cursor.bytes > text.len() {
        return;
    }
    let Some(suffix) = text.get(cursor.bytes..).filter(|suffix| !suffix.is_empty()) else {
        return;
    };
    cursor.bytes = text.len();
    out.push(match kind {
        tau_provider_chat_completions::DisplayOutputKind::Reasoning => {
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index,
                kind: tau_proto::ReasoningTextKind::Full,
                text: suffix.to_owned(),
            }
        }
        tau_provider_chat_completions::DisplayOutputKind::Message => {
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
