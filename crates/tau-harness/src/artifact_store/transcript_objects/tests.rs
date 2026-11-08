use std::os::unix::fs::{MetadataExt, symlink};

use super::*;
use crate::artifact_store::tests::put;

/// Reproduces the audited 4325782/f9bb9dda ordinary-object cleanup path.
///
/// Deliberately does not call the new metadata fallback or root-aware cleaner:
/// old cleaners traverse only canonical `blake3`, not arbitrary root siblings.
fn legacy_cleanup(store: &ArtifactStore, now: u64) {
    let _lock = store.lock(true).expect("legacy coordination");
    for entry in fs::read_dir(store.root.join("blake3")).expect("objects") {
        let entry = entry.expect("object");
        let Ok(_) = ArtifactKey::from_digest(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let metadata: Metadata =
            read_json(&entry.path().join("meta.json")).expect("legacy metadata");
        if metadata.version == 1 && now.saturating_sub(metadata.last_put_at) >= 1 {
            store
                .detach(&entry.path())
                .expect("legacy detach and unlink");
        }
    }
}

/// Publishes a protected fixture without any new canonical reference event.
fn rooted_fixture() -> (tempfile::TempDir, ArtifactStore, ArtifactDescriptor) {
    rooted_fixture_with_identity(tau_proto::ArtifactIdentity::anonymous())
}

/// Reconstructs a historical object without routing a legacy identity through
/// new-upload admission, which only permits metadata-bound writes.
fn rooted_fixture_with_identity(
    identity: tau_proto::ArtifactIdentity,
) -> (tempfile::TempDir, ArtifactStore, ArtifactDescriptor) {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    fs::create_dir_all(temp.path().join("agents/history")).expect("agent");
    let descriptor = if identity == tau_proto::ArtifactIdentity::LegacyPayload {
        let bytes = b"prepared pixels";
        let descriptor = ArtifactDescriptor::from_bytes(bytes, identity.clone()).unwrap();
        let _lock = store.lock(true).unwrap();
        let path = store
            .root
            .join("blake3")
            .join(identity.digest(bytes).to_hex().as_str());
        private_dir(&path).unwrap();
        write_synced(&path.join("data"), bytes).unwrap();
        write_json(
            &path.join("meta.json"),
            &Metadata {
                version: 1,
                size: bytes.len() as u64,
                last_put_at: 1,
                identity,
            },
        )
        .unwrap();
        descriptor
    } else {
        put(&mut store, "tool", "connection", b"prepared pixels", 1).1
    };
    store
        .retain_transcript_artifact(
            &tau_proto::AgentId::parse("history").expect("agent id"),
            &descriptor,
        )
        .expect("root");
    (temp, store, descriptor)
}

/// Canonical unlink by an old cleaner preserves the inode and exact-key reads.
#[test]
fn legacy_cleanup_preserves_hardlink_and_private_read_fallback() {
    let (_temp, mut store, descriptor) =
        rooted_fixture_with_identity(tau_proto::ArtifactIdentity::LegacyPayload);
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let canonical = store.root.join("blake3").join(&digest).join("data");
    let protected = store
        .root
        .join("transcript-objects")
        .join(&digest)
        .join("data");
    assert_eq!(
        fs::metadata(&canonical).expect("ordinary").ino(),
        fs::metadata(&protected).expect("protected").ino()
    );
    legacy_cleanup(&store, 100);
    assert!(!canonical.exists());
    assert!(protected.exists());
    assert_eq!(
        store.descriptor(&descriptor.key).expect("fallback"),
        descriptor
    );
    let ArtifactValue::Opened {
        read,
        descriptor: opened,
    } = store
        .open("provider", descriptor.key.clone())
        .expect("open")
    else {
        panic!("opened");
    };
    assert_eq!(opened, descriptor);
    let ArtifactValue::Chunk { bytes, eof, .. } =
        store.read("provider", &read, 0, 100).expect("read")
    else {
        panic!("chunk");
    };
    assert!(eof);
    verify(&descriptor, &bytes).expect("consumer verifies bytes");
    store
        .execute("provider", "provider", ArtifactOp::Close { read }, 100)
        .expect("close");
    assert_eq!(
        store.list("", 10, || true).expect("ordinary discovery"),
        ArtifactValue::Listed { entries: vec![] }
    );
    store
        .retain_transcript_artifact(
            &tau_proto::AgentId::parse("history").expect("agent"),
            &descriptor,
        )
        .expect("new retained occurrence uses protected backing");
}

/// Explicit duplicate puts preserve immutable backing and age; retry is not a
/// put.
#[test]
fn duplicate_put_and_receipt_retry_never_mutate_backing() {
    let (_temp, mut store, descriptor) = rooted_fixture();
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let protected = store.root.join("transcript-objects").join(&digest);
    let original_inode = fs::metadata(protected.join("data")).expect("data").ino();
    let (upload, duplicate) = put(&mut store, "tool", "connection", b"prepared pixels", 20);
    assert_eq!(duplicate, descriptor);
    store
        .execute("tool", "connection", ArtifactOp::Finalize { upload }, 90)
        .expect("completed receipt retry");
    let metadata: Metadata = read_json(&protected.join("meta.json")).expect("metadata");
    assert_eq!(metadata.last_put_at, 20);
    assert_eq!(
        fs::metadata(protected.join("data")).expect("data").ino(),
        original_inode
    );
    verify(
        &descriptor,
        &read_bounded(&protected.join("data")).expect("bytes"),
    )
    .expect("immutable");
    {
        let _lock = store.lock(true).unwrap();
        store
            .detach(&store.root.join("blake3").join(&digest))
            .unwrap();
    }
    let (_, restored) = put(&mut store, "tool", "connection", b"prepared pixels", 110);
    assert_eq!(restored, descriptor);
    assert_eq!(
        fs::metadata(store.root.join("blake3").join(digest).join("data"))
            .expect("canonical")
            .ino(),
        original_inode
    );
    assert_eq!(
        store.metadata(&descriptor.key).expect("age").last_put_at,
        110
    );
}

/// An independent old-style reupload can make a new inode without harming
/// roots.
#[test]
fn legacy_reupload_and_metadata_renewal_preserve_protected_bytes() {
    let (_temp, store, descriptor) =
        rooted_fixture_with_identity(tau_proto::ArtifactIdentity::LegacyPayload);
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let canonical = store.root.join("blake3").join(&digest);
    let protected = store.root.join("transcript-objects").join(&digest);
    let original_inode = fs::metadata(protected.join("data")).expect("data").ino();
    // Audited old duplicate publication verifies bytes and replaces only
    // metadata.
    {
        let _lock = store.lock(true).expect("legacy writer");
        verify(
            &descriptor,
            &read_bounded(&canonical.join("data")).expect("bytes"),
        )
        .expect("old duplicate verifies");
        replace_json(
            &canonical.join("meta.json"),
            &Metadata {
                identity: tau_proto::ArtifactIdentity::LegacyPayload,
                version: 1,
                size: descriptor.size.get(),
                last_put_at: 20,
            },
        )
        .expect("old metadata renewal");
    }
    assert_eq!(
        fs::metadata(protected.join("data")).expect("data").ino(),
        original_inode
    );
    legacy_cleanup(&store, 100);
    // The old missing-object branch writes a fresh staging inode, then renames.
    {
        let _lock = store.lock(true).expect("legacy writer");
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(store.root.join("blake3"))
            .expect("staging");
        write_synced(&staging.path().join("data"), b"prepared pixels").expect("new inode");
        write_json(
            &staging.path().join("meta.json"),
            &Metadata {
                identity: tau_proto::ArtifactIdentity::LegacyPayload,
                version: 1,
                size: descriptor.size.get(),
                last_put_at: 110,
            },
        )
        .expect("new metadata");
        sync_dir(staging.path()).expect("stage sync");
        fs::rename(staging.path(), &canonical).expect("publish");
        sync_dir(&store.root.join("blake3")).expect("parent sync");
    }
    assert_ne!(
        fs::metadata(canonical.join("data")).expect("data").ino(),
        original_inode
    );
    assert_eq!(
        fs::metadata(protected.join("data")).expect("data").ino(),
        original_inode
    );
    verify(
        &descriptor,
        &read_bounded(&protected.join("data")).expect("bytes"),
    )
    .expect("immutable");
}

/// A damaged protected alias must reject new roots even if the canonical copy
/// works.
#[test]
fn corrupt_protected_alias_cannot_be_certified_by_ordinary_bytes() {
    let (_temp, store, descriptor) = rooted_fixture();
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let protected = store
        .root
        .join("transcript-objects")
        .join(&digest)
        .join("data");
    // Replace rather than mutate the shared inode, leaving canonical bytes
    // valid.
    fs::remove_file(&protected).expect("unlink alias");
    fs::write(&protected, b"wrong protected").expect("independent corrupt alias");
    assert_eq!(
        store.retain_transcript_artifact(
            &tau_proto::AgentId::parse("history").expect("agent"),
            &descriptor
        ),
        Err(ArtifactError::Integrity)
    );
}

/// Backing publication without a committed root is a reclaimable orphan.
#[test]
fn backing_without_root_obeys_age_policy_and_disabled_retention() {
    let temp = tempfile::tempdir().expect("state");
    let mut store = ArtifactStore::new(temp.path());
    let (_, descriptor) = put(&mut store, "tool", "connection", b"orphan", 1);
    {
        let _lock = store.lock(true).expect("exclusive preparation");
        store
            .protect_transcript_object(&descriptor)
            .expect("backing");
        // Simulated cancellation/crash before manifest publication.
    }
    store.cleanup(None, 100).expect("retention disabled");
    assert!(store.descriptor(&descriptor.key).is_ok());
    store
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("age cleanup");
    assert_eq!(
        store.descriptor(&descriptor.key),
        Err(ArtifactError::Unavailable)
    );
}

/// Neither a symlink nor corrupt size can be certified as prepared durable
/// bytes.
#[test]
fn backing_rejects_symlink_and_wrong_size_metadata() {
    let (_temp, store, descriptor) = rooted_fixture();
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let protected = store.root.join("transcript-objects").join(&digest);
    let data = protected.join("data");
    fs::remove_file(&data).expect("unlink");
    symlink(store.root.join("blake3").join(&digest).join("data"), &data).expect("symlink");
    assert!(store.protect_transcript_object(&descriptor).is_err());
    fs::remove_file(&data).expect("unlink");
    fs::hard_link(store.root.join("blake3").join(digest).join("data"), &data)
        .expect("restore link");
    replace_json(
        &protected.join("meta.json"),
        &Metadata {
            identity: tau_proto::ArtifactIdentity::LegacyPayload,
            version: 1,
            size: 1,
            last_put_at: 1,
        },
    )
    .expect("wrong metadata size");
    assert_eq!(
        store.protect_transcript_object(&descriptor),
        Err(ArtifactError::Integrity)
    );
}

/// An interrupted finalization retries metadata publication without changing
/// bytes.
#[test]
fn incomplete_receipt_recovery_preserves_protected_inode() {
    let (_temp, mut store, descriptor) = rooted_fixture();
    let digest = store
        .storage_digest(&descriptor.key)
        .expect("storage digest");
    let protected = store
        .root
        .join("transcript-objects")
        .join(&digest)
        .join("data");
    let inode = fs::metadata(&protected).expect("data").ino();
    let (upload, _) = put(&mut store, "tool", "connection", b"prepared pixels", 20);
    let operation = store.root.join("operations").join(upload.as_str());
    let mut receipt: Receipt = read_json(&operation.join("receipt.json")).expect("receipt");
    receipt.complete = false;
    replace_json(&operation.join("receipt.json"), &receipt).expect("interrupted intent");
    fs::write(operation.join("data"), b"prepared pixels").expect("recoverable bytes");
    {
        let _lock = store.lock(true).unwrap();
        store
            .detach(&store.root.join("blake3").join(&digest))
            .unwrap();
    }
    store
        .execute("tool", "connection", ArtifactOp::Finalize { upload }, 200)
        .expect("recover");
    assert_eq!(
        store.metadata(&descriptor.key).expect("age").last_put_at,
        20
    );
    assert_eq!(fs::metadata(&protected).expect("data").ino(), inode);
    verify(&descriptor, &read_bounded(&protected).expect("bytes")).expect("verified");
}
