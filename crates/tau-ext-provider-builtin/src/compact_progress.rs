//! Content-free native compaction activity sampling.

use std::time::Instant;

use crate::{
    Event, HarnessInputMessage, PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL, ProviderReportSink,
    ProviderResponseStatusUpdate, ProviderResponseUpdated, ResponseUpdateTarget,
};

/// Samples validated attempt-local counts without handling any provider output.
#[derive(Default)]
pub(crate) struct CompactProgress {
    /// Last successfully published counter.
    emitted_count: u64,
    /// Last successful publication time, absent until the first activity.
    emitted_at: Option<Instant>,
    /// Latest validated count, including updates not yet sampled.
    latest_count: u64,
}

impl CompactProgress {
    /// Publishes the first activity promptly and subsequent changes at normal
    /// cadence.
    pub(crate) fn emit(
        &mut self,
        count: u64,
        target: &ResponseUpdateTarget<'_>,
        writer: &mut impl ProviderReportSink,
        now: Instant,
    ) {
        self.latest_count = count;
        self.publish(count, target, writer, now, false);
    }

    /// Flushes the final count before a successful provider terminal report.
    /// It remains activity: only canonical transaction success can finish UI.
    pub(crate) fn flush(
        &mut self,
        target: &ResponseUpdateTarget<'_>,
        writer: &mut impl ProviderReportSink,
    ) {
        self.publish(self.latest_count, target, writer, Instant::now(), true);
    }

    /// Applies public cadence without exposing provider payloads.
    fn publish(
        &mut self,
        count: u64,
        target: &ResponseUpdateTarget<'_>,
        writer: &mut impl ProviderReportSink,
        now: Instant,
        terminal_flush: bool,
    ) {
        if count == 0
            || count == self.emitted_count
            || !terminal_flush
                && self.emitted_at.is_some_and(|last| {
                    now.saturating_duration_since(last) < PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                })
        {
            return;
        }
        let update = ProviderResponseUpdated {
            agent_prompt_id: target.agent_prompt_id.clone(),
            agent_id: target.agent_id.clone(),
            originator: target.originator.clone(),
            deltas: Vec::new(),
            compaction: Some(tau_proto::ProviderResponseCompactionUpdate {
                status: tau_proto::ProviderResponseCompactionStatus::Started,
                current: Some(count),
                total: None,
                original_input_tokens: None,
                compaction_output_tokens: None,
            }),
            response_stats: None,
            status: Some(ProviderResponseStatusUpdate {
                text: "Compacting…".to_owned(),
                clear_response: false,
                retry: None,
                native_tool: None,
            }),
        };
        if writer
            .send_report(HarnessInputMessage::emit_transient(
                Event::ProviderResponseUpdatedReported(update),
            ))
            .is_ok()
        {
            self.emitted_count = count;
            self.emitted_at = Some(now);
        }
    }
}
