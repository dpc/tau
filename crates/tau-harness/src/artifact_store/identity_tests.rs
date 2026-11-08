use super::*;

/// Drives the real client uploader so store fixtures exercise metadata
/// admission and the producer's exact identity check rather than bypassing the
/// protocol.
fn upload(store: &mut ArtifactStore, filename: &str, mime: &str, now: u64) -> ArtifactDescriptor {
    let mut upload = tau_client::ArtifactUpload::with_metadata(
        b"same payload".to_vec(),
        Some(filename.into()),
        Some(mime.into()),
    )
    .unwrap();
    while let Some(op) = upload.next_op() {
        upload
            .accept(store.execute("owner", "connection", op, now).unwrap())
            .unwrap();
    }
    upload.descriptor().unwrap().clone()
}

/// Metadata survives deduplication, restart, list/stat, protected-only reads
/// and receipt retries; changed metadata never aliases equal payload bytes.
#[test]
fn immutable_metadata_roundtrip_restart_and_protected_reads() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = ArtifactStore::new(temp.path());
    let first = upload(&mut store, "example.txt", "text/plain", 1);
    let renamed = upload(&mut store, "other.txt", "text/plain", 2);
    let retyped = upload(&mut store, "example.txt", "application/x-test", 3);
    assert_ne!(first.key, renamed.key);
    assert_ne!(first.key, retyped.key);
    assert_eq!(first, upload(&mut store, "example.txt", "text/plain", 4));
    assert_eq!(store.metadata(&first.key).unwrap().last_put_at, 4);
    fs::create_dir_all(temp.path().join("agents/history")).unwrap();
    store
        .retain_transcript_artifact(&tau_proto::AgentId::parse("history").unwrap(), &first)
        .unwrap();
    let digest = store.storage_digest(&first.key).unwrap();
    fs::remove_dir_all(store.root.join("blake3").join(&digest)).unwrap();
    drop(store);
    let mut store = ArtifactStore::new(temp.path());
    assert_eq!(store.descriptor(&first.key).unwrap(), first);
    assert_eq!(store.descriptor(&renamed.key).unwrap(), renamed);
    let mut download = tau_client::ArtifactDownload::new(first.key.clone());
    while let Some(op) = download.next_op() {
        download
            .accept(store.execute("owner", "connection", op, 5).unwrap())
            .unwrap();
    }
    assert_eq!(download.descriptor(), Some(&first));
    assert_eq!(download.into_bytes().unwrap(), b"same payload");
    let ArtifactValue::Listed { entries } = store.list("", 10, || true).unwrap() else {
        panic!("list")
    };
    assert!(entries.iter().any(|entry| entry.descriptor == renamed));
    assert!(entries.iter().any(|entry| entry.descriptor == retyped));
    let path = store
        .root
        .join("transcript-objects")
        .join(digest)
        .join("meta.json");
    let mut metadata: Metadata = read_json(&path).unwrap();
    metadata.identity =
        tau_proto::ArtifactIdentity::metadata(Some("tampered".into()), Some("text/plain".into()))
            .unwrap();
    replace_json(&path, &metadata).unwrap();
    assert_eq!(
        store.protect_transcript_object(&first),
        Err(ArtifactError::Integrity)
    );
    let changed = store.descriptor(&first.key).unwrap();
    assert_eq!(
        changed.verify(b"same payload"),
        Err(ArtifactError::Integrity)
    );
}
