//! Focused tests for response-sampler timing state.

use std::{io as path_std_io, time as path_std_time};

use super::{RESPONSE_UPDATE_INTERVAL, ResponseSampler, SamplingProgress};

/// Minimal borrowed projection used to exercise the production sampler seam.
struct FakeProgress<'a> {
    /// Cumulative byte count.
    bytes: u64,
    /// Assistant text.
    message: &'a str,
    /// Reasoning text.
    reasoning: &'a str,
}

impl SamplingProgress for FakeProgress<'_> {
    fn response_bytes_received(&self) -> u64 {
        self.bytes
    }

    fn has_timed_semantic_output(&self) -> bool {
        true
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
        visit(
            0,
            tau_provider_chat_completions::DisplayOutputKind::Message,
            self.message,
            Default::default(),
        );
        visit(
            1,
            tau_provider_chat_completions::DisplayOutputKind::Reasoning,
            self.reasoning,
            Default::default(),
        );
    }
}

/// The production due-sample seam publishes borrowed message/reasoning
/// projections and slices multibyte suffixes from byte cursors.
#[test]
fn due_borrowed_projection_publishes_initial_and_unicode_suffix_deltas() {
    let prompt = crate::openai_tests::prompt();
    let mut sampler = ResponseSampler::new();
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_if_due_from(
            &prompt.agent_prompt_id,
            &prompt,
            &FakeProgress {
                bytes: 2,
                message: "a",
                reasoning: "r",
            },
            &mut writer,
        );
        sampler.last_emitted_at = Some(path_std_time::Instant::now() - RESPONSE_UPDATE_INTERVAL);
        sampler.emit_if_due_from(
            &prompt.agent_prompt_id,
            &prompt,
            &FakeProgress {
                bytes: 7,
                message: "a雪",
                reasoning: "rλ",
            },
            &mut writer,
        );
    }
    let updates = decode_updates(&bytes);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].deltas.len(), 2);
    assert_eq!(
        updates[1].deltas,
        vec![
            tau_proto::ProviderResponseTextDelta::Message {
                output_index: 0,
                text: "雪".to_owned(),
                phase: None,
            },
            tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index: 1,
                kind: tau_proto::ReasoningTextKind::Full,
                text: "λ".to_owned(),
            },
        ]
    );
}

/// Standalone local summaries retain semantic parser state privately while
/// publishing the existing content-free progress sample.
#[test]
fn standalone_summary_progress_is_stats_only() {
    let mut prompt = crate::openai_tests::prompt();
    prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut sampler = ResponseSampler::new();
    sampler.latest_items = vec![
        tau_provider_chat_completions::AttemptOutputItem {
            output_index: 0,
            display_generation: Default::default(),
            item: tau_proto::ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::Assistant,
                content: vec![tau_proto::ContentPart::Text {
                    text: "private narrative".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }),
        },
        tau_provider_chat_completions::AttemptOutputItem {
            output_index: 1,
            display_generation: Default::default(),
            item: tau_proto::ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "private reasoning".to_owned(),
            }),
        },
    ];
    sampler.latest_bytes = 42;
    let mut bytes = Vec::new();
    let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
    sampler.emit_at(
        &prompt.agent_prompt_id,
        &prompt,
        &mut writer,
        path_std_time::Instant::now(),
        true,
    );

    let updates = decode_updates(&bytes);
    assert_eq!(updates.len(), 1);
    assert!(updates[0].deltas.is_empty());
    assert_eq!(
        updates[0]
            .response_stats
            .as_ref()
            .expect("content-free stats")
            .current
            .response_bytes_received,
        42
    );
}

/// Semantic timing is captured before cadence filtering and remains immutable
/// on every later sample, including terminal flush.
#[test]
fn first_semantic_output_timing_precedes_batching_and_repeats() {
    let prompt = crate::openai_tests::prompt();
    let start = path_std_time::Instant::now();
    let mut sampler = ResponseSampler::new();
    sampler.mark_dispatched(start);
    sampler.observe_progress(start + RESPONSE_UPDATE_INTERVAL / 2, true);
    sampler.observe_progress(start + RESPONSE_UPDATE_INTERVAL, true);
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            start + RESPONSE_UPDATE_INTERVAL,
            false,
        );
        sampler.emit_at(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            start + RESPONSE_UPDATE_INTERVAL * 2,
            true,
        );
    }
    let updates = decode_updates(&bytes);
    assert_eq!(updates.len(), 2);
    for update in updates {
        assert_eq!(
            update
                .response_stats
                .expect("stats")
                .first_semantic_output_elapsed_micros,
            Some(500_000)
        );
    }
}

fn decode_updates(bytes: &[u8]) -> Vec<tau_proto::ProviderResponseUpdated> {
    let mut decoder = tau_proto::HarnessInputReader::new(path_std_io::BufReader::new(bytes));
    let mut updates = Vec::new();
    while let Some(message) = decoder.read_message().expect("decode provider update") {
        if let tau_proto::HarnessInputMessage::Emit(emit) = message
            && let tau_proto::Event::ProviderResponseUpdatedReported(update) = *emit.event
        {
            updates.push(update);
        }
    }
    updates
}
