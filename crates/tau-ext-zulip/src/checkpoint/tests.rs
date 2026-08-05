use std::{fs, io};

use tau_proto::MessageFactId;

use super::{CheckpointRuntime, CheckpointStore};

/// Corrupt state must stop catch-up rather than silently reset and skip an
/// offline gap.
#[test]
fn corrupt_checkpoint_is_rejected() {
    let directory = tempfile::tempdir().expect("state directory");
    let store = CheckpointStore::open(directory.path(), &[3; 32]).expect("store");
    fs::write(&store.path, b"{not-json").expect("corrupt checkpoint");
    assert_eq!(
        store.load().expect_err("corruption must fail").kind(),
        io::ErrorKind::InvalidData
    );
}

/// Atomic writes must round-trip the newest acknowledged native position.
#[test]
fn checkpoint_round_trips_after_atomic_replace() {
    let directory = tempfile::tempdir().expect("state directory");
    let store = CheckpointStore::open(directory.path(), &[4; 32]).expect("store");
    assert_eq!(store.load().expect("empty state"), None);
    store.store(41).expect("first checkpoint");
    store.store(99).expect("replacement checkpoint");
    assert_eq!(store.load().expect("saved state"), Some(99));
}

/// One identity namespace must reject concurrent processes so two pollers
/// cannot race checkpoint advancement.
#[test]
fn identity_namespace_has_one_owner() {
    let directory = tempfile::tempdir().expect("state directory");
    let _owner = CheckpointStore::open(directory.path(), &[5; 32]).expect("owner");
    assert!(CheckpointStore::open(directory.path(), &[5; 32]).is_err());
    assert!(CheckpointStore::open(directory.path(), &[6; 32]).is_ok());
}

/// Namespace paths must be secret-derived opaque digests rather than raw
/// identity-key material.
#[test]
fn namespace_path_does_not_expose_identity_secret() {
    let directory = tempfile::tempdir().expect("state directory");
    let key = *b"raw-identity-secret-32-bytes!!!!";
    let store = CheckpointStore::open(directory.path(), &key).expect("store");
    assert!(!store.path.to_string_lossy().contains("raw-identity-secret"));
}

/// A failed report remains the ordered-prefix barrier even when a later
/// filtered message completes, then advances both only after retry and echo.
#[test]
fn retry_barrier_cannot_be_skipped_by_later_completion() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut runtime = CheckpointRuntime::open(directory.path(), &[8; 32]).expect("runtime");
    assert!(runtime.begin(10));
    runtime.retry(10);
    assert!(runtime.begin(11));
    runtime.filtered(11);
    runtime.advance().expect("no-op advancement");
    assert_eq!(runtime.position(), None);

    assert!(runtime.begin(10));
    let fact_id = MessageFactId::new("fact-10");
    runtime.submitted(10, fact_id.clone());
    assert!(runtime.acknowledge(&fact_id));
    runtime.advance().expect("advance retried prefix");
    assert_eq!(runtime.position(), Some(11));
}

/// A failed atomic replacement must retain the completed prefix in memory so a
/// later retry persists it rather than skipping or forgetting the position.
#[test]
fn checkpoint_write_failure_retries_same_completed_prefix() {
    let directory = tempfile::tempdir().expect("state directory");
    let mut runtime = CheckpointRuntime::open(directory.path(), &[10; 32]).expect("runtime");
    assert!(runtime.begin(7));
    runtime.filtered(7);
    fs::remove_dir_all(directory.path()).expect("remove state directory");
    assert!(runtime.advance().is_err());
    assert_eq!(runtime.position(), None);
    fs::create_dir_all(directory.path()).expect("restore state directory");
    runtime.advance().expect("retry checkpoint");
    assert_eq!(runtime.position(), Some(7));
}
