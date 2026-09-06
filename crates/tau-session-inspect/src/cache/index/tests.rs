//! Owner-private index replacement and key-retention tests.

use super::*;

/// A committed index is mode-private, reopens with the same key, and marks
/// loaded evidence as indexed without retaining source bodies.
#[test]
fn index_round_trip_reuses_only_the_same_private_index_key() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let key = state.key.0;
    let request = ExactRequest {
        session: "session".into(),
        agent: "agent".into(),
        prompt: "prompt".into(),
        instance: "i".repeat(64),
        attempt: Some("a".repeat(64)),
        dispatch: Some(1),
        adapter: "responses".into(),
        body: "b".repeat(64),
        instructions: None,
        tools: "t".repeat(64),
        controls: "c".repeat(64),
        other: "o".repeat(64),
        route: "r".repeat(64),
        cache_key: None,
        previous_response: None,
        items: vec!["x".repeat(64)],
        prefixes: vec!["p".repeat(64)],
        complete: true,
        indexed: false,
        recorded_at_unix_micros: Some(1),
        request_form: Some("full".into()),
        model: Some("fixture-model".into()),
        operation: Some("inference".into()),
        attempt_ordinal: Some(1),
    };
    state
        .commit("build", std::slice::from_ref(&request), &[])
        .expect("commit private index");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            path.metadata().expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
    let loaded = IndexState::open(&path, "build", 1024 * 1024).expect("reopen index");
    assert_eq!(loaded.key.0, key);
    assert!(loaded.requests[0].indexed);
}

/// Index replacement rejects unbounded selection metadata instead of
/// persisting malformed values as future filter authority.
#[test]
fn index_rejects_unbounded_selection_metadata() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let mut request = ExactRequest {
        session: "session".into(),
        agent: "agent".into(),
        prompt: "prompt".into(),
        instance: "i".repeat(64),
        attempt: Some("a".repeat(64)),
        dispatch: Some(1),
        adapter: "responses".into(),
        body: "b".repeat(64),
        instructions: None,
        tools: "t".repeat(64),
        controls: "c".repeat(64),
        other: "o".repeat(64),
        route: "r".repeat(64),
        cache_key: None,
        previous_response: None,
        items: Vec::new(),
        prefixes: Vec::new(),
        complete: true,
        indexed: false,
        recorded_at_unix_micros: Some(1),
        request_form: Some("full".into()),
        model: Some("x".repeat(129)),
        operation: Some("inference".into()),
        attempt_ordinal: Some(1),
    };
    assert_eq!(
        state.commit("build", std::slice::from_ref(&request), &[]),
        Err("cache_index_malformed")
    );
    request.model = Some("model".into());
    request.operation = Some("arbitrary".into());
    assert_eq!(
        state.commit("build", &[request], &[]),
        Err("cache_index_malformed")
    );
    assert!(!path.exists());
}

/// Shared permissions and symlink substitution fail closed rather than loading
/// or replacing a secret equality key.
#[cfg(unix)]
#[test]
fn index_rejects_shared_permissions_and_symlinks() {
    use std::fs::Permissions;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    std::fs::write(&path, b"{}").expect("fixture");
    std::fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("shared mode");
    assert_eq!(
        IndexState::open(&path, "build", 1024 * 1024)
            .err()
            .expect("reject shared file"),
        "cache_index_not_private"
    );
    std::fs::remove_file(&path).expect("remove fixture");
    symlink(root.path().join("missing"), &path).expect("symlink fixture");
    assert_eq!(
        IndexState::open(&path, "build", 1024 * 1024)
            .err()
            .expect("reject symlink"),
        "cache_index_unreadable"
    );
}
