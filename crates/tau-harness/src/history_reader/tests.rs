//! Deterministic physical-admission and cancellation boundaries.

use std::sync::mpsc;
use std::time::Duration;

use tau_core::{AgentStore, SemanticPersistenceOwner};

use super::*;

/// A complete-written creation provides a real finite file capability.
fn prepared_prefix(root: &std::path::Path) -> (AgentStore, AgentHistoryPrefix) {
    let id = tau_proto::AgentId::parse("reader-test").expect("agent id");
    let owner = Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(root, owner.clone()).expect("store");
    store.reserve_new_agent(id.as_str()).expect("reserve");
    store
        .append_agent_event(
            id.as_str(),
            None,
            tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("creation");
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable,
    );
    let prefix = store
        .agent_history_prefix(&id)
        .expect("capture")
        .expect("readable");
    (store, prefix)
}

/// Completions still occupying the runtime channel count against all eight
/// permits; admission cannot turn a slow consumer into an unbounded backlog.
#[test]
fn history_reader_counts_unconsumed_completions() {
    let (tx, rx) = mpsc::channel();
    let reader = HistoryReader::start(tx).expect("worker");
    for id in 0..8 {
        reader.submit(id, Vec::new()).expect("admit");
    }
    assert!(reader.submit(9, Vec::new()).is_err());
    for id in 0..8 {
        let HarnessEvent::Command(HarnessCommand::HistoryReadCompleted(completed)) =
            rx.recv_timeout(Duration::from_secs(2)).expect("completion")
        else {
            panic!("unexpected runtime event");
        };
        assert_eq!(completed.id, id);
        assert!(matches!(completed.result, Ok(Some(ref histories)) if histories.is_empty()));
        assert_eq!(reader.outstanding.load(Ordering::Acquire), 8 - id as usize);
        drop(completed);
    }
    assert_eq!(reader.outstanding.load(Ordering::Acquire), 0);
    reader.submit(10, Vec::new()).expect("capacity restored");
}

/// Canceling an active or queued logical request cannot recycle its physical
/// permit. The single reader stops canceled queued work before another read.
#[test]
fn history_reader_cancellation_keeps_physical_permit() {
    let temp = tempfile::tempdir().expect("root");
    let (_store, prefix) = prepared_prefix(temp.path());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let reader = HistoryReader::start_with_reader(tx, move |prefix| {
        entered_tx.send(()).expect("signal active read");
        release_rx.recv().expect("release active read");
        prefix.prefetch()
    })
    .expect("worker");
    let active = reader.submit(0, vec![prefix.clone()]).expect("active");
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reader held");
    active.store(true, Ordering::Release);
    for id in 1..8 {
        reader
            .submit(id, vec![prefix.clone()])
            .expect("queue")
            .store(true, Ordering::Release);
    }
    assert!(reader.submit(8, vec![prefix]).is_err());
    assert_eq!(reader.outstanding.load(Ordering::Acquire), 8);
    release_tx.send(()).expect("release");
    for _ in 0..8 {
        let HarnessEvent::Command(HarnessCommand::HistoryReadCompleted(completed)) =
            rx.recv_timeout(Duration::from_secs(2)).expect("completion")
        else {
            panic!("unexpected runtime event");
        };
        assert!(matches!(completed.result, Ok(None)));
    }
    assert!(
        entered_rx.try_recv().is_err(),
        "canceled queue did not read"
    );
    assert_eq!(reader.outstanding.load(Ordering::Acquire), 0);
}
