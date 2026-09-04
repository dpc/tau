#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::{fs as path_std_fs, io as path_std_io};

use fs2::FileExt as _;
use tau_core::{AgentEventParent, AgentStore, SessionStore};
use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionAgentUnloaded, SessionId, UnixMicros};
use tempfile::TempDir;

/// Future timestamps remain conservatively ineligible.
#[test]
fn future_timestamp_is_not_expired() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    assert!(!super::expired(
        now,
        now + Duration::from_secs(1),
        Duration::from_secs(1)
    ));
}

/// The exact retention cutoff is inclusive.
#[test]
fn exact_timestamp_cutoff_is_expired() {
    let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    assert!(super::expired(
        timestamp + Duration::from_secs(5),
        timestamp,
        Duration::from_secs(5)
    ));
}

fn write_old_agent(state: &std::path::Path, name: &str) -> AgentId {
    let agent_id = AgentId::parse(name).expect("agent id");
    let mut store = AgentStore::open_lazy(state.join("agents")).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(1),
        )
        .expect("append creation");
    drop(store);
    agent_id
}

/// An exact old orphan is detached, removed, and permanently tombstoned.
#[test]
fn old_unreferenced_agent_is_removed_and_tombstoned() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "old-agent");
    let now = SystemTime::now() + Duration::from_secs(60);

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        now,
    );

    assert_eq!(summary.detached, 1);
    assert_eq!(summary.removed, 1);
    assert!(!temp.path().join("agents/old-agent").exists());
    assert!(tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}

/// A surviving session's historical durable load protects an agent even after
/// a later unload removes it from current membership.
#[test]
fn ever_loaded_session_reference_protects_unloaded_agent() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "referenced-agent");
    let session_id = SessionId::parse("session").expect("session id");
    let mut sessions = SessionStore::open(temp.path().join("sessions")).expect("session store");
    sessions
        .append_session_event(
            session_id.as_str(),
            None,
            Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("init")
                    .expect("initialization id"),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("append load");
    sessions
        .append_session_event(
            session_id.as_str(),
            None,
            Event::SessionAgentUnloaded(SessionAgentUnloaded {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
            }),
        )
        .expect("append unload");
    drop(sessions);

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_referenced, 1);
    assert!(temp.path().join("agents/referenced-agent").exists());
    assert!(!tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}

/// A canonical manifest with no readable membership journal aborts the entire
/// agent-deletion portion because its ownership set is unknown.
#[test]
fn canonical_session_with_missing_journal_aborts_agent_deletion() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "orphan");
    let session = temp.path().join("sessions/canonical");
    path_std_fs::create_dir_all(&session).expect("session dir");
    path_std_fs::write(
        session.join("meta.json"),
        serde_json::to_vec(&tau_core::SessionMeta {
            created_at: 1,
            last_touched: 1,
        })
        .expect("meta"),
    )
    .expect("write meta");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.failures, 1);
    assert!(temp.path().join("agents/orphan").exists());
}

/// A live or snapshot-held lock preserves an otherwise expired candidate.
#[test]
fn held_agent_lock_preserves_candidate() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "locked-agent");
    let lock = path_std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join("agents/locked-agent/lock"))
        .expect("lock file");
    lock.lock_shared().expect("hold snapshot-style lock");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_locked, 1);
    assert!(temp.path().join("agents/locked-agent").exists());
}

/// A recent journal mtime independently fences deletion even when the semantic
/// checkpoint timestamp is old.
#[test]
fn recent_journal_mtime_preserves_old_semantic_timestamp() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "recent-journal");
    let modified = path_std_fs::metadata(temp.path().join("agents/recent-journal/events.cbor"))
        .expect("journal metadata")
        .modified()
        .expect("journal mtime");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(60),
        modified,
    );

    assert_eq!(summary.detached, 0);
    assert!(temp.path().join("agents/recent-journal").exists());
}

/// Corrupt checkpoint JSON cannot become deletion authority.
#[test]
fn corrupt_checkpoint_preserves_candidate() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "corrupt-agent");
    path_std_fs::write(
        temp.path().join("agents/corrupt-agent/meta.json"),
        b"not json",
    )
    .expect("corrupt checkpoint");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_invalid, 1);
    assert!(temp.path().join("agents/corrupt-agent").exists());
}

/// A symlinked agent artifact is retained rather than followed as age
/// authority.
#[cfg(unix)]
#[test]
fn symlinked_checkpoint_preserves_candidate_and_target() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "linked-agent");
    let checkpoint = temp.path().join("agents/linked-agent/meta.json");
    let external = temp.path().join("external-meta.json");
    path_std_fs::rename(&checkpoint, &external).expect("move checkpoint");
    unix_fs::symlink(&external, &checkpoint).expect("checkpoint symlink");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_invalid, 1);
    assert!(temp.path().join("agents/linked-agent").exists());
    assert!(external.exists());
}

/// A structurally valid checkpoint with a false next sequence is corrupt and
/// cannot authorize deletion.
#[test]
fn wrong_checkpoint_next_sequence_preserves_candidate() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "wrong-seq");
    let checkpoint_path = temp.path().join("agents/wrong-seq/meta.json");
    let mut checkpoint: tau_core::AgentCheckpoint =
        serde_json::from_slice(&path_std_fs::read(&checkpoint_path).expect("checkpoint bytes"))
            .expect("checkpoint");
    checkpoint.journal.next_seq = tau_core::PersistedAgentEventSeq::new(9);
    path_std_fs::write(
        &checkpoint_path,
        serde_json::to_vec_pretty(&checkpoint).expect("checkpoint json"),
    )
    .expect("write wrong sequence");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_invalid, 1);
    assert!(temp.path().join("agents/wrong-seq").exists());
}

/// Bytes beyond the exact checkpoint boundary retain the agent rather than
/// letting cleanup ignore an invalid or uncheckpointed suffix.
#[test]
fn journal_suffix_beyond_checkpoint_preserves_candidate() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "suffix-agent");
    let journal = temp.path().join("agents/suffix-agent/events.cbor");
    let mut bytes = path_std_fs::read(&journal).expect("journal");
    bytes.extend_from_slice(&[0, 0, 0, 8, 1, 2]);
    path_std_fs::write(&journal, bytes).expect("append invalid suffix");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.skipped_invalid, 1);
    assert!(temp.path().join("agents/suffix-agent").exists());
}

/// The candidate-specific scan observes a new ownership edge added after the
/// coarse pass and retains the candidate.
#[test]
fn candidate_rescan_catches_reference_added_after_coarse_scan() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "late-reference");
    let sessions_dir = temp.path().join("sessions");
    let mut inserted = false;

    let summary = super::cleanup_agents_with_before_rescan(
        &temp.path().join("agents"),
        &sessions_dir,
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
        |candidate| {
            if !inserted {
                inserted = true;
                let session_id = SessionId::parse("late-owner").expect("session id");
                let mut sessions = SessionStore::open(&sessions_dir).expect("session store");
                sessions
                    .append_session_event(
                        session_id.as_str(),
                        None,
                        Event::SessionAgentLoaded(SessionAgentLoaded {
                            agent_initialization_id: tau_proto::AgentInitializationId::parse(
                                "late-init",
                            )
                            .expect("initialization id"),
                            session_id: session_id.clone(),
                            agent_id: candidate.clone(),
                            ephemeral: false,
                        }),
                    )
                    .expect("append late reference");
            }
        },
    );

    assert_eq!(summary.skipped_referenced, 1);
    assert!(temp.path().join("agents/late-reference").exists());
    assert!(!tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}

/// A symlinked retired-ID root makes tombstone publication fail closed before
/// the live agent directory is detached.
#[cfg(unix)]
#[test]
fn symlinked_retired_root_prevents_agent_detach() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "unsafe-tombstone");
    let external = temp.path().join("external-retired");
    path_std_fs::create_dir(&external).expect("external retired dir");
    unix_fs::symlink(&external, temp.path().join(".agents.retired")).expect("retired root symlink");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.failures, 1);
    assert!(temp.path().join("agents/unsafe-tombstone").exists());
    assert_eq!(
        path_std_fs::read_dir(external)
            .expect("external dir")
            .count(),
        0
    );
}

/// A corrupt pre-existing tombstone never counts as durable publication and
/// therefore cannot precede detach.
#[test]
fn nonempty_existing_tombstone_prevents_agent_detach() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "corrupt-tombstone");
    let tombstone = tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id);
    path_std_fs::create_dir_all(tombstone.parent().expect("retired dir")).expect("retired dir");
    path_std_fs::write(&tombstone, b"corrupt").expect("corrupt tombstone");

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.failures, 1);
    assert!(temp.path().join("agents/corrupt-tombstone").exists());
}

/// The production managed existing-agent preparation retains the exclusive
/// agent lock that membership publication relies on to serialize with cleanup.
#[test]
fn production_existing_agent_load_retains_lock_until_release() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "loaded-agent");
    let owner = Arc::new(
        tau_core::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store =
        AgentStore::open_managed(temp.path().join("agents"), owner).expect("managed store");
    store
        .prepare_existing_agent(agent_id.as_str())
        .expect("prepare existing agent");
    let competing = path_std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.path().join("agents/loaded-agent/lock"))
        .expect("competing lock");

    assert!(
        competing.try_lock_exclusive().is_err(),
        "prepared existing agent must retain the lock through later membership publication"
    );
}

/// A tombstone sync failure leaves the live agent attached, and a later pass
/// validates and syncs the already-created empty tombstone before retrying.
#[test]
fn tombstone_sync_failure_is_retryable_before_detach() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "sync-retry");
    let injected = super::create_tombstone_with(
        &temp.path().join("agents"),
        &agent_id,
        super::create_new_tombstone,
        |_| Err(path_std_io::Error::other("injected tombstone sync failure")),
        |_, _| Ok(()),
    );
    assert!(injected.is_err());
    assert!(temp.path().join("agents/sync-retry").exists());

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.detached, 1);
    assert!(!temp.path().join("agents/sync-retry").exists());
}

/// Tombstone create-new failure cannot detach the live agent or publish a
/// partial reservation.
#[test]
fn tombstone_create_failure_precedes_detach() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "create-failure");
    let result = super::create_tombstone_with(
        &temp.path().join("agents"),
        &agent_id,
        |_| Err(path_std_io::Error::other("injected create-new failure")),
        |_| Ok(()),
        |_, _| Ok(()),
    );

    assert!(result.is_err());
    assert!(temp.path().join("agents/create-failure").exists());
    assert!(!tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());
}

/// A retired-directory sync failure leaves the live tree attached and can be
/// retried through the same validated empty tombstone.
#[test]
fn tombstone_directory_sync_failure_is_retryable_before_detach() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "dir-sync-retry");
    let injected = super::create_tombstone_with(
        &temp.path().join("agents"),
        &agent_id,
        super::create_new_tombstone,
        |_| Ok(()),
        |_, _| Err(path_std_io::Error::other("injected directory sync failure")),
    );
    assert!(injected.is_err());
    assert!(temp.path().join("agents/dir-sync-retry").exists());

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.detached, 1);
    assert!(!temp.path().join("agents/dir-sync-retry").exists());
}

/// The parent-directory publication sync is a distinct required cut after the
/// new retired root and tombstone have been synchronized.
#[test]
fn tombstone_parent_directory_sync_failure_is_retryable() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "parent-sync-retry");
    let state_root = temp.path().to_path_buf();
    let injected = super::create_tombstone_with(
        &temp.path().join("agents"),
        &agent_id,
        super::create_new_tombstone,
        |_| Ok(()),
        |path, _| {
            if path == state_root {
                Err(path_std_io::Error::other("injected parent sync failure"))
            } else {
                Ok(())
            }
        },
    );
    assert!(injected.is_err());
    assert!(temp.path().join("agents/parent-sync-retry").exists());

    let summary = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );

    assert_eq!(summary.detached, 1);
}

/// Rename failure occurs after durable tombstone publication but leaves the
/// live tree intact for a later cleanup retry.
#[test]
fn detach_rename_failure_leaves_live_tree_for_retry() {
    let temp = TempDir::new().expect("temp state");
    let agent_id = write_old_agent(temp.path(), "rename-retry");
    let mut before_rescan = |_: &AgentId| {};
    let mut tombstone =
        |agents_dir: &std::path::Path, id: &AgentId| super::create_tombstone(agents_dir, id);
    let mut rename = |_: &std::path::Path, _: &std::path::Path| {
        Err(path_std_io::Error::other("injected rename failure"))
    };
    let mut remove = |path: &std::path::Path| path_std_fs::remove_dir_all(path);
    let failed = super::cleanup_agents_with_hooks(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
        super::AgentCleanupHooks {
            before_rescan: &mut before_rescan,
            create_tombstone: &mut tombstone,
            rename: &mut rename,
            remove_dir: &mut remove,
        },
    );
    assert_eq!(failed.failures, 1);
    assert!(temp.path().join("agents/rename-retry").exists());
    assert!(tau_core::retired_agent_tombstone(&temp.path().join("agents"), &agent_id).exists());

    let retried = super::cleanup_agents(
        &temp.path().join("agents"),
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
    );
    assert_eq!(retried.detached, 1);
    assert!(!temp.path().join("agents/rename-retry").exists());
}

/// Recursive removal failure leaves only the detached staging tree; restart
/// finalization removes it without touching a replacement live directory.
#[test]
fn removal_failure_restart_preserves_replacement_live_directory() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "replacement-safe");
    let agents_dir = temp.path().join("agents");
    let mut injected = false;
    let mut before_rescan = |_: &AgentId| {};
    let mut tombstone =
        |agents_dir: &std::path::Path, id: &AgentId| super::create_tombstone(agents_dir, id);
    let mut rename = |source: &std::path::Path, destination: &std::path::Path| {
        path_std_fs::rename(source, destination)
    };
    let mut remove = |_detached: &std::path::Path| {
        injected = true;
        path_std_fs::create_dir_all(agents_dir.join("replacement-safe"))
            .expect("replacement live directory");
        path_std_fs::write(agents_dir.join("replacement-safe/replacement"), b"new")
            .expect("replacement marker");
        Err(path_std_io::Error::other(
            "injected recursive removal failure",
        ))
    };
    let summary = super::cleanup_agents_with_hooks(
        &agents_dir,
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
        super::AgentCleanupHooks {
            before_rescan: &mut before_rescan,
            create_tombstone: &mut tombstone,
            rename: &mut rename,
            remove_dir: &mut remove,
        },
    );
    assert!(injected);
    assert_eq!(summary.detached, 1);
    assert_eq!(summary.failures, 1);

    let finalized = super::finalize_detached_agents(&agents_dir);
    assert_eq!(finalized.removed, 1);
    assert_eq!(
        path_std_fs::read(agents_dir.join("replacement-safe/replacement")).expect("replacement"),
        b"new"
    );
}

/// A replacement installed at the candidate-rescan cut never inherits the old
/// locked directory's eligibility.
#[test]
fn replacement_during_rescan_survives_identity_revalidation() {
    let temp = TempDir::new().expect("temp state");
    write_old_agent(temp.path(), "rescan-swap");
    let agents_dir = temp.path().join("agents");
    let displaced = temp.path().join("displaced-agent");
    let mut swapped = false;

    let summary = super::cleanup_agents_with_before_rescan(
        &agents_dir,
        &temp.path().join("sessions"),
        Duration::from_secs(1),
        SystemTime::now() + Duration::from_secs(60),
        |_| {
            if !swapped {
                swapped = true;
                path_std_fs::rename(agents_dir.join("rescan-swap"), &displaced)
                    .expect("displace locked candidate");
                path_std_fs::create_dir_all(agents_dir.join("rescan-swap"))
                    .expect("replacement directory");
                path_std_fs::write(agents_dir.join("rescan-swap/replacement"), b"new")
                    .expect("replacement marker");
            }
        },
    );

    assert_eq!(summary.detached, 0);
    assert_eq!(
        path_std_fs::read(agents_dir.join("rescan-swap/replacement")).expect("replacement"),
        b"new"
    );
    assert!(displaced.exists());
}

/// A restart finalizer removal failure leaves staging present for the next
/// startup attempt.
#[test]
fn detached_staging_finalization_failure_is_retryable() {
    let temp = TempDir::new().expect("temp state");
    let agents_dir = temp.path().join("agents");
    let staging = temp.path().join(".agents.cleanup/stale");
    path_std_fs::create_dir_all(&staging).expect("staging");

    let failed = super::finalize_detached_agents_with(&agents_dir, |_| {
        Err(path_std_io::Error::other("injected finalizer failure"))
    });
    assert_eq!(failed.failures, 1);
    assert!(staging.exists());

    let retried = super::finalize_detached_agents(&agents_dir);
    assert_eq!(retried.removed, 1);
    assert!(!staging.exists());
}
