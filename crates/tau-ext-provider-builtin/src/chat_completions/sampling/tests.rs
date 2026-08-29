//! Focused tests for response-sampler timing state.

use std::{io as path_std_io, time as path_std_time};

use super::{RESPONSE_UPDATE_INTERVAL, ResponseSampler};

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
