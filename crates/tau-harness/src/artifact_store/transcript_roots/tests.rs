use std::process::Command;

use super::*;
use crate::artifact_store::tests::put;

/// Creates only the live-directory prerequisite; no canonical refs exist yet.
fn owner(store: &ArtifactStore, name: &str) -> AgentId {
    let id = AgentId::parse(name).expect("owner");
    fs::create_dir_all(
        store
            .root
            .parent()
            .expect("state")
            .join("agents")
            .join(name),
    )
    .expect("live agent directory");
    id
}

/// Returns the fixture's canonical per-transcript manifest path.
fn manifest(store: &ArtifactStore, id: &AgentId) -> PathBuf {
    store
        .root
        .join("transcript-roots")
        .join(id.as_str())
        .join("roots.json")
}

/// Legacy and new roots survive restart, preserve all historical additions,
/// and never renew age.
#[test]
fn roots_are_a_durable_union_without_age_renewal() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "history");
    let (_, first) = put(&mut store, "tool", "connection", b"old branch", 1);
    let (_, second) = put(&mut store, "tool", "connection", b"replacement window", 2);
    let (_, textual) = put(&mut store, "tool", "connection", b"textual link", 3);
    for descriptor in [&first, &second, &first] {
        store
            .retain_transcript_artifact(&id, descriptor)
            .expect("durable protection");
    }
    let roots = read_roots(manifest(&store, &id).parent().expect("owner"), &id).expect("roots");
    assert_eq!(roots.artifacts.len(), 2);
    assert_eq!(store.metadata(&first.key).expect("metadata").last_put_at, 1);
    drop(store);
    let restarted = ArtifactStore::new(temp.path());
    restarted
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("root-aware cleanup");
    assert!(restarted.descriptor(&first.key).is_ok());
    assert!(restarted.descriptor(&second.key).is_ok());
    assert_eq!(
        restarted.descriptor(&textual.key),
        Err(ArtifactError::Unavailable)
    );
}

/// One owner's removal cannot release a shared object's other transcript owner.
#[test]
fn shared_bytes_remain_until_all_transcript_roots_are_released() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let first = owner(&store, "first");
    let second = owner(&store, "second");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"same pixels", 1);
    for id in [&first, &second] {
        store
            .retain_transcript_artifact(id, &descriptor)
            .expect("root");
    }
    // Storage-only primitive: production calls it only after durable deletion.
    store.release_deleted_transcript(&first).expect("release");
    store
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("cleanup");
    assert!(store.descriptor(&descriptor.key).is_ok());
    store.release_deleted_transcript(&second).expect("release");
    store
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("cleanup");
    assert_eq!(
        store.descriptor(&descriptor.key),
        Err(ArtifactError::Unavailable)
    );
}

/// A failed append or final sync may leak protection, but never promises
/// success.
#[test]
fn interrupted_publication_and_retry_preserve_orphan_protection() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "interrupted");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
    assert_eq!(
        store.retain_transcript_artifact_with(&id, &descriptor, |_| Err(ArtifactError::Io)),
        Err(ArtifactError::Io)
    );
    drop(store);
    let restarted = ArtifactStore::new(temp.path());
    restarted
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("orphan remains rooted");
    assert_eq!(
        restarted
            .retain_transcript_artifact(&id, &descriptor)
            .expect("retry establishes all syncs"),
        b"prepared"
    );
    // No journal append follows; root remains intentionally conservative.
    restarted
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("failed append cannot unpin");
    assert!(restarted.descriptor(&descriptor.key).is_ok());
}

/// Descriptor equality and full digest verification precede root publication.
#[test]
fn missing_corrupt_and_wrong_size_objects_cannot_acquire_roots() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "invalid");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"image", 1);
    let wrong = ArtifactDescriptor::new(descriptor.key.clone(), 2).expect("descriptor");
    assert_eq!(
        store.retain_transcript_artifact(&id, &wrong),
        Err(ArtifactError::Integrity)
    );
    let data = store
        .root
        .join("blake3")
        .join(
            store
                .storage_digest(&descriptor.key)
                .expect("storage digest")
                .as_hex(),
        )
        .join("data");
    fs::write(&data, b"other").expect("same-size corruption");
    assert_eq!(
        store.retain_transcript_artifact(&id, &descriptor),
        Err(ArtifactError::Integrity)
    );
    fs::remove_file(data).expect("missing object");
    assert_eq!(
        store.retain_transcript_artifact(&id, &descriptor),
        Err(ArtifactError::Unavailable)
    );
    assert!(!manifest(&store, &id).exists());
}

/// Corrupt or incomplete authority cannot turn any rooted object into GC input.
#[test]
fn malformed_missing_and_duplicate_root_metadata_fail_closed() {
    for damage in [
        "missing",
        "json",
        "version",
        "owner",
        "duplicate",
        "oversized",
    ] {
        let temp = tempfile::tempdir().expect("state");
        let mut store = ArtifactStore::new(temp.path());
        let id = owner(&store, "protected");
        let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
        store
            .retain_transcript_artifact(&id, &descriptor)
            .expect("root");
        let path = manifest(&store, &id);
        match damage {
            "missing" => fs::remove_file(&path).expect("remove"),
            "json" => fs::write(&path, b"broken").expect("corrupt"),
            "oversized" => File::options()
                .write(true)
                .open(&path)
                .expect("manifest")
                .set_len(MANIFEST_MAX_BYTES + 1)
                .expect("oversize"),
            "duplicate" => fs::write(
                &path,
                format!(
                    r#"{{"version":1,"agent_id":"protected","artifacts":{{"{}":8,"{}":8}}}}"#,
                    descriptor.key, descriptor.key
                ),
            )
            .expect("duplicate keys"),
            field => {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).expect("manifest")).expect("json");
                if field == "version" {
                    json["version"] = 2.into();
                } else {
                    json["agent_id"] = "different".into();
                }
                fs::write(&path, serde_json::to_vec(&json).expect("encode")).expect("replace");
            }
        }
        let mut reports = Vec::new();
        assert!(
            store
                .cleanup_reporting(Some(Duration::from_secs(1)), 100, |path| {
                    reports.push(path.to_owned());
                })
                .is_err(),
            "{damage}"
        );
        assert_eq!(reports.len(), 1, "{damage}");
        assert!(store.descriptor(&descriptor.key).is_ok(), "{damage}");
    }
}

/// The registry marker detects whole-directory loss; scan exhaustion is not
/// empty.
#[test]
fn missing_registry_and_exhausted_scan_preserve_objects() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "protected");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
    store
        .retain_transcript_artifact(&id, &descriptor)
        .expect("root");
    assert_eq!(
        store.scan_transcript_roots(
            &HashSet::from([descriptor.key.clone()]),
            &mut |_| {},
            Duration::ZERO
        ),
        Err(ArtifactError::Busy)
    );
    fs::remove_dir_all(store.root.join("transcript-roots")).expect("lost registry");
    assert!(store.cleanup(Some(Duration::from_secs(1)), 100).is_err());
    assert!(store.descriptor(&descriptor.key).is_ok());
}

/// Another OS process cannot clean between object verification and root
/// durability.
#[test]
fn root_publication_excludes_cross_process_cleanup() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "protected");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
    let child = |busy: &str| {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "artifact_store::tests::artifact_coordination_child",
                "--nocapture",
            ])
            .env("TAU_ARTIFACT_TEST_ROOT", temp.path())
            .env("TAU_ARTIFACT_TEST_BUSY", busy)
            .output()
            .expect("cleanup process");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    };
    store
        .retain_transcript_artifact_with(&id, &descriptor, |registry| {
            child("yes");
            sync_dir(registry)
        })
        .expect("protected publication");
    child("no");
    assert!(store.descriptor(&descriptor.key).is_ok());
}

/// Overflow is a failed establishment, never permission to prune old history.
#[test]
fn manifest_bound_rejects_without_mutating_union() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "bounded");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
    store
        .retain_transcript_artifact(&id, &descriptor)
        .expect("root");
    let roots = read_roots(manifest(&store, &id).parent().expect("owner"), &id).expect("roots");
    let encoded = serde_json::to_vec(&roots).expect("encode");
    assert_eq!(check_manifest_size(&roots, encoded.len() as u64), Ok(()));
    assert_eq!(
        check_manifest_size(&roots, encoded.len() as u64 - 1),
        Err(ArtifactError::Invalid)
    );
    assert_eq!(roots.artifacts.len(), 1);
}

/// An already retired agent cannot reacquire roots after history deletion.
#[test]
fn retired_transcript_cannot_reacquire_protection() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let id = owner(&store, "retired");
    let (_, descriptor) = put(&mut store, "tool", "connection", b"prepared", 1);
    let tombstone = tau_core::retired_agent_tombstone(&temp.path().join("agents"), &id);
    fs::create_dir_all(tombstone.parent().expect("parent")).expect("retired directory");
    fs::write(tombstone, []).expect("retired id");
    assert_eq!(
        store.retain_transcript_artifact(&id, &descriptor),
        Err(ArtifactError::Unavailable)
    );
    assert!(!manifest(&store, &id).exists());
}
