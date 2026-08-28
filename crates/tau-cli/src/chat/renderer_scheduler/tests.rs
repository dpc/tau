use tau_cli_term::RendererDeliveryId;
use tau_proto::{
    AgentId, AgentPromptId, PromptOriginator, ProviderResponseStatsSample,
    ProviderResponseTextDelta, UnixMicros,
};

use super::*;
use crate::chat::{
    RendererQueueFrame, begin_remote_memory_handler, cold_attach_stager,
    finish_remote_memory_handler,
};

/// Creates a scheduler and both production-shaped command senders.
fn scheduler() -> (
    Arc<AtomicU64>,
    RemoteRendererSender,
    LocalRendererSender,
    RendererCommandScheduler,
) {
    scheduler_with_memory(None)
}

/// Creates production-shaped command senders with an enabled ownership tracker.
fn scheduler_with_memory(
    delivery_memory: Option<Arc<DeliveryMemoryTracker>>,
) -> (
    Arc<AtomicU64>,
    RemoteRendererSender,
    LocalRendererSender,
    RendererCommandScheduler,
) {
    let admitted = Arc::new(AtomicU64::new(0));
    let arbiter = Arc::new(Mutex::new(()));
    let (wake_tx, wake_rx) = tau_blocking_notify_channel::channel();
    let (remote_tx, remote_rx) = RemoteRendererSender::channel(16, wake_tx.clone());
    let (local_tx, local_rx) =
        LocalRendererSender::channel(admitted.clone(), arbiter.clone(), wake_tx);
    let receiver = RendererCommandScheduler::new(
        remote_rx,
        local_rx,
        admitted.clone(),
        arbiter,
        wake_rx,
        delivery_memory,
    );
    (admitted, remote_tx, local_tx, receiver)
}

/// Builds one pure provider update with independently selectable delta and
/// stats.
fn update(
    delivery_id: u64,
    prompt: &str,
    text: &str,
    stats: Option<ProviderResponseStats>,
) -> RendererCmd {
    RendererCmd::Remote {
        event: Box::new(Event::ProviderResponseUpdated(
            tau_proto::ProviderResponseUpdated {
                agent_prompt_id: AgentPromptId::parse(prompt).expect("prompt id"),
                agent_id: AgentId::parse("main").expect("agent id"),
                deltas: vec![ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: text.to_owned(),
                    phase: None,
                }],
                compaction: None,
                status: None,
                response_stats: stats,
                originator: PromptOriginator::User,
            },
        )),
        presentation: cold_attach_stager::RendererPresentation::Ordinary,
        abandoned_shell_starts: Vec::new(),
        recorded_at: UnixMicros::new(delivery_id),
        delivery_id: RendererDeliveryId::new(delivery_id),
        queue_bytes: delivery_id as usize,
        enqueued_at: Instant::now(),
        folded_frames: Vec::new(),
    }
}

/// Builds a response stats sample whose endpoints make span direction explicit.
fn stats(previous: u64, current: u64, first_semantic: u64) -> ProviderResponseStats {
    ProviderResponseStats {
        previous: ProviderResponseStatsSample {
            response_bytes_received: previous,
            elapsed_micros: previous * 10,
        },
        current: ProviderResponseStatsSample {
            response_bytes_received: current,
            elapsed_micros: current * 10,
        },
        first_semantic_output_elapsed_micros: Some(first_semantic),
    }
}

/// Adjacent admitted updates fold once while preserving delta order, stats
/// endpoints, immutable first-semantic duration, and every accounting receipt.
#[test]
fn folds_adjacent_pure_updates_in_exact_order() {
    let memory = Arc::new(DeliveryMemoryTracker::new());
    memory.force_enable_for_test();
    let encoded = tau_proto::ProtocolMessageBytes::new(1).expect("encoded byte");
    for id in 1..=3 {
        memory.observe_decode(
            RendererDeliveryId::new(id),
            &tau_proto::HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {})),
            encoded,
        );
        memory.transition(RendererDeliveryId::new(id), DeliveryMemoryCut::RendererFifo);
    }
    let (admitted, remote_tx, _local_tx, mut receiver) =
        scheduler_with_memory(Some(Arc::clone(&memory)));
    admitted.store(3, Ordering::Release);
    remote_tx
        .send(update(
            1,
            "prompt-1",
            "a",
            Some(ProviderResponseStats {
                first_semantic_output_elapsed_micros: None,
                ..stats(0, 1, 17)
            }),
        ))
        .expect("first update");
    remote_tx
        .send(update(2, "prompt-1", "b", Some(stats(1, 2, 99))))
        .expect("second update");
    remote_tx
        .send(update(3, "prompt-1", "c", Some(stats(2, 3, 99))))
        .expect("third update");

    let RendererCmd::Remote {
        event,
        folded_frames,
        ..
    } = receiver
        .recv_timeout(Duration::ZERO)
        .expect("folded update")
    else {
        panic!("expected remote update");
    };
    let Event::ProviderResponseUpdated(update) = event.as_ref() else {
        panic!("expected provider update");
    };
    let text: String = update
        .deltas
        .iter()
        .map(|delta| match delta {
            ProviderResponseTextDelta::Message { text, .. } => text.as_str(),
            _ => panic!("expected message delta"),
        })
        .collect();
    assert_eq!(text, "abc");
    assert_eq!(update.response_stats, Some(stats(0, 3, 99)));
    assert_eq!(folded_frames.len(), 2);
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(1)),
        Some(DeliveryMemoryCut::Scheduler)
    );
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(2)),
        Some(DeliveryMemoryCut::Scheduler)
    );
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(3)),
        Some(DeliveryMemoryCut::Scheduler)
    );
    assert_eq!(
        folded_frames
            .iter()
            .map(|RendererQueueFrame { delivery_id, .. }| delivery_id.get())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        folded_frames
            .iter()
            .map(|frame| frame.queue_bytes)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let receipts = begin_remote_memory_handler(
        Some(memory.as_ref()),
        RendererDeliveryId::new(1),
        &folded_frames,
    );
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(1)),
        Some(DeliveryMemoryCut::Handler)
    );
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(2)),
        Some(DeliveryMemoryCut::Handler)
    );
    assert_eq!(
        memory.cut_for_test(RendererDeliveryId::new(3)),
        Some(DeliveryMemoryCut::Handler)
    );
    finish_remote_memory_handler(Some(memory.as_ref()), RendererDeliveryId::new(1), receipts);
    assert_eq!(memory.active_len_for_test(), 0);
}

/// A captured local watermark is a hard barrier: a later admitted update cannot
/// be sampled before the local command even when both are already queued.
#[test]
fn local_watermark_splits_adjacent_updates() {
    let (admitted, remote_tx, local_tx, mut receiver) = scheduler();
    admitted.store(1, Ordering::Release);
    remote_tx
        .send(update(1, "prompt-1", "a", None))
        .expect("first update");
    local_tx
        .send(RendererCmd::ShowSessionStats)
        .expect("watermarked local command");
    admitted.store(2, Ordering::Release);
    remote_tx
        .send(update(2, "prompt-1", "b", None))
        .expect("second update");

    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::Remote {
            folded_frames,
            ..
        }) if folded_frames.is_empty()
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::ShowSessionStats)
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::Remote { .. })
    ));
}

/// Folding only probes the queue nonblockingly; a reserved but not-yet-enqueued
/// suffix cannot delay the first visible update.
#[test]
fn folding_never_waits_for_a_later_arrival() {
    let (admitted, remote_tx, _local_tx, mut receiver) = scheduler();
    admitted.store(2, Ordering::Release);
    remote_tx
        .send(update(1, "prompt-1", "visible", None))
        .expect("first update");

    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::Remote {
            folded_frames,
            ..
        }) if folded_frames.is_empty()
    ));
}

/// A different prompt is retained as FIFO lookahead rather than consumed,
/// reordered, or accidentally joined to the first projection.
#[test]
fn different_prompt_is_a_fifo_barrier() {
    let (admitted, remote_tx, _local_tx, mut receiver) = scheduler();
    admitted.store(2, Ordering::Release);
    remote_tx
        .send(update(1, "prompt-1", "a", None))
        .expect("first update");
    remote_tx
        .send(update(2, "prompt-2", "b", None))
        .expect("barrier update");

    for expected_prompt in ["prompt-1", "prompt-2"] {
        let RendererCmd::Remote { event, .. } = receiver
            .recv_timeout(Duration::ZERO)
            .expect("ordered update")
        else {
            panic!("expected remote update");
        };
        let Event::ProviderResponseUpdated(update) = event.as_ref() else {
            panic!("expected provider update");
        };
        assert_eq!(update.agent_prompt_id.as_str(), expected_prompt);
    }
}

/// Status clears and compaction transitions remain individual projections and
/// split otherwise matching pure updates on both sides.
#[test]
fn status_and_compaction_are_hard_barriers() {
    let (admitted, remote_tx, _local_tx, mut receiver) = scheduler();
    let mut status = update(2, "prompt-1", "", None);
    let RendererCmd::Remote { event, .. } = &mut status else {
        unreachable!("update helper returns remote command");
    };
    let Event::ProviderResponseUpdated(status_update) = event.as_mut() else {
        unreachable!("update helper returns provider update");
    };
    status_update.deltas.clear();
    status_update.status = Some(tau_proto::ProviderResponseStatusUpdate {
        text: "retrying".to_owned(),
        clear_response: true,
        retry: None,
    });

    let mut compaction = update(4, "prompt-1", "", None);
    let RendererCmd::Remote { event, .. } = &mut compaction else {
        unreachable!("update helper returns remote command");
    };
    let Event::ProviderResponseUpdated(compaction_update) = event.as_mut() else {
        unreachable!("update helper returns provider update");
    };
    compaction_update.deltas.clear();
    compaction_update.compaction = Some(tau_proto::ProviderResponseCompactionUpdate {
        status: tau_proto::ProviderResponseCompactionStatus::Started,
        original_input_tokens: None,
        compaction_output_tokens: None,
    });

    admitted.store(5, Ordering::Release);
    for command in [
        update(1, "prompt-1", "a", None),
        status,
        update(3, "prompt-1", "b", None),
        compaction,
        update(5, "prompt-1", "c", None),
    ] {
        remote_tx.send(command).expect("barrier sequence");
    }

    for _ in 0..5 {
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            Ok(RendererCmd::Remote {
                folded_frames,
                ..
            }) if folded_frames.is_empty()
        ));
    }
}

/// Agent and originator mismatches split a run even when prompt IDs and event
/// shapes otherwise match.
#[test]
fn agent_and_originator_mismatches_split_the_run() {
    let (admitted, remote_tx, _local_tx, mut receiver) = scheduler();
    let mut other_agent = update(2, "prompt-1", "b", None);
    let RendererCmd::Remote { event, .. } = &mut other_agent else {
        unreachable!("update helper returns remote command");
    };
    let Event::ProviderResponseUpdated(other_agent_update) = event.as_mut() else {
        unreachable!("update helper returns provider update");
    };
    other_agent_update.agent_id = AgentId::parse("worker").expect("agent id");

    let mut other_originator = update(3, "prompt-1", "c", None);
    let RendererCmd::Remote { event, .. } = &mut other_originator else {
        unreachable!("update helper returns remote command");
    };
    let Event::ProviderResponseUpdated(other_originator_update) = event.as_mut() else {
        unreachable!("update helper returns provider update");
    };
    other_originator_update.originator = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("origin").expect("extension name"),
        query_id: "q1".to_owned(),
    };

    admitted.store(3, Ordering::Release);
    for command in [
        update(1, "prompt-1", "a", None),
        other_agent,
        other_originator,
    ] {
        remote_tx.send(command).expect("mismatch sequence");
    }
    for _ in 0..3 {
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            Ok(RendererCmd::Remote {
                folded_frames,
                ..
            }) if folded_frames.is_empty()
        ));
    }
}

/// A canceled prompt terminal and the terminal FIFO disconnect command are
/// retained as barriers; neither can be consumed into an adjacent update.
#[test]
fn other_event_and_disconnect_are_fifo_barriers() {
    let (admitted, remote_tx, _local_tx, mut receiver) = scheduler();
    admitted.store(3, Ordering::Release);
    remote_tx
        .send(update(1, "prompt-1", "a", None))
        .expect("provider update");
    remote_tx
        .send(RendererCmd::Remote {
            event: Box::new(Event::AgentPromptTerminated(
                tau_proto::AgentPromptTerminated {
                    agent_id: AgentId::parse("main").expect("agent id"),
                    agent_prompt_id: AgentPromptId::parse("prompt-1").expect("prompt id"),
                    reason: tau_proto::AgentPromptTerminationReason::Canceled,
                    originator: PromptOriginator::User,
                    automatic_compaction_decision: None,
                },
            )),
            presentation: cold_attach_stager::RendererPresentation::Ordinary,
            abandoned_shell_starts: Vec::new(),
            recorded_at: UnixMicros::new(2),
            delivery_id: RendererDeliveryId::new(2),
            queue_bytes: 2,
            enqueued_at: Instant::now(),
            folded_frames: Vec::new(),
        })
        .expect("other event");
    remote_tx
        .send(RendererCmd::RemoteDisconnect {
            reason: Some("done".to_owned()),
            delivery_id: RendererDeliveryId::new(3),
            queue_bytes: 3,
            enqueued_at: Instant::now(),
        })
        .expect("disconnect");

    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::Remote {
            event,
            folded_frames,
            ..
        }) if matches!(event.as_ref(), Event::ProviderResponseUpdated(_))
            && folded_frames.is_empty()
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::Remote {
            event,
            folded_frames,
            ..
        }) if matches!(event.as_ref(), Event::AgentPromptTerminated(_))
            && folded_frames.is_empty()
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::ZERO),
        Ok(RendererCmd::RemoteDisconnect { .. })
    ));
}
