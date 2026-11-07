//! Compatibility tests use pre-migration disk layouts, not a second store.

use super::*;

/// Existing full-digest objects work through both key spellings after restart,
/// while legacy requests remain exact and receipts keep their original key.
#[test]
fn legacy_objects_and_receipts_survive_short_key_migration() {
    let root = tempfile::tempdir().expect("root");
    let mut store = ArtifactStore::new(root.path());
    let bytes = b"pre-migration bytes";
    let full = blake3::hash(bytes).to_hex();
    let legacy = ArtifactKey::parse(format!("blake3:{full}")).expect("legacy");
    let short = ArtifactKey::from_digest(full.as_str()).expect("short");
    let object = store.root.join("blake3").join(full.as_str());
    private_dir(&object).expect("legacy object");
    write_synced(&object.join("data"), bytes).expect("original");
    write_json(
        &object.join("meta.json"),
        &Metadata {
            version: 1,
            size: bytes.len() as u64,
            last_put_at: 1,
        },
    )
    .expect("legacy metadata");
    for key in [&short, &legacy] {
        let mut download = tau_client::ArtifactDownload::new(key.clone());
        while let Some(op) = download.next_op() {
            download
                .accept(store.execute("owner", "connection", op, 2).expect("read"))
                .expect("verify");
        }
        assert_eq!(download.into_bytes().expect("original"), bytes);
    }
    let wrong_full = format!("blake3:{}{}", short, "0".repeat(40));
    assert_eq!(
        store.descriptor(&wrong_full.parse().expect("legacy")),
        Err(ArtifactError::Unavailable)
    );
    let (upload, descriptor) = tests::put(&mut store, "owner", "connection", bytes, 3);
    assert_eq!(descriptor.key, short);
    let receipt_path = store
        .root
        .join("operations")
        .join(upload.as_str())
        .join("receipt.json");
    let mut receipt: Receipt = read_json(&receipt_path).expect("receipt");
    receipt.descriptor.key = legacy.clone();
    replace_json(&receipt_path, &receipt).expect("pre-migration receipt");
    drop(store);
    let mut restarted = ArtifactStore::new(root.path());
    assert_eq!(
        restarted
            .finalize("owner", "connection", &upload, 4)
            .expect("old retry"),
        ArtifactValue::Descriptor(
            ArtifactDescriptor::new(legacy, bytes.len() as u64).expect("descriptor")
        )
    );
    let ArtifactValue::Listed { entries } = restarted.list("", 10, || true).expect("list") else {
        panic!("list");
    };
    assert_eq!(entries[0].descriptor.key, short);
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
        );
    }
    let ArtifactValue::Listed { entries } = restarted
        .list(&format!("blake3:{}", short), 10, || true)
        .expect("legacy prefix")
    else {
        panic!("list");
    };
    assert_eq!(entries.len(), 1);
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
    let full = blake3::hash(bytes).to_hex();
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
    let legacy = ArtifactKey::parse(format!("blake3:{full}")).expect("legacy");
    assert!(
        store.descriptor(&legacy).is_ok(),
        "legacy lookup remains exact"
    );
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
        store.metadata(&legacy).expect("unchanged age").last_put_at,
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
