//! Compatibility tests use pre-migration disk layouts, not a second store.

use super::*;

/// Existing full-digest object paths remain readable without renaming bytes.
/// Short-key receipts survive restart without renewing age.
#[test]
fn full_digest_objects_and_short_key_receipts_survive_restart() {
    let root = tempfile::tempdir().expect("root");
    let mut store = ArtifactStore::new(root.path());
    let bytes = b"pre-migration bytes";
    let full = blake3::hash(bytes).to_hex();
    let short = ArtifactKey::from_digest(full.as_str()).expect("short");
    let object = store.root.join("blake3").join(full.as_str());
    private_dir(&object).expect("legacy object");
    write_synced(&object.join("data"), bytes).expect("original");
    write_json(
        &object.join("meta.json"),
        &serde_json::json!({"version": 1, "size": bytes.len(), "last_put_at": 1}),
    )
    .expect("legacy metadata");
    let mut download = tau_client::ArtifactDownload::new(short.clone());
    while let Some(op) = download.next_op() {
        download
            .accept(store.execute("owner", "connection", op, 2).expect("read"))
            .expect("verify");
    }
    assert_eq!(download.into_bytes().expect("original"), bytes);
    let (upload, descriptor) = tests::put(&mut store, "owner", "connection", bytes, 3);
    assert_ne!(descriptor.key, short);
    drop(store);
    let mut restarted = ArtifactStore::new(root.path());
    assert_eq!(
        restarted
            .finalize("owner", "connection", &upload, 4)
            .expect("old retry"),
        ArtifactValue::Descriptor(descriptor.clone())
    );
    let ArtifactValue::Listed { entries } = restarted.list("", 10, || true).expect("list") else {
        panic!("list");
    };
    assert_eq!(entries[0].descriptor, descriptor);
    assert_eq!(entries[1].descriptor.key, short);
    assert_eq!(entries[0].last_put_at, 3);
    for prefix in ["b", "0", "f"] {
        let ArtifactValue::Listed { entries } =
            restarted.list(prefix, 10, || true).expect("hex prefix")
        else {
            panic!("list");
        };
        assert_eq!(
            entries.len(),
            usize::from(short.as_str().starts_with(prefix))
                + usize::from(descriptor.key.as_str().starts_with(prefix))
        );
    }
}

/// A removed full-key receipt fails closed on retry without rewriting either
/// the receipt or the immutable original.
#[test]
fn historical_full_key_receipt_is_rejected_without_store_migration() {
    let root = tempfile::tempdir().expect("root");
    let mut store = ArtifactStore::new(root.path());
    let bytes = b"historical original";
    let (upload, descriptor) = tests::put(&mut store, "owner", "connection", bytes, 1);
    let path = store
        .root
        .join("operations")
        .join(upload.as_str())
        .join("receipt.json");
    let mut receipt: serde_json::Value = read_json(&path).expect("receipt");
    receipt["descriptor"]["key"] =
        serde_json::json!(format!("blake3:{}", blake3::hash(bytes).to_hex()));
    replace_json(&path, &receipt).expect("historical receipt");
    drop(store);
    let mut restarted = ArtifactStore::new(root.path());
    assert_eq!(
        restarted.finalize("owner", "connection", &upload, 2),
        Err(ArtifactError::Integrity)
    );
    assert_eq!(
        read_json::<serde_json::Value>(&path).expect("unchanged receipt"),
        receipt
    );
    assert_eq!(
        restarted
            .descriptor(&descriptor.key)
            .expect("original still accessible"),
        descriptor
    );
}

/// A simulated collision must fail reads and new publication without touching
/// either object; ordinary/protected aliases of one full digest are not a
/// clash.
#[test]
fn short_identity_collision_never_selects_or_overwrites_an_object() {
    let root = tempfile::tempdir().expect("root");
    let mut store = ArtifactStore::new(root.path());
    let bytes = b"immutable";
    let (_, descriptor) = tests::put(&mut store, "owner", "connection", bytes, 1);
    store
        .protect_transcript_object(&descriptor)
        .expect("same-digest alias");
    assert!(store.descriptor(&descriptor.key).is_ok());
    let full = descriptor.identity.digest(bytes).to_hex();
    let different = format!("{}{}", descriptor.key, "0".repeat(40));
    assert_ne!(different, full.as_str());
    let collision = store.root.join("transcript-objects").join(&different);
    private_dir(&collision).expect("simulated collision");
    write_synced(&collision.join("data"), b"other").expect("other bytes");
    assert_eq!(
        store.descriptor(&descriptor.key),
        Err(ArtifactError::Integrity)
    );
    assert_eq!(store.list("", 10, || true), Err(ArtifactError::Integrity));
    let mut upload = tau_client::ArtifactUpload::new(bytes.to_vec()).expect("upload");
    while let Some(op) = upload.next_op() {
        match store.execute("owner", "second", op, 20) {
            Ok(value) => upload.accept(value).expect("upload progress"),
            Err(error) => {
                assert_eq!(error, ArtifactError::Integrity);
                break;
            }
        }
    }
    assert!(upload.descriptor().is_none());
    assert_eq!(
        read_json::<Metadata>(
            &store
                .root
                .join("blake3")
                .join(full.as_str())
                .join("meta.json")
        )
        .expect("unchanged age")
        .last_put_at,
        1
    );
    assert_eq!(
        read_bounded(&collision.join("data")).expect("other intact"),
        b"other"
    );
    assert_eq!(
        read_bounded(&store.root.join("blake3").join(full.as_str()).join("data"))
            .expect("original intact"),
        bytes
    );
}
