use std::cell::Cell;
use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::time::Instant;
use std::{sync as path_std_sync, time as path_std_time};

use super::*;
use crate::journal_sync::SyncTargetKind;
use crate::record_log::AppendFault;

fn managed_charge_projection(event_count: usize) -> ManagedAgentProjection {
    let agent_id = AgentId::parse("charge-benchmark-agent").expect("agent id");
    let events: Vec<_> = (0..event_count)
        .map(|index| PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([index as u8; 16]),
            seq: PersistedAgentEventSeq::new(index as u64),
            source: None,
            event: Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: format!("record-{index}"),
                inference_activation: false,
                message_class: Default::default(),
            }),
            parent: AgentEventParent::InheritHead,
            fold_semantics: AgentJournalFoldSemantics::Legacy,
            recorded_at: UnixMicros::new(index as u64),
        })
        .collect();
    let tree = AgentTree::try_from_events(agent_id, &events).expect("valid benchmark records");
    let mut summary = AgentSummary::default();
    for event in &events {
        summary.apply(event);
    }
    ManagedAgentProjection::from_replay(tree, summary, events)
}

/// The replay-derived aggregate produces the same saturating admission charge
/// as an explicit record scan, including arithmetic overflow.
#[test]
fn managed_projection_cached_charge_matches_full_measurement() {
    for event_count in [0, 1, 17, 257] {
        let projection = managed_charge_projection(event_count);
        let new_record_bytes = usize::MAX / 8;
        let explicit = managed_agent_encoded_event_bytes(&projection.events)
            .saturating_add(new_record_bytes)
            .saturating_mul(4)
            .saturating_add(std::mem::size_of::<ManagedAgentProjection>());
        assert_eq!(
            managed_agent_projection_charge(&projection, new_record_bytes),
            explicit,
            "event_count={event_count}"
        );
    }
}

/// Manual asymptotic benchmark reports cached-charge latency across histories
/// from empty to large; it deliberately compares scaling instead of enforcing
/// a flaky wall-clock threshold.
#[test]
#[ignore = "manual managed-charge asymptotic benchmark"]
fn benchmark_managed_projection_cached_charge_scaling() {
    const SAMPLES: usize = 1_000_000;
    for event_count in [0, 16, 1_024, 65_536] {
        let projection = managed_charge_projection(event_count);
        let started = Instant::now();
        let mut checksum = 0;
        for sample in 0..SAMPLES {
            checksum ^= std::hint::black_box(managed_agent_projection_charge(
                std::hint::black_box(&projection),
                sample,
            ));
        }
        eprintln!(
            "managed_charge events={event_count} samples={SAMPLES} elapsed={:?} checksum={checksum}",
            started.elapsed()
        );
    }
}

/// Normal-build inspection state rejects lock/repair mutation without
/// artifacts.
#[test]
fn read_only_agent_store_rejects_recovery_without_mutation() {
    let temp = tempfile::tempdir().expect("temporary root");
    let root = temp.path().join("agents");
    let mut store = AgentStore::read_only(&root);
    let error = store
        .lock_and_recover_agent("missing-agent")
        .expect_err("read-only recovery rejects");
    assert!(error.to_string().contains("unavailable"));
    assert!(!root.exists());
}

fn facts_budget(max_record_bytes: u64, remaining_bytes: u64) -> AgentCreationFactsBudget {
    AgentCreationFactsBudget {
        max_record_bytes,
        remaining_bytes,
    }
}

/// An invalid parent wins over a record-only V1 owner overlap, and each
/// rejected append leaves the journal and cached tree untouched while replay
/// still rejects the corrupted overlap.
#[test]
fn overlapping_v1_owner_append_rejects_before_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("v1-overlap").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("durable store");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                inference_activation: true,
                message_class: Default::default(),
            }),
        )
        .expect("seed head");
    let checkpoint = |prompt: &str| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: tau_proto::AgentPromptId::parse(prompt).expect("prompt id"),
            through: tau_proto::AgentHead::Node(NodeId::new(0)),
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
            output_length_continuation: None,
        })
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-first"),
            UnixMicros::new(1),
        )
        .expect("first owner");
    let before = store.agent_events(agent_id.as_str()).expect("events");
    let before_tree = store.agent(agent_id.as_str()).expect("tree").clone();
    let journal = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_bytes = fs::read(&journal).expect("journal bytes");
    let parent_error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(99)),
            checkpoint("ap-invalid-parent"),
            UnixMicros::new(2),
        )
        .expect_err("unknown parent must win over owner overlap");
    assert!(
        parent_error
            .to_string()
            .contains("parent referenced unknown node_id"),
        "event validation must retain precedence over record-only validation"
    );
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    assert_eq!(store.agent(agent_id.as_str()).expect("tree"), &before_tree);
    assert_eq!(fs::read(&journal).expect("journal bytes"), before_bytes);
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-second"),
            UnixMicros::new(2),
        )
        .expect_err("overlap must reject");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    assert_eq!(store.agent(agent_id.as_str()).expect("tree"), &before_tree);
    assert_eq!(fs::read(&journal).expect("journal bytes"), before_bytes);
    drop(store);
    let mut reopened = AgentStore::open_lazy(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("clean reopen");
    assert_eq!(
        reopened.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    let mut overlapping_replay = before.clone();
    overlapping_replay.push(PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([9; 16]),
        seq: PersistedAgentEventSeq::new(2),
        source: None,
        event: checkpoint("ap-replay-overlap"),
        parent: AgentEventParent::Under(NodeId::new(0)),
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: UnixMicros::new(3),
    });
    assert!(
        AgentTree::try_from_events(agent_id, &overlapping_replay).is_err(),
        "cold replay must reject overlapping marked owners"
    );
}

/// A store-wide memory-only policy keeps unexpected agent activity replayable
/// without consulting or creating its supplied durable root.
#[test]
fn memory_only_store_never_touches_durable_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("state/agents");
    let seeded_agent_dir = agents_dir.join("seeded-agent");
    fs::create_dir_all(&seeded_agent_dir).expect("seed durable agent root");
    let seeded_journal = seeded_agent_dir.join("events.cbor");
    fs::write(&seeded_journal, b"seeded durable bytes").expect("seed durable journal");
    let mut store = AgentStore::open_memory_only(&agents_dir);
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    assert!(!store.agent_id_is_reserved(agent_id.as_str()));
    assert!(!store.agent_id_is_reserved("seeded-agent"));
    assert!(
        store
            .lock_and_recover_agent("seeded-agent")
            .expect("memory-only recovery is process-local")
            .is_none()
    );

    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends in memory");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "preview"),
        )
        .expect("projection appends in memory");
    store
        .record_agent_meta(agent_id.as_str())
        .expect("metadata remains in memory");

    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("memory replay")
            .len(),
        2
    );
    assert_eq!(
        store.agent(&agent_id).and_then(AgentTree::display_name),
        Some("preview")
    );
    assert_eq!(
        fs::read(&seeded_journal).expect("seed remains readable"),
        b"seeded durable bytes"
    );
    assert!(!seeded_agent_dir.join("lock").exists());
    assert!(!agents_dir.join(agent_id.as_str()).exists());
}

/// Permanent retired-ID tombstones reserve the namespace in discovery and at
/// the durable reservation boundary.
#[test]
fn retired_agent_id_cannot_be_reserved_again() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_id = AgentId::parse("retired-agent").expect("agent id");
    let tombstone = super::retired_agent_tombstone(&agents_dir, &agent_id);
    fs::create_dir_all(tombstone.parent().expect("retired root")).expect("retired root");
    fs::write(&tombstone, []).expect("tombstone");
    let owner = path_std_sync::Arc::new(
        crate::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store = AgentStore::open_managed(&agents_dir, owner).expect("managed store");

    assert!(store.agent_id_is_reserved(agent_id.as_str()));
    assert!(matches!(
        store.reserve_new_agent(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
    assert!(!agents_dir.join(agent_id.as_str()).exists());
}

/// A dangling tombstone symlink still reserves the stable ID for both durable
/// and ephemeral creation paths.
#[cfg(unix)]
#[test]
fn dangling_retired_tombstone_symlink_reserves_agent_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_id = AgentId::parse("dangling-retired").expect("agent id");
    let tombstone = super::retired_agent_tombstone(&agents_dir, &agent_id);
    fs::create_dir_all(tombstone.parent().expect("retired root")).expect("retired root");
    unix_fs::symlink(temp.path().join("missing-target"), &tombstone).expect("dangling tombstone");
    let owner = path_std_sync::Arc::new(
        crate::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store = AgentStore::open_managed(&agents_dir, owner).expect("managed store");

    assert!(store.agent_id_is_reserved(agent_id.as_str()));
    assert!(matches!(
        store.reserve_new_agent(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
    assert!(matches!(
        store.mark_agent_ephemeral(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
}

/// Prompt previews retain the legacy normalized text for empty, boundary, and
/// Unicode inputs while avoiding a full scan after the retained prefix.
#[test]
fn preview_text_matches_legacy_normalization_for_varied_unicode() {
    fn reference_preview_text(text: &str, max: usize) -> String {
        let single_line: String = text
            .chars()
            .map(|character| if character == '\n' { ' ' } else { character })
            .collect();
        if single_line.chars().count() < max + 1 {
            single_line
        } else {
            format!("{}…", single_line.chars().take(max).collect::<String>())
        }
    }

    let arbitrary_unicode: String = (0..=char::MAX as u32)
        .step_by(7_919)
        .filter_map(char::from_u32)
        .collect();
    let inputs = [
        String::new(),
        "short prompt".to_owned(),
        "x".repeat(48),
        "x".repeat(49),
        "first\nsecond\r\nthird".to_owned(),
        "e\u{301}".repeat(24),
        arbitrary_unicode,
    ];
    for max in [0, 1, 47, 48, 49] {
        for text in &inputs {
            assert_eq!(
                preview_text(text, max),
                reference_preview_text(text, max),
                "max={max}, input={text:?}"
            );
        }
    }
}

/// A multi-megabyte prompt must inspect only the retained prefix plus the
/// scalar that decides whether the preview needs an ellipsis.
#[test]
fn preview_text_stops_after_retained_prefix() {
    const MAX: usize = 48;
    let text = "雪\n".repeat(2 * 1024 * 1024);
    let mut inspected = 0;
    let preview = preview_text_from_chars(
        text.chars().inspect(|_| {
            inspected += 1;
        }),
        MAX,
    );

    assert_eq!(inspected, MAX + 1);
    assert_eq!(preview, format!("{}…", "雪 ".repeat(MAX / 2)));
}

/// A live ephemeral append and a same-daemon refold of its retained events
/// preserve the identical newline-normalized Unicode prompt preview.
#[test]
fn ephemeral_prompt_preview_matches_live_and_refolded_metadata() {
    let agent_id = AgentId::parse("preview-agent").expect("agent id");
    let event = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id.clone(),
        text: format!("{}\n{}", "雪".repeat(47), "ignored"),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    });
    let mut store = AgentStore::open_memory_only("/unused/ephemeral-agents");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("ephemeral creation");
    store
        .append_agent_event(agent_id.as_str(), None, event)
        .expect("ephemeral prompt");
    let live = store
        .agent_meta(agent_id.as_str())
        .expect("ephemeral metadata")
        .expect("ephemeral metadata exists");
    let mut replayed = AgentMeta::default();

    for record in store
        .agent_events(agent_id.as_str())
        .expect("retained events")
    {
        touch_ephemeral_meta_for_event(
            &mut replayed,
            &record.event,
            record.recorded_at.get() / 1_000_000,
        );
    }

    assert_eq!(
        live.latest_user_prompt_preview,
        replayed.latest_user_prompt_preview
    );
    let expected = format!("{} …", "雪".repeat(47));
    assert_eq!(
        live.latest_user_prompt_preview.as_deref(),
        Some(expected.as_str())
    );
}

/// Manual work and output benchmark documents that preview storage remains
/// bounded by 48 retained scalars rather than by the source prompt size.
#[test]
#[ignore = "manual prompt preview work/output benchmark"]
fn benchmark_preview_text_work_and_output() {
    const MAX: usize = 48;
    println!("input_bytes,inspected_scalars,output_bytes,output_capacity");
    for input_bytes in [48, 1024 * 1024, 8 * 1024 * 1024] {
        let text = "x".repeat(input_bytes);
        let mut inspected = 0;
        let preview = preview_text_from_chars(
            text.chars().inspect(|_| {
                inspected += 1;
            }),
            MAX,
        );
        assert!(inspected <= MAX + 1);
        assert!(preview.chars().count() <= MAX + 1);
        println!(
            "{input_bytes},{inspected},{},{}",
            preview.len(),
            preview.capacity()
        );
    }
}

/// Returns the CBOR payload size of the representative oversized agent record.
fn encoded_record_length(text_len: usize, seq: u64) -> usize {
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let event = injected_message_event(&agent_id, "x".repeat(text_len));
    let fold_semantics = AgentJournalFoldSemantics::for_new_event(&event);
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([7; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics,
        recorded_at: UnixMicros::new(42),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&record, &mut encoded).expect("test record encodes");
    encoded.len()
}

/// Builds a message injection whose text can exercise the encoded record bound.
fn injected_message_event(agent_id: &AgentId, text: String) -> Event {
    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        agent_id: agent_id.clone(),
        text,
        inference_activation: false,
        message_class: Default::default(),
    })
}

/// Produces a message body whose agent record has the requested encoded size.
fn body_for_encoded_length(encoded_length: usize, seq: u64) -> String {
    const PROBE_BODY_BYTES: usize = 1024 * 1024;
    let overhead = encoded_record_length(PROBE_BODY_BYTES, seq) - PROBE_BODY_BYTES;
    let body = "x".repeat(encoded_length - overhead);
    assert_eq!(
        encoded_record_length(body.len(), seq),
        encoded_length,
        "large-record CBOR overhead should remain stable"
    );
    body
}

/// An oversized durable append leaves the journal, cached fold, sequence, and
/// checkpoint unchanged, then permits a retry at the rejected sequence.
#[test]
fn oversized_agent_append_is_atomic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let checkpoint_before = fs::read(&checkpoint_path).expect("baseline checkpoint");
    let events_before = store
        .agent_events(agent_id.as_str())
        .expect("baseline events");
    let tree_before = store
        .agent(agent_id.as_str())
        .expect("baseline tree")
        .clone();
    let oversized_body = body_for_encoded_length((MAX_RECORD_BYTES + 1) as usize, 1);

    let error = store
        .append_agent_event_at_with_observation_id(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            injected_message_event(&agent_id, oversized_body),
            UnixMicros::new(42),
            tau_proto::ObservationId::from_bytes([7; 16]),
        )
        .expect_err("oversized record must fail");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
    assert_eq!(
        fs::read(&journal_path).expect("journal remains"),
        journal_before
    );
    assert_eq!(
        fs::read(&checkpoint_path).expect("checkpoint remains"),
        checkpoint_before
    );
    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("events remain"),
        events_before
    );
    let tree = store.agent(agent_id.as_str()).expect("tree remains");
    assert_eq!(tree, &tree_before);
    assert_eq!(tree.head(), tree_before.head());
    assert_eq!(tree.next_event_seq(), tree_before.next_event_seq());

    let retry = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "retry"),
            UnixMicros::new(43),
        )
        .expect("later bounded record appends");
    assert_eq!(retry.seq, PersistedAgentEventSeq::new(1));
    drop(store);

    let mut reopened = AgentStore::open_lazy(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("replay succeeds");
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events"),
        vec![
            events_before[0].clone(),
            PersistedAgentEvent {
                observation_id: retry.observation_id,
                seq: retry.seq,
                source: None,
                event: display_name_event(&agent_id, "retry"),
                parent: AgentEventParent::InheritHead,
                fold_semantics: AgentJournalFoldSemantics::Legacy,
                recorded_at: UnixMicros::new(43),
            },
        ]
    );
}

/// A later semantic append and fold complete while prior journal sync remains
/// blocked in the background.
#[test]
fn semantic_append_continues_while_sync_is_blocked() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let sync = store
        .legacy_io
        .as_mut()
        .expect("legacy writer")
        .framed_appends
        .inject_blocking_sync();
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends");
    assert!(sync.wait_until_blocked(), "worker did not block");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let continued_id = agent_id.clone();
    let continuation = std::thread::spawn(move || {
        let result = store.append_agent_event(
            continued_id.as_str(),
            None,
            display_name_event(&continued_id, "continued"),
        );
        tx.send((store, result)).expect("send continuation");
    });
    let received = rx.recv_timeout(path_std_time::Duration::from_secs(2));
    sync.release();
    let (store, result) = received.expect("later semantic append blocked on sync");
    result.expect("later semantic append completes");
    continuation.join().expect("continuation thread");
    assert_eq!(
        store.agent(&agent_id).and_then(AgentTree::display_name),
        Some("continued")
    );
}

/// A first durable append creates and locks its missing branch, while later
/// appends reuse that retained ownership and preserve the journal sequence.
#[test]
fn durable_repeated_append_reuses_first_append_branch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let agent_dir = temp.path().join(agent_id.as_str());
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");

    assert!(!agent_dir.exists(), "first append must create the branch");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("first append creates and locks branch");
    assert!(
        agent_dir.join("lock").is_file(),
        "first append creates lock"
    );
    assert!(
        agent_dir.join("events.cbor").is_file(),
        "first append creates journal"
    );

    let second = store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "warm"),
        )
        .expect("warm append reuses retained branch lock");
    assert_eq!(second.seq, PersistedAgentEventSeq::new(1));

    drop(store);
    let mut reopened = AgentStore::open_lazy(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("replay journal");
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events")
            .len(),
        2
    );
}

/// A later writable lifetime re-covers both an existing store root and its
/// locked branch as independent typed boundary targets.
#[test]
fn writable_reopen_recovers_store_root_and_branch_boundaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("state/agents");
    {
        let created = missing_directories(&agents_dir);
        fs::create_dir_all(&agents_dir).expect("first lifetime creates root");
        let mut first = FramedAppendState::default();
        first.inject_sync_spawn_failure();
        first.note_created_directories(created);
        assert!(first.dirty_target(&agents_dir).is_some());
    }
    let mut store = AgentStore::open_lazy(&agents_dir).expect("second store opens");
    store
        .legacy_io
        .as_mut()
        .expect("legacy writer")
        .framed_appends
        .inject_sync_spawn_failure();
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("writable append");

    let root_target = store
        .legacy_io
        .as_ref()
        .expect("legacy writer")
        .framed_appends
        .dirty_target(&agents_dir)
        .expect("store-root target");
    assert!(root_target.directories.contains(&temp.path().join("state")));
    assert!(root_target.directories.contains(&temp.path().to_path_buf()));
    assert_eq!(root_target.kind, SyncTargetKind::DirectoryBoundary);
    let branch = agents_dir.join(agent_id.as_str());
    let branch_target = store
        .legacy_io
        .as_ref()
        .expect("legacy writer")
        .framed_appends
        .dirty_target(&branch)
        .expect("branch target");
    assert_eq!(
        branch_target.directories,
        [agents_dir].into_iter().collect()
    );
    assert_eq!(branch_target.kind, SyncTargetKind::DirectoryBoundary);
}

/// The roster enrichment path reads immutable creation fields and the latest
/// in-memory display projection without scanning transcript history.
#[test]
fn creation_facts_read_valid_first_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: Some(AgentId::parse("parent").expect("parent id")),
                role: "engineer".to_owned(),
                display_name: Some("Initial".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "Latest".to_owned(),
            }),
            UnixMicros::new(43),
        )
        .expect("name appends");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("within budget");

    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            started_at: Some(started_at),
            role,
            display_name: Some(display_name),
            ..
        } if started_at == UnixMicros::new(42)
            && role == "engineer"
            && display_name == "Latest"
    ));
}

/// Cold roster enrichment accepts a journal-bound checkpoint but suppresses a
/// structurally valid checkpoint whose boundary witness no longer matches.
#[test]
fn creation_facts_require_journal_bound_cold_checkpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    agent_id: agent_id.clone(),
                    parent_agent: None,
                    role: "engineer".to_owned(),
                    display_name: Some("Cold".to_owned()),
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
                UnixMicros::new(42),
            )
            .expect("creation appends");
    }

    let store = AgentStore::open_lazy(temp.path()).expect("cold store opens");
    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("fresh checkpoint");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: Some(display_name),
            ..
        } if display_name == "Cold"
    ));

    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("invalid checkpoint stays categorical");
    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            display_name: None,
            ..
        }
    ));
}

/// Missing journals remain categorical rows rather than becoming I/O errors.
#[test]
fn creation_facts_report_missing_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("missing").expect("agent id");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("missing does not consume budget");

    assert_eq!(facts, AgentCreationFacts::Missing);
    assert_eq!(facts.bytes_read(), 0);
}

/// Aggregate roster enrichment fails before allocating a first record that does
/// not fit the caller's remaining budget.
#[test]
fn creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    let result = store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1));

    assert_eq!(result, Err(AgentCreationFactsBudgetExceeded));
}

/// In-memory ephemeral creation projections consume the same aggregate budget
/// even though no journal read is needed.
#[test]
fn ephemeral_creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("ephemeral").expect("agent id");
    store
        .mark_agent_ephemeral(agent_id.as_str())
        .expect("mark ephemeral");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: true,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    assert_eq!(
        store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1)),
        Err(AgentCreationFactsBudgetExceeded)
    );
}

/// Truncated durable records charge their advertised bounded length so repeated
/// malformed members cannot bypass the aggregate roster budget.
#[test]
fn truncated_creation_records_consume_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let record_length = 256 * 1024_u64;
    let mut remaining = 4 * 1024 * 1024_u64;
    for index in 0..17 {
        let agent_id = AgentId::parse(format!("truncated-{index}")).expect("agent id");
        let dir = store.agent_dir(agent_id.as_str());
        fs::create_dir_all(&dir).expect("agent dir");
        let mut bytes = record_length.to_le_bytes().to_vec();
        bytes.resize(8 + record_length as usize - 1, 0);
        fs::write(dir.join("events.cbor"), bytes).expect("truncated journal");

        let facts = store.agent_creation_facts(&agent_id, facts_budget(record_length, remaining));
        if index < 16 {
            let facts = facts.expect("record fits remaining budget");
            assert_eq!(
                facts,
                AgentCreationFacts::Unreadable {
                    bytes_read: record_length
                }
            );
            remaining = remaining.saturating_sub(facts.bytes_read());
        } else {
            assert_eq!(facts, Err(AgentCreationFactsBudgetExceeded));
        }
    }
}

/// A durable agent append failure rolls back the frame, leaves its checkpoint
/// and folded sequence unchanged, and reuses that sequence on retry.
#[test]
fn failed_frame_append_is_atomic_and_retry_reuses_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let checkpoint_before = fs::read(&checkpoint_path).expect("baseline checkpoint");
    store
        .legacy_io
        .as_mut()
        .expect("legacy writer")
        .framed_appends
        .inject_fault(
            &journal_path,
            AppendFault {
                fail_write_at: Some(3),
                ..AppendFault::default()
            },
        );

    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");

    assert!(matches!(error, AgentStoreError::Write { .. }));
    assert_eq!(fs::read(&journal_path).expect("journal"), journal_before);
    assert_eq!(
        fs::read(&checkpoint_path).expect("checkpoint"),
        checkpoint_before
    );
    assert_eq!(
        store
            .load_agent(agent_id.as_str())
            .expect("agent remains loadable")
            .expect("agent remains loaded")
            .display_name(),
        None
    );
    let retry = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "retry"),
            UnixMicros::new(43),
        )
        .expect("retry appends");
    assert_eq!(retry.seq, PersistedAgentEventSeq::new(1));
    assert_eq!(
        store
            .load_agent(agent_id.as_str())
            .expect("agent remains loadable")
            .expect("agent remains loaded")
            .display_name(),
        Some("retry")
    );
    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("valid journal")
            .len(),
        2
    );
}

/// An uncertain agent-journal rollback poisons only that live stream and later
/// appends reject it without changing its bytes.
#[test]
fn rollback_failure_poisons_agent_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    store
        .legacy_io
        .as_mut()
        .expect("legacy writer")
        .framed_appends
        .inject_fault(
            &journal_path,
            AppendFault {
                fail_write_at: Some(3),
                fail_truncate: true,
                ..AppendFault::default()
            },
        );
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append fails");
    let bytes_after_failure = fs::read(&journal_path).expect("failed journal");

    let poisoned = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "rejected"),
            UnixMicros::new(43),
        )
        .expect_err("poisoned journal rejects append");
    let other_agent_id = AgentId::parse("agent-2").expect("agent id");
    let other = store
        .append_agent_event_at(
            other_agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&other_agent_id),
            UnixMicros::new(44),
        )
        .expect("other journal remains writable");

    assert!(
        poisoned
            .to_string()
            .contains("append disabled after an incomplete rollback")
    );
    assert_eq!(other.seq, PersistedAgentEventSeq::new(0));
    assert_eq!(
        fs::read(&journal_path).expect("poisoned journal"),
        bytes_after_failure
    );
}

/// A poisoned agent journal keeps its early write rejection ahead of later
/// event validation, so callers receive the established deterministic error.
#[test]
fn poisoned_agent_journal_rejection_precedes_event_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    store
        .legacy_io
        .as_mut()
        .expect("legacy writer")
        .framed_appends
        .inject_fault(
            &journal_path,
            AppendFault {
                fail_write_at: Some(3),
                fail_truncate: true,
                ..AppendFault::default()
            },
        );
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "failed"),
            UnixMicros::new(42),
        )
        .expect_err("injected append poisons journal");

    let mismatched_agent_id = AgentId::parse("agent-2").expect("agent id");
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&mismatched_agent_id, "rejected"),
            UnixMicros::new(43),
        )
        .expect_err("poisoned journal rejects before event validation");

    assert!(matches!(error, AgentStoreError::Write { .. }));
    assert!(
        error
            .to_string()
            .contains("append disabled after an incomplete rollback")
    );
}

/// Read-only replay rejects a partial payload at EOF, while the next locked
/// append keeps the valid prefix and removes only that incomplete crash tail.
#[test]
fn strict_replay_rejects_partial_frame_before_valid_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let suffix = [5_u64.to_le_bytes().as_slice(), &[1, 2]].concat();
    OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal")
        .write_all(&suffix)
        .expect("append malformed suffix");

    let error = AgentStore::open(temp.path()).expect_err("strict replay rejects torn frame");

    assert!(matches!(error, AgentStoreError::Read { .. }));
    let mut store = AgentStore::open_lazy(temp.path()).expect("lazy store opens");
    let appended = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "recovered"),
            UnixMicros::new(43),
        )
        .expect("locked append repairs suffix");
    assert_eq!(appended.seq, PersistedAgentEventSeq::new(1));
    let events = store
        .agent_events(agent_id.as_str())
        .expect("journal reads");
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1].event,
        Event::AgentDisplayNameSet(name) if name.display_name == "recovered"
    ));
}

/// A complete framed record with a malformed durable controlled identifier is
/// a decode failure, not a repairable incomplete crash tail.
#[test]
fn strict_replay_rejects_framed_record_with_malformed_agent_message_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let valid = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1_u8; 16]),
        seq: PersistedAgentEventSeq::new(1),
        source: None,
        event: Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("message-1").expect("message id"),
            sender_id: agent_id.clone(),
            recipient: tau_proto::AgentMessageRecipient::Agent {
                agent_id: AgentId::parse("recipient-agent").expect("agent id"),
            },
            kind: tau_proto::AgentMessageKind::Message,
            message: "hello".to_owned(),
        }),
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: UnixMicros::new(42),
    };
    let mut malformed = serde_json::to_value(valid).expect("serialize record value");
    *malformed
        .pointer_mut("/event/payload/message_id")
        .expect("message id field") = serde_json::Value::String("bad.message".to_owned());
    let mut encoded = Vec::new();
    ciborium::into_writer(&malformed, &mut encoded).expect("encode malformed framed record");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write frame length");
    file.write_all(&encoded).expect("write complete frame");
    drop(file);
    let bytes_before = fs::read(&journal_path).expect("read malformed journal");

    let error = AgentStore::open(temp.path()).expect_err("malformed identifier must fail replay");

    assert!(matches!(error, AgentStoreError::Decode { .. }));
    let mut lazy = AgentStore::open_lazy(temp.path()).expect("lazy store opens");
    let append_error = lazy
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "must-not-append"),
            UnixMicros::new(43),
        )
        .expect_err("locked writer must reject complete malformed frame");
    assert!(matches!(append_error, AgentStoreError::Read { .. }));
    assert_eq!(
        fs::read(&journal_path).expect("read unchanged malformed journal"),
        bytes_before
    );
}

/// A complete journal frame with a malformed watched-agent status fails strict
/// replay and reports the fixed validation diagnostic without retaining a
/// permissive historical representation.
#[test]
fn strict_replay_rejects_framed_record_with_malformed_watch_work_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let valid = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1_u8; 16]),
        seq: PersistedAgentEventSeq::new(1),
        source: None,
        event: Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("message-1").expect("message id"),
            sender_id: AgentId::parse("watched-agent").expect("sender id"),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
                subscription_id: "watch-1".to_owned(),
                status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
                phase: tau_proto::AgentWorkStatusPhase::Working,
                title: Some("inspect replay".to_owned()),
                initial: false,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        }),
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
        recorded_at: UnixMicros::new(42),
    };
    let mut malformed = serde_json::to_value(valid).expect("serialize record value");
    *malformed
        .pointer_mut("/event/payload/watch_work_status/title")
        .expect("watch-status title field") = serde_json::Value::Null;
    let mut encoded = Vec::new();
    ciborium::into_writer(&malformed, &mut encoded).expect("encode malformed framed record");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write frame length");
    file.write_all(&encoded).expect("write complete frame");
    drop(file);

    let error =
        AgentStore::open(temp.path()).expect_err("malformed watch status must fail strict replay");

    assert!(matches!(error, AgentStoreError::Decode { .. }));
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("invalid watch work status: work-status title must be absent"),
        "replay diagnostic must identify the rejected watch-status shape: {diagnostic}"
    );
    assert!(
        diagnostic.len() < 512,
        "replay diagnostic must remain bounded: {diagnostic}"
    );
}

/// Builds the immutable first event used by durable append tests.
fn started_event(agent_id: &AgentId) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id.clone(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

/// Builds one sequence-advancing event used by durable append tests.
fn display_name_event(agent_id: &AgentId, display_name: &str) -> Event {
    Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
        agent_id: agent_id.clone(),
        display_name: display_name.to_owned(),
    })
}

/// A read-only snapshot of a missing durable agent must fail without creating
/// the requested agent directory or lock file.
#[test]
fn journal_snapshot_does_not_create_missing_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-missing").expect("agent id");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect_err("missing journal must fail");

    assert!(matches!(error, AgentStoreError::JournalMissing { .. }));
    assert!(!temp.path().join(agent_id.as_str()).exists());
}

/// A live writer exposes only its exact checkpoint prefix and remains writable.
#[test]
fn journal_snapshot_uses_lock_held_checkpoint_without_disrupting_writer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");

    let snapshot = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect("active writer exposes committed checkpoint");
    let records = snapshot
        .records(&agent_id)
        .expect("selected journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("valid checkpoint prefix");
    assert_eq!(records.len(), 1);

    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "still-live"),
        )
        .expect("later write survives trace");
}

/// Inactive strict capture derives EOF without trusting a stale checkpoint
/// witness.
#[test]
fn journal_snapshot_uses_inactive_eof_despite_mismatched_checkpoint_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-boundary").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    drop(store);
    AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect("inactive snapshot ignores stale checkpoint");
}

/// A live checkpoint with a mismatched replay cursor exhausts bounded
/// observation retries as checkpoint corruption while leaving its writer
/// usable.
#[test]
fn journal_snapshot_rejects_lock_held_checkpoint_sequence_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-sequence").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.next_seq = checkpoint.journal.next_seq.next();
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect_err("live mismatched checkpoint cursor");
    assert!(matches!(
        error,
        AgentStoreError::Read { path, source }
            if path == checkpoint_path && source.kind() == io::ErrorKind::InvalidData
    ));
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "still-live"),
        )
        .expect("writer survives failed trace");
}

/// Checkpoint production and validation share one fallible platform file
/// identity, so a writer can immediately read its own bound checkpoint.
#[test]
fn checkpoint_writer_reader_file_identity_round_trip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-identity-round-trip").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let agent_dir = temp.path().join(agent_id.as_str());
    let mut journal = File::open(agent_dir.join("events.cbor")).expect("journal");

    let checkpoint =
        read_journal_bound_checkpoint(&agent_dir.join("meta.json"), &agent_id, &mut journal)
            .expect("writer checkpoint binds to its journal");

    assert_eq!(checkpoint.agent_id, agent_id);
    assert_eq!(
        checkpoint.journal.covered_bytes,
        journal.metadata().expect("journal metadata").len()
    );
}

/// Live capture retries an inconsistent atomic checkpoint observation and
/// accepts the next exact journal-bound replacement within its fixed attempt
/// budget.
#[test]
fn journal_snapshot_retries_live_checkpoint_atomic_replacement_race() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-race").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    let generation_zero = prepared_snapshot_generation(temp.path(), &agent_id, 0);
    let generation_one = prepared_snapshot_generation(temp.path(), &agent_id, 1);
    install_snapshot_generation(&generation_zero, &agent_dir);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let checkpoint_path = agent_dir.join("meta.json");
    let expected = read_checkpoint(&generation_one.join("meta.json"))
        .expect("replacement checkpoint")
        .journal
        .covered_bytes;
    let attempts = Cell::new(0);
    let mut replacement = Some(generation_one);

    let covered =
        super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |attempt| {
            attempts.set(attempts.get() + 1);
            if attempt == 0 {
                install_snapshot_generation(
                    &replacement.take().expect("one replacement generation"),
                    &agent_dir,
                );
            }
        })
        .expect("second bounded observation succeeds");

    assert_eq!(attempts.get(), 2);
    assert_eq!(covered, expected);
    assert_eq!(
        read_checkpoint(&checkpoint_path)
            .expect("selected replacement checkpoint")
            .journal
            .covered_bytes,
        expected
    );
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_lazy(&target_root).expect("reopen replacement generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-capture"),
        )
        .expect("replacement generation remains appendable");
}

fn prepared_snapshot_generation(root: &Path, agent_id: &AgentId, suffix_events: usize) -> PathBuf {
    let generation_root = root.join(format!("generation-{suffix_events}"));
    let mut store = AgentStore::open_lazy(&generation_root).expect("generation store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
        .expect("generation creation");
    for index in 0..suffix_events {
        store
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(agent_id, &format!("generation-{suffix_events}-{index}")),
            )
            .expect("generation suffix");
    }
    drop(store);
    generation_root.join(agent_id.as_str())
}

fn prepared_collision_generation(root: &Path, agent_id: &AgentId, marker: &str) -> (PathBuf, u64) {
    let generation_root = root.join(format!("collision-{marker}"));
    let mut store = AgentStore::open_lazy(&generation_root).expect("generation store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
        .expect("generation creation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(agent_id, marker),
        )
        .expect("different equal-length prefix");
    let agent_dir = generation_root.join(agent_id.as_str());
    let final_record_offset = read_checkpoint(&agent_dir.join("meta.json"))
        .expect("middle checkpoint")
        .journal
        .covered_bytes;
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(agent_id, &"same-terminal-boundary-".repeat(8)),
        )
        .expect("identical long boundary suffix");
    drop(store);
    (agent_dir, final_record_offset)
}

fn install_snapshot_generation(source: &Path, target: &Path) {
    let checkpoint = read_checkpoint(&source.join("meta.json")).expect("source checkpoint");
    fs::rename(source.join("events.cbor"), target.join("events.cbor"))
        .expect("atomically replace journal generation");
    crate::agent_checkpoint::write_checkpoint_atomic(&target.join("meta.json"), &checkpoint)
        .expect("atomically publish bound checkpoint");
}

/// Equal-length generations with equal replay cursors and boundary witnesses
/// still cannot cross the open/checkpoint observation boundary.
#[test]
fn journal_snapshot_rejects_collision_shaped_generation_crossing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-collision").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    let (generation_zero, zero_final_offset) =
        prepared_collision_generation(temp.path(), &agent_id, "generation-a");
    let (generation_one, one_final_offset) =
        prepared_collision_generation(temp.path(), &agent_id, "generation-b");
    assert_eq!(zero_final_offset, one_final_offset);
    let checkpoint_zero =
        read_checkpoint(&generation_zero.join("meta.json")).expect("old checkpoint");
    let mut checkpoint_one =
        read_checkpoint(&generation_one.join("meta.json")).expect("new checkpoint");
    let old_bytes = fs::read(generation_zero.join("events.cbor")).expect("old journal");
    let mut new_bytes = fs::read(generation_one.join("events.cbor")).expect("new journal");
    new_bytes[usize::try_from(one_final_offset).expect("bounded offset")..]
        .copy_from_slice(&old_bytes[usize::try_from(zero_final_offset).expect("bounded offset")..]);
    fs::write(generation_one.join("events.cbor"), &new_bytes).expect("copy identical final record");
    checkpoint_one.journal.boundary_blake3_128 =
        checkpoint_zero.journal.boundary_blake3_128.clone();
    crate::agent_checkpoint::write_checkpoint_atomic(
        &generation_one.join("meta.json"),
        &checkpoint_one,
    )
    .expect("publish collision-shaped checkpoint");
    assert_eq!(
        checkpoint_zero.journal.covered_bytes,
        checkpoint_one.journal.covered_bytes
    );
    assert_eq!(
        checkpoint_zero.journal.next_seq,
        checkpoint_one.journal.next_seq
    );
    assert_eq!(
        checkpoint_zero.journal.boundary_blake3_128,
        checkpoint_one.journal.boundary_blake3_128
    );
    assert_ne!(old_bytes, new_bytes);
    install_snapshot_generation(&generation_zero, &agent_dir);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let attempts = Cell::new(0);
    let mut replacement = Some(generation_one);

    let covered =
        super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |attempt| {
            attempts.set(attempts.get() + 1);
            if attempt == 0 {
                install_snapshot_generation(
                    &replacement.take().expect("one replacement generation"),
                    &agent_dir,
                );
            }
        })
        .expect("identity mismatch retries to the exact replacement");

    assert_eq!(attempts.get(), 2);
    assert_eq!(covered, checkpoint_one.journal.covered_bytes);
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_lazy(&target_root).expect("reopen replacement generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-collision-capture"),
        )
        .expect("replacement generation remains appendable");
}

/// Live capture attempts three real journal/checkpoint generation crossings
/// before returning a checkpoint-path error.
#[test]
fn journal_snapshot_live_checkpoint_retry_budget_is_bounded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-budget").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    install_snapshot_generation(
        &prepared_snapshot_generation(temp.path(), &agent_id, 0),
        &agent_dir,
    );
    let mut replacements = (1..=3)
        .map(|suffix| prepared_snapshot_generation(temp.path(), &agent_id, suffix))
        .collect::<VecDeque<_>>();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let checkpoint_path = agent_dir.join("meta.json");
    let attempts = Cell::new(0);

    let error = super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |_| {
        attempts.set(attempts.get() + 1);
        install_snapshot_generation(
            &replacements.pop_front().expect("replacement per attempt"),
            &agent_dir,
        );
    })
    .expect_err("retry budget exhausts");

    assert_eq!(attempts.get(), 3);
    assert!(matches!(
        error,
        AgentStoreError::Read { path, source }
            if path == checkpoint_path && source.kind() == io::ErrorKind::InvalidData
    ));
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_lazy(&target_root).expect("reopen final generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-exhaustion"),
        )
        .expect("final replacement remains appendable");
}

/// Inactive capture must acquire the shared lock before selecting EOF, so
/// an append and writer release immediately before acquisition are included.
#[test]
fn journal_snapshot_selects_inactive_eof_after_lock_acquisition() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-race").expect("agent id");
    let mut store = AgentStore::open_lazy(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let journal_path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_append = fs::metadata(&journal_path).expect("journal metadata").len();
    let mut store = Some(store);

    let snapshot = AgentJournalSnapshot::capture_for_test(temp.path(), [agent_id.clone()], |_| {
        store
            .as_mut()
            .expect("writer")
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(&agent_id, "before-lock"),
            )
            .expect("append before snapshot lock");
        drop(store.take());
    })
    .expect("inactive snapshot after writer release");

    assert!(
        before_append
            < fs::metadata(&journal_path)
                .expect("updated journal metadata")
                .len()
    );
    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("snapshot records")
            .collect::<Result<Vec<_>, _>>()
            .expect("complete locked EOF")
            .len(),
        2
    );
}

/// Snapshot validation must reject a journal whose stored sequence no longer
/// matches its authoritative file position.
#[test]
fn journal_snapshot_rejects_non_monotonic_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-corrupt").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let mut bytes = fs::read(&path).expect("journal");
    let seq = bytes
        .windows(5)
        .position(|window| window == b"\x63seq\x00")
        .map(|offset| offset + 4)
        .expect("sequence encoding");
    bytes[seq] = 1;
    fs::write(&path, bytes).expect("corrupt sequence");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect_err("corrupt journal must fail");

    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));
}

/// Snapshot validation must reject a crash-torn frame rather than exporting a
/// valid prefix as though it were the complete journal.
#[test]
fn journal_snapshot_rejects_torn_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-torn").expect("agent id");
    {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal")
        .write_all(&[1, 2, 3])
        .expect("torn header");

    let error =
        AgentJournalSnapshot::capture(temp.path(), [agent_id]).expect_err("torn journal must fail");

    assert!(matches!(error, AgentStoreError::Read { .. }));
}

/// A captured snapshot retains all journal locks, preventing records from
/// changing until the consumer has finished with the validated data.
#[test]
fn journal_snapshot_prevents_changes_during_read() {
    let temp = tempfile::tempdir().expect("tempdir");
    let first = AgentId::parse("agent-a").expect("agent id");
    let second = AgentId::parse("agent-b").expect("agent id");
    for agent_id in [&second, &first] {
        let mut store = AgentStore::open_lazy(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
            .expect("creation");
    }
    let snapshot = AgentJournalSnapshot::capture(temp.path(), [second.clone(), first.clone()])
        .expect("stable snapshot");
    assert_eq!(
        snapshot.agent_ids().collect::<Vec<_>>(),
        vec![&first, &second],
        "snapshot identities use lexical agent order"
    );
    let absent = AgentId::parse("agent-absent").expect("agent id");
    assert!(matches!(
        snapshot.records(&absent),
        Err(AgentStoreError::JournalNotIncluded { agent_id }) if agent_id == absent
    ));

    let mut writer = AgentStore::open_lazy(temp.path()).expect("writer");
    let error = writer
        .append_agent_event(first.as_str(), None, display_name_event(&first, "changed"))
        .expect_err("snapshot lock must prevent append");
    assert!(matches!(error, AgentStoreError::Locked { .. }));
    assert_eq!(
        snapshot
            .records(&first)
            .expect("stable records")
            .collect::<Result<Vec<_>, _>>()
            .expect("records")
            .len(),
        1
    );
}
