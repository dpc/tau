//! Deterministic regression tests for durable Rostra notification state.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rostra_client::RostraId;
use rostra_core::id::RostraIdSecretKey;
use tau_proto::{
    AgentId, CborValue, ExtensionName, MessageAgentTarget, MessageDelivered, MessageExtensionData,
    MessageFactId, MessageParty, MessagePublisherId, UnixMillis,
};

use super::*;

/// Decodes an opaque cursor only in tests; production code preserves it
/// opaquely.
fn cursor(position: u64) -> SocialPostMaterializationCursor {
    serde_json::from_value(serde_json::json!(position)).expect("opaque cursor JSON")
}

/// Returns the stable typed publisher identity used by state-file tests.
fn publisher() -> ExtensionName {
    ExtensionName::parse("std-rostra").expect("publisher")
}

/// Returns a stable typed Rostra identity used by state-file tests.
fn identity() -> RostraId {
    static IDENTITY: OnceLock<RostraId> = OnceLock::new();
    *IDENTITY.get_or_init(|| RostraIdSecretKey::generate().id())
}

/// Creates an isolated configured state plus its durable directory.
fn configured_state() -> (tempfile::TempDir, State) {
    let directory = tempfile::tempdir().expect("state directory");
    let mut state = State::default();
    state
        .configure(publisher(), identity(), directory.path())
        .expect("configure empty state");
    (directory, state)
}

/// Builds one canonical echo carrying the exact opaque scanned cursor.
fn delivered(agent: &AgentId, end: SocialPostMaterializationCursor) -> MessageDelivered {
    let mut delivered = MessageDelivered::new(
        MessagePublisherId::parse("std-rostra").expect("publisher"),
        MessageAgentTarget::new(agent.as_ref()),
        MessageFactId::new("test-batch"),
        MessageParty {
            stable_id: "rostra-following".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "test",
    );
    delivered.extension_data = MessageExtensionData::new(CborValue::Map(vec![
        (
            CborValue::Text("schema".to_owned()),
            CborValue::Text(SCHEMA.to_owned()),
        ),
        (
            CborValue::Text("scanned_through".to_owned()),
            serde_json::to_value(end)
                .map(|value| tau_proto::json_to_cbor(&value))
                .expect("cursor"),
        ),
    ]))
    .expect("extension data");
    delivered
}

/// Ensures the immutable first-enable baseline remains distinct from an
/// advanced page cursor.
#[test]
fn stored_registration_preserves_first_enable_baseline() {
    let registration = Registration {
        baseline: cursor(4),
        committed: cursor(17),
        last_canonical_report_unix_ms: None,
        pending: None,
        inflight_end: None,
        queued_since_unix_ms: None,
    };
    let stored = registration.stored();
    assert_eq!(stored.baseline, cursor(4));
    assert_eq!(stored.committed, cursor(17));
}

/// Ensures report spacing is reconstructed from a durable canonical-echo time
/// after restart.
#[test]
fn durable_report_spacing_survives_reconstruction() {
    const TOLERANCE: Duration = Duration::from_secs(1);

    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(5));
    assert!(
        state
            .acknowledge(&delivered(&agent, cursor(5)))
            .expect("canonical echo")
    );
    drop(state);

    let mut reconstructed = State::default();
    reconstructed
        .configure(publisher(), identity(), directory.path())
        .expect("reopen persisted checkpoint");
    let registration = reconstructed
        .registrations
        .get_mut(&agent)
        .expect("persisted registration");
    let last_report = registration
        .last_canonical_report_unix_ms
        .expect("persisted canonical report time");
    let now = Instant::now();
    let wall_clock_now = now_ms().max(last_report);
    registration.pending = Some(Pending {
        end: cursor(5),
        first_queued_at: now.checked_sub(MAX_BATCH_AGE).expect("monotonic start"),
        last_queued_at: now.checked_sub(IDLE_DEBOUNCE).expect("monotonic idle"),
        count: 1,
    });
    let due = registration
        .due_at(
            now,
            wall_clock_now,
            IDLE_DEBOUNCE,
            MAX_BATCH_AGE,
            REPORT_INTERVAL,
        )
        .expect("due time");
    let elapsed = Duration::from_millis(wall_clock_now.get().saturating_sub(last_report.get()));
    let expected_remaining = REPORT_INTERVAL.saturating_sub(elapsed);
    let remaining = due.duration_since(now);
    assert!(
        expected_remaining.saturating_sub(TOLERANCE) <= remaining
            && remaining <= expected_remaining + TOLERANCE,
        "reconstructed report spacing {remaining:?} must remain approximately {expected_remaining:?}"
    );
}

/// Ensures each pre-rename write phase leaves memory and a restarted state
/// unchanged.
#[test]
fn pre_rename_persistence_failures_roll_back_enable() {
    for fault in [
        PersistFault::Write,
        PersistFault::FileSync,
        PersistFault::Rename,
    ] {
        let (directory, mut state) = configured_state();
        let agent = AgentId::parse("agent").expect("agent id");
        state.fault = Some(fault);
        assert!(state.enable(agent.clone(), cursor(4)).is_err());
        assert!(!state.registrations.contains_key(&agent));
        state.fault = None;
        let mut restarted = State::default();
        restarted
            .configure(publisher(), identity(), directory.path())
            .expect("restart after pre-rename failure");
        assert!(!restarted.registrations.contains_key(&agent));
    }
}

/// Ensures a directory-sync failure keeps memory aligned with the renamed file,
/// poisons later mutations, and reports failure instead of claiming a durable
/// tool success.
#[test]
fn post_rename_failure_installs_memory_then_poisoning_blocks_mutations() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.fault = Some(PersistFault::DirectorySync);
    assert!(state.enable(agent.clone(), cursor(4)).is_err());
    assert!(state.registrations.contains_key(&agent));
    assert!(state.disable(&agent).is_err());
    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart reads renamed candidate");
    assert!(restarted.registrations.contains_key(&agent));
}

/// Ensures a canonical report echo durably advances the cursor across a process
/// restart.
#[test]
fn canonical_delivery_checkpoint_survives_restart() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(17));
    assert!(
        state
            .acknowledge(&delivered(&agent, cursor(17)))
            .expect("acknowledge")
    );
    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart");
    let registration = restarted
        .registrations
        .get(&agent)
        .expect("restored registration");
    assert_eq!(registration.baseline, cursor(4));
    assert_eq!(registration.committed, cursor(17));
}

/// Ensures count-only report metadata changes do not migrate the durable v1
/// checkpoint file that existing notification registrations must still load.
#[test]
fn durable_checkpoint_file_schema_remains_v1() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent, cursor(4)).expect("enable");
    let path = directory.path().join("rostra-notifications-v1.cbor");
    let bytes = std::fs::read(path).expect("state file");
    let stored: StoredState =
        ciborium::de::from_reader(bytes.as_slice()).expect("stored state schema");
    assert_eq!(stored.schema, "rostra-notifications-v1");
    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("existing v1 checkpoint loads");
}

/// Ensures the typed durable report attempt retains the exact legacy CBOR
/// scalar representation at the largest valid value.
#[test]
fn stored_report_attempt_preserves_legacy_cbor_scalar() {
    #[derive(serde::Serialize)]
    struct LegacyStoredState {
        schema: String,
        publisher: ExtensionName,
        rostra_identity: RostraId,
        next_report_attempt: u64,
        agents: BTreeMap<String, StoredRegistration>,
    }

    let stored = StoredState {
        schema: STATE_SCHEMA.to_owned(),
        publisher: publisher(),
        rostra_identity: identity(),
        next_report_attempt: ReportAttempt(u64::MAX),
        agents: BTreeMap::new(),
    };
    let legacy = LegacyStoredState {
        schema: stored.schema.clone(),
        publisher: stored.publisher.clone(),
        rostra_identity: stored.rostra_identity,
        next_report_attempt: u64::MAX,
        agents: BTreeMap::new(),
    };
    let mut encoded = Vec::new();
    ciborium::ser::into_writer(&stored, &mut encoded).expect("typed state encodes");
    let mut legacy_encoded = Vec::new();
    ciborium::ser::into_writer(&legacy, &mut legacy_encoded).expect("legacy state encodes");
    assert_eq!(encoded, legacy_encoded);
    let decoded = ciborium::de::from_reader::<StoredState, _>(legacy_encoded.as_slice())
        .expect("legacy report-attempt scalar decodes");
    assert_eq!(decoded.next_report_attempt, stored.next_report_attempt);
}

/// Ensures report attempts retain zero origin, decimal fact-ID spelling, and
/// the checked exhaustion boundary without wrapping or reusing an ID.
#[test]
fn report_attempt_preserves_zero_decimal_max_and_exhaustion() {
    assert_eq!(
        ReportAttempt::default().fact_id().as_str(),
        "rostra-batch-v1:0"
    );
    assert_eq!(ReportAttempt(42).fact_id().as_str(), "rostra-batch-v1:42");
    assert_eq!(
        ReportAttempt(u64::MAX).fact_id().as_str(),
        "rostra-batch-v1:18446744073709551615"
    );
    assert_eq!(ReportAttempt(0).next(), Some(ReportAttempt(1)));
    assert_eq!(ReportAttempt(u64::MAX).next(), None);
}

/// Ensures allocation at the terminal report attempt fails before persistence
/// or reuse can advance the durable counter.
#[test]
fn report_attempt_allocation_rejects_exhaustion_without_advancing() {
    let (_directory, mut state) = configured_state();
    state.next_report_attempt = ReportAttempt(u64::MAX);
    assert_eq!(
        state.allocate_report_attempt(),
        Err("notification report attempt counter is exhausted")
    );
    assert_eq!(state.next_report_attempt, ReportAttempt(u64::MAX));
}

/// Ensures transparent Unix-millisecond timestamps write the exact legacy
/// checkpoint CBOR field bytes and continue to decode from that representation.
#[test]
fn stored_registration_timestamps_preserve_legacy_cbor_bytes() {
    #[derive(serde::Serialize)]
    struct LegacyStoredRegistration {
        baseline: SocialPostMaterializationCursor,
        committed: SocialPostMaterializationCursor,
        last_canonical_report_unix_ms: Option<u64>,
        queued_since_unix_ms: Option<u64>,
    }

    let registration = StoredRegistration {
        baseline: cursor(4),
        committed: cursor(17),
        last_canonical_report_unix_ms: Some(UnixMillis::new(1)),
        queued_since_unix_ms: Some(UnixMillis::new(u64::MAX)),
    };
    let legacy = LegacyStoredRegistration {
        baseline: cursor(4),
        committed: cursor(17),
        last_canonical_report_unix_ms: Some(1),
        queued_since_unix_ms: Some(u64::MAX),
    };
    let mut encoded = Vec::new();
    ciborium::ser::into_writer(&registration, &mut encoded).expect("typed registration encodes");
    let mut legacy_encoded = Vec::new();
    ciborium::ser::into_writer(&legacy, &mut legacy_encoded).expect("legacy registration encodes");
    assert_eq!(encoded, legacy_encoded);
    let decoded = ciborium::de::from_reader::<StoredRegistration, _>(legacy_encoded.as_slice())
        .expect("legacy timestamp scalars decode");
    assert_eq!(decoded.baseline, registration.baseline);
    assert_eq!(decoded.committed, registration.committed);
    assert_eq!(
        decoded.last_canonical_report_unix_ms,
        registration.last_canonical_report_unix_ms
    );
    assert_eq!(
        decoded.queued_since_unix_ms,
        registration.queued_since_unix_ms
    );
}

/// Ensures timestamp conversion retains both the pre-epoch zero fallback and
/// saturation of duration values too large for a Unix-millisecond scalar.
#[test]
fn unix_millis_clock_preserves_pre_epoch_fallback_and_saturation() {
    let pre_epoch = UNIX_EPOCH
        .checked_sub(Duration::from_millis(1))
        .expect("pre-epoch system time")
        .duration_since(UNIX_EPOCH);
    assert_eq!(unix_millis_since_epoch(pre_epoch), UnixMillis::new(0));
    assert_eq!(
        unix_millis_since_epoch(Ok(Duration::new(u64::MAX, 999_999_999))),
        UnixMillis::new(u64::MAX)
    );
}

/// Ensures report spacing keeps its exact millisecond boundary and truncates
/// sub-millisecond duration remainder just as the durable scalar policy did.
#[test]
fn report_due_at_preserves_millisecond_boundary_and_rounding() {
    let now = Instant::now();
    let pending = Pending {
        end: cursor(5),
        first_queued_at: now.checked_sub(MAX_BATCH_AGE).expect("batch age"),
        last_queued_at: now.checked_sub(IDLE_DEBOUNCE).expect("idle age"),
        count: 1,
    };
    let registration = Registration {
        baseline: cursor(4),
        committed: cursor(4),
        last_canonical_report_unix_ms: Some(UnixMillis::new(1_000)),
        pending: Some(pending),
        inflight_end: None,
        queued_since_unix_ms: None,
    };
    assert_eq!(
        registration
            .due_at(
                now,
                UnixMillis::new(1_999),
                IDLE_DEBOUNCE,
                MAX_BATCH_AGE,
                Duration::from_secs(1),
            )
            .expect("one millisecond remains")
            .duration_since(now),
        Duration::from_millis(1)
    );
    assert_eq!(
        registration
            .due_at(
                now,
                UnixMillis::new(1_000),
                IDLE_DEBOUNCE,
                MAX_BATCH_AGE,
                Duration::from_micros(1_500),
            )
            .expect("sub-millisecond remainder truncates")
            .duration_since(now),
        Duration::from_millis(1)
    );
}

/// Ensures restart recovery reconstructs a persisted queued age through a
/// rescan, preserving an already-due live batch's scheduling result.
#[test]
fn restart_rescan_preserves_live_queued_age_scheduling() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    let first_scan = state.scan_snapshot(&agent).expect("first scan");
    state
        .merge_page(
            &agent,
            &first_scan,
            ScannedPage {
                scanned_through: cursor(5),
                had_items: true,
                exhausted: true,
                count: 1,
            },
        )
        .expect("first selected row");
    {
        let registration = state
            .registrations
            .get_mut(&agent)
            .expect("queued registration");
        registration.queued_since_unix_ms = Some(UnixMillis::new(
            now_ms().get().saturating_sub(duration_ms(MAX_BATCH_AGE)),
        ));
        registration
            .pending
            .as_mut()
            .expect("live pending page")
            .first_queued_at = Instant::now()
            .checked_sub(MAX_BATCH_AGE)
            .expect("live batch age");
    }
    state
        .commit(state.registrations.clone())
        .expect("persist queued age");
    let live_now = Instant::now();
    assert_eq!(
        state.registrations[&agent]
            .due_at(
                live_now,
                now_ms(),
                IDLE_DEBOUNCE,
                MAX_BATCH_AGE,
                REPORT_INTERVAL,
            )
            .expect("live overdue batch"),
        live_now
    );
    drop(state);

    let mut restored_state = State::default();
    restored_state
        .configure(publisher(), identity(), directory.path())
        .expect("restore checkpoint");
    let replay_scan = restored_state
        .scan_snapshot(&agent)
        .expect("replay scan after restart");
    restored_state
        .merge_page(
            &agent,
            &replay_scan,
            ScannedPage {
                scanned_through: cursor(5),
                had_items: true,
                exhausted: true,
                count: 1,
            },
        )
        .expect("replay selected row");
    let restored_now = Instant::now();
    assert_eq!(
        restored_state.registrations[&agent]
            .due_at(
                restored_now,
                now_ms(),
                IDLE_DEBOUNCE,
                MAX_BATCH_AGE,
                REPORT_INTERVAL,
            )
            .expect("restored overdue batch"),
        restored_now
    );
}

/// Ensures replayed or stale canonical facts cannot checkpoint an un-emitted
/// page.
#[test]
fn only_the_live_inflight_delivery_can_checkpoint() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    assert!(
        !state
            .acknowledge(&delivered(&agent, cursor(17)))
            .expect("stale delivery")
    );
    assert_eq!(state.registrations[&agent].committed, cursor(4));
}

/// Ensures a selected non-exhausted page keeps scanning immediately to merge
/// later feed pages before trailing debounce, but stops after report enqueue.
#[test]
fn pending_page_continuation_runs_until_a_report_is_inflight() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.loaded(agent.clone());
    state.replay_complete(agent.clone());
    state.continuations.insert(agent.clone());
    let now = Instant::now();
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .pending = Some(Pending {
        end: cursor(17),
        first_queued_at: now,
        last_queued_at: now,
        count: 1,
    });
    let deadline = state.next_deadline().expect("continuation deadline");
    assert!(
        deadline
            < now
                .checked_add(Duration::from_secs(1))
                .expect("immediate continuation bound")
    );
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(17));
    assert_eq!(state.next_deadline(), None);
}

/// Ensures sequential one-row state-machine outcomes advance filtered and
/// selected cursors immediately, carry the final cursor into one report, and
/// let exactly one matching canonical echo commit that cursor.
#[test]
fn one_row_continuation_advances_and_canonical_echo_commits_once() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.loaded(agent.clone());
    state.replay_complete(agent.clone());

    let first = state.scan_snapshot(&agent).expect("first scan");
    state
        .merge_page(
            &agent,
            &first,
            ScannedPage {
                scanned_through: cursor(5),
                had_items: true,
                exhausted: false,
                count: 0,
            },
        )
        .expect("merge filtered row");
    assert_eq!(state.registrations[&agent].committed, cursor(5));
    assert!(state.next_deadline().is_some_and(|deadline| {
        deadline
            < Instant::now()
                .checked_add(Duration::from_secs(1))
                .expect("immediate continuation bound")
    }));

    let second = state.scan_snapshot(&agent).expect("second scan");
    state
        .merge_page(
            &agent,
            &second,
            ScannedPage {
                scanned_through: cursor(6),
                had_items: true,
                exhausted: false,
                count: 1,
            },
        )
        .expect("merge selected row");
    assert!(state.next_deadline().is_some_and(|deadline| {
        deadline
            < Instant::now()
                .checked_add(Duration::from_secs(1))
                .expect("selected-row continuation bound")
    }));
    let third = state.scan_snapshot(&agent).expect("third scan");
    state
        .merge_page(
            &agent,
            &third,
            ScannedPage {
                scanned_through: cursor(7),
                had_items: true,
                exhausted: true,
                count: 0,
            },
        )
        .expect("merge final filtered row");
    let pending = state.registrations[&agent]
        .pending
        .as_ref()
        .expect("selected batch");
    assert_eq!(pending.end, cursor(7));
    assert_eq!(pending.count, 1);
    assert!(!state.continuations.contains(&agent));

    state.set_pending_due(&agent, cursor(7), 1);
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(7));
    let echo = delivered(&agent, cursor(7));
    assert!(state.acknowledge(&echo).expect("first canonical echo"));
    assert_eq!(state.registrations[&agent].committed, cursor(7));
    assert!(!state.acknowledge(&echo).expect("duplicate canonical echo"));
}

/// Ensures unloading cancels a pending scan continuation rather than retaining
/// runnable source work for an agent that no longer has a live delivery gate.
#[test]
fn unloading_cancels_notification_continuation() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.loaded(agent.clone());
    state.replay_complete(agent.clone());
    state.continuations.insert(agent.clone());
    state.unloaded(&agent);
    assert_eq!(state.next_deadline(), None);
}

/// Ensures the identity-wide attempt counter survives an empty registration
/// set, so disabling and later re-enabling an agent cannot reuse a retained
/// publisher-scoped message ID.
#[test]
fn report_attempt_counter_survives_disable_and_restart() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    assert_eq!(
        state.allocate_report_attempt().expect("first attempt"),
        ReportAttempt(0)
    );
    state.disable(&agent).expect("disable");
    assert_eq!(
        state.allocate_report_attempt().expect("second attempt"),
        ReportAttempt(1)
    );
    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart");
    assert_eq!(
        restarted.allocate_report_attempt().expect("third attempt"),
        ReportAttempt(2)
    );
}

/// Ensures retry backoff delays an otherwise immediate continuation, preventing
/// failed source work from turning an overdue page into a ready-loop.
#[test]
fn retry_backoff_overrides_immediate_continuation() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.loaded(agent.clone());
    state.replay_complete(agent.clone());
    state.continuations.insert(agent);
    let retry_at = Instant::now()
        .checked_add(Duration::from_secs(1))
        .expect("retry deadline");
    state.retry_at = Some(retry_at);
    assert_eq!(state.next_deadline(), Some(retry_at));
}

/// Ensures a pre-rename checkpoint failure neither advances memory nor lets a
/// restarted process treat the old canonical echo as a second live delivery.
#[test]
fn pre_rename_checkpoint_failure_requires_restart_reconciliation() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(17));
    state.fault = Some(PersistFault::Write);
    assert!(state.acknowledge(&delivered(&agent, cursor(17))).is_err());
    assert_eq!(state.registrations[&agent].committed, cursor(4));

    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart after failed checkpoint");
    assert_eq!(restarted.registrations[&agent].committed, cursor(4));
    assert!(
        !restarted
            .acknowledge(&delivered(&agent, cursor(17)))
            .expect("restart must not require a second live echo")
    );
}

/// Ensures a post-rename checkpoint failure recovers the visible checkpoint on
/// restart without depending on a duplicate live canonical echo.
#[test]
fn post_rename_checkpoint_failure_recovers_without_second_live_echo() {
    let (directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .inflight_end = Some(cursor(17));
    state.fault = Some(PersistFault::DirectorySync);
    assert!(state.acknowledge(&delivered(&agent, cursor(17))).is_err());
    assert_eq!(state.registrations[&agent].committed, cursor(17));

    let mut restarted = State::default();
    restarted
        .configure(publisher(), identity(), directory.path())
        .expect("restart reads visible checkpoint");
    assert_eq!(restarted.registrations[&agent].committed, cursor(17));
    assert!(
        !restarted
            .acknowledge(&delivered(&agent, cursor(17)))
            .expect("restart must not require a second live echo")
    );
}

/// Ensures a future retry deadline clamps an already overdue pending report,
/// preventing wake-driven reconciliation from becoming a CPU ready-loop.
#[test]
fn retry_backoff_overrides_overdue_pending_report() {
    let (_directory, mut state) = configured_state();
    let agent = AgentId::parse("agent").expect("agent id");
    state.enable(agent.clone(), cursor(4)).expect("enable");
    state.loaded(agent.clone());
    state.replay_complete(agent.clone());
    let now = Instant::now();
    state
        .registrations
        .get_mut(&agent)
        .expect("registration")
        .pending = Some(Pending {
        end: cursor(17),
        first_queued_at: now.checked_sub(MAX_BATCH_AGE).expect("old pending page"),
        last_queued_at: now.checked_sub(MAX_BATCH_AGE).expect("old pending page"),
        count: 1,
    });
    let retry_at = now
        .checked_add(Duration::from_secs(1))
        .expect("retry deadline");
    state.retry_at = Some(retry_at);
    assert_eq!(state.next_deadline(), Some(retry_at));
}

/// Ensures a later merged row extends trailing debounce without moving the
/// original batch-age cap.
#[test]
fn merged_rows_extend_idle_debounce_but_keep_batch_age_cap() {
    let now = Instant::now();
    let mut registration = Registration {
        baseline: cursor(4),
        committed: cursor(4),
        last_canonical_report_unix_ms: None,
        pending: Some(Pending {
            end: cursor(5),
            first_queued_at: now
                .checked_sub(Duration::from_secs(29))
                .expect("first selected row"),
            last_queued_at: now,
            count: 2,
        }),
        inflight_end: None,
        queued_since_unix_ms: None,
    };
    assert!(
        now < registration
            .due_at(now, now_ms(), IDLE_DEBOUNCE, MAX_BATCH_AGE, REPORT_INTERVAL)
            .expect("later row extends idle debounce")
    );
    registration
        .pending
        .as_mut()
        .expect("pending")
        .first_queued_at = now.checked_sub(MAX_BATCH_AGE).expect("batch age");
    assert_eq!(
        registration
            .due_at(now, now_ms(), IDLE_DEBOUNCE, MAX_BATCH_AGE, REPORT_INTERVAL)
            .expect("five-minute cap"),
        now
    );
}
