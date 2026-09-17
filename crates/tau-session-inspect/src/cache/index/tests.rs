//! Owner-private index replacement and key-retention tests.

use serde_json::{Value, json};

use super::*;
use crate::cache::exact_geometry::{request, response};

/// Builds request and response evidence through the same producer path used by
/// cache inspection, avoiding impossible synthetic fingerprint fixtures.
fn producer_evidence(key: &FingerprintKey) -> (ExactRequest, ExactResponse) {
    let capture = json!({
        "session_id": "session",
        "agent_id": "agent",
        "agent_prompt_id": "prompt",
        "backend": "responses",
        "transport": "http-sse",
        "model": "fixture-model",
        "attempt_id": "0123456789abcdef0123456789abcdef",
        "wire_dispatch_index": 1,
        "provider_response_id": "response",
        "body": {
            "input": [{"role": "user", "content": "private"}],
            "instructions": "private",
            "tools": [],
            "prompt_cache_key": "private",
            "previous_response_id": "response"
        }
    });
    (
        request(key, "instance", &capture).expect("request evidence"),
        response(key, "instance", &capture).expect("response evidence"),
    )
}

/// Rewrites one committed index value without changing its private mode.
fn mutate_committed_index(path: &Path, mutate: impl FnOnce(&mut Value)) {
    let mut value: Value =
        serde_json::from_slice(&std::fs::read(path).expect("read index")).expect("parse index");
    mutate(&mut value);
    std::fs::write(path, serde_json::to_vec(&value).expect("encode index")).expect("rewrite index");
}

/// A committed index is mode-private, reopens with the same key, and marks
/// loaded evidence as indexed without retaining source bodies.
#[test]
fn index_round_trip_reuses_only_the_same_private_index_key() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let key = state.key.0;
    let (request, response) = producer_evidence(&state.key);
    state
        .commit(
            "build",
            std::slice::from_ref(&request),
            std::slice::from_ref(&response),
        )
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
    assert!(loaded.responses[0].indexed);
}

/// Index replacement rejects unbounded selection metadata instead of
/// persisting malformed values as future filter authority.
#[test]
fn index_rejects_unbounded_selection_metadata() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let (mut request, _) = producer_evidence(&state.key);
    request.model = Some("x".repeat(129));
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

/// Loading fails closed when a private matching-build index contains structural
/// values that the evidence producer cannot emit.
#[test]
fn index_open_rejects_impossible_structural_evidence() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let (request, response) = producer_evidence(&state.key);

    let mutations: &[fn(&mut Value)] = &[
        |value| value["requests"][0]["adapter"] = "arbitrary".into(),
        |value| value["requests"][0]["request_form"] = "arbitrary".into(),
        |value| value["requests"][0]["body"] = "0".into(),
        |value| value["requests"][0]["tools"] = "A".repeat(64).into(),
        |value| value["responses"][0]["response"] = "0".into(),
        |value| value["responses"][0]["instance"] = "g".repeat(64).into(),
        |value| value["requests"][0]["prefixes"] = json!([]),
    ];
    for mutate in mutations {
        state
            .commit(
                "build",
                std::slice::from_ref(&request),
                std::slice::from_ref(&response),
            )
            .expect("restore valid index");
        mutate_committed_index(&path, mutate);
        assert_eq!(
            IndexState::open(&path, "build", 1024 * 1024)
                .err()
                .expect("reject malformed index"),
            "cache_index_malformed"
        );
    }
}

/// Commit applies the same structural validator before replacement, preserving
/// the prior valid index when proposed evidence is impossible.
#[test]
fn index_commit_rejects_impossible_structure_without_replacement() {
    let root = tempfile::tempdir().expect("index directory");
    let path = root.path().join("cache.index");
    let state = IndexState::open(&path, "build", 1024 * 1024).expect("fresh index");
    let (request, response) = producer_evidence(&state.key);
    state
        .commit(
            "build",
            std::slice::from_ref(&request),
            std::slice::from_ref(&response),
        )
        .expect("commit valid index");
    let before = std::fs::read(&path).expect("read valid index");

    let mut invalid_request = request.clone();
    invalid_request.adapter = "arbitrary".into();
    assert_eq!(
        state.commit(
            "build",
            std::slice::from_ref(&invalid_request),
            std::slice::from_ref(&response),
        ),
        Err("cache_index_malformed")
    );
    let mut invalid_response = response;
    invalid_response.response = "0".into();
    assert_eq!(
        state.commit(
            "build",
            std::slice::from_ref(&request),
            std::slice::from_ref(&invalid_response),
        ),
        Err("cache_index_malformed")
    );
    assert_eq!(std::fs::read(&path).expect("read retained index"), before);
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
