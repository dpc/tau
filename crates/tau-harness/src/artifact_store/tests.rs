use std::process::Command;

use super::*;

fn put(
    store: &mut ArtifactStore,
    owner: &str,
    connection: &str,
    bytes: &[u8],
    now: u64,
) -> (ArtifactUploadId, ArtifactDescriptor) {
    let ArtifactValue::Upload { upload } = store
        .execute(
            owner,
            connection,
            ArtifactOp::Begin {
                size: tau_proto::ArtifactSize::new(bytes.len() as u64).expect("bounded fixture"),
            },
            now,
        )
        .expect("begin fixture upload")
    else {
        panic!("upload")
    };
    for (index, chunk) in bytes.chunks(ARTIFACT_CHUNK_BYTES).enumerate() {
        store
            .execute(
                owner,
                connection,
                ArtifactOp::Write {
                    upload: upload.clone(),
                    offset: (index * ARTIFACT_CHUNK_BYTES) as u64,
                    bytes: chunk.to_vec(),
                },
                now,
            )
            .expect("write fixture chunk");
    }
    let ArtifactValue::Descriptor(descriptor) = store
        .execute(
            owner,
            connection,
            ArtifactOp::Finalize {
                upload: upload.clone(),
            },
            now,
        )
        .expect("finalize fixture upload")
    else {
        panic!("descriptor")
    };
    (upload, descriptor)
}

/// Original identity excludes hints and creator sessions; new duplicates renew
/// shared age, while finalization retries survive restart without renewal.
#[test]
fn duplicates_and_restart_retries_have_distinct_age_semantics() {
    let temp = tempfile::tempdir().expect("private root");
    let mut first = ArtifactStore::new(temp.path());
    let (upload, descriptor) = put(
        &mut first,
        "image/session-a",
        "one",
        b"original\0bytes",
        100,
    );
    let mut second = ArtifactStore::new(temp.path());
    let (_, duplicate) = put(
        &mut second,
        "shell/session-b",
        "two",
        b"original\0bytes",
        200,
    );
    assert_eq!(descriptor, duplicate);
    assert_eq!(
        second
            .metadata(&descriptor.key)
            .expect("metadata")
            .last_put_at,
        200
    );
    drop(first);
    let result = second
        .execute(
            "image/session-a",
            "reconnected",
            ArtifactOp::Finalize {
                upload: upload.clone(),
            },
            300,
        )
        .expect("retry finalize");
    assert_eq!(result, ArtifactValue::Descriptor(descriptor.clone()));
    assert_eq!(
        second
            .metadata(&descriptor.key)
            .expect("metadata")
            .last_put_at,
        200
    );
    assert_eq!(
        second.execute(
            "other/session-a",
            "reconnected",
            ArtifactOp::Finalize { upload },
            300
        ),
        Err(ArtifactError::Permission)
    );
    let (_, rolled_back) = put(
        &mut second,
        "shell/session-b",
        "two",
        b"original\0bytes",
        150,
    );
    assert_eq!(rolled_back, descriptor);
    assert_eq!(
        second
            .metadata(&descriptor.key)
            .expect("metadata")
            .last_put_at,
        200
    );
    assert_eq!(
        fs::read_dir(temp.path().join("artifacts/blake3"))
            .expect("object directory")
            .count(),
        1
    );
}

/// Exact retransmission repairs response loss; gaps, overlapping extensions,
/// conflicting bytes, partial finalize, and unrelated connection writers fail.
#[test]
fn writes_are_bounded_and_retry_safe() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let ArtifactValue::Upload { upload } = store
        .begin(
            "owner/session",
            "one",
            tau_proto::ArtifactSize::new(4).expect("size"),
        )
        .expect("begin upload")
    else {
        panic!("upload")
    };
    assert_eq!(
        store.write("one", &upload, 0, b"ab"),
        Ok(ArtifactValue::Written { next_offset: 2 })
    );
    assert_eq!(
        store.write("one", &upload, 0, b"ab"),
        Ok(ArtifactValue::Written { next_offset: 2 })
    );
    assert_eq!(
        store.write("one", &upload, 0, b"ax"),
        Err(ArtifactError::Integrity)
    );
    assert_eq!(
        store.write("one", &upload, 1, b"bc"),
        Err(ArtifactError::Integrity)
    );
    assert_eq!(
        store.write("one", &upload, 3, b"d"),
        Err(ArtifactError::Invalid)
    );
    assert_eq!(
        store.write("other", &upload, 2, b"cd"),
        Err(ArtifactError::Permission)
    );
    assert_eq!(
        store.finalize("owner/session", "one", &upload, 1),
        Err(ArtifactError::Invalid)
    );
    store.write("one", &upload, 2, b"cd").expect("finish bytes");
    assert!(store.finalize("owner/session", "one", &upload, 2).is_ok());
    assert!(matches!(
        tau_proto::ArtifactSize::new(ARTIFACT_MAX_BYTES + 1),
        Err(ArtifactError::Invalid)
    ));
    assert_eq!(
        ArtifactUploadId::parse("../escape"),
        Err(ArtifactError::Invalid)
    );
}

/// An open bounded read protects originals across independent store instances;
/// stat and reads do not renew age, and cleanup rechecks a concurrent renewal.
#[test]
fn active_reads_cleanup_and_renewal_serialize_under_stable_lock() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (_, descriptor) = put(&mut store, "shell/one", "one", b"abc", 10);
    let cleaner = ArtifactStore::new(temp.path());
    let ArtifactValue::Opened { read, .. } = store
        .open("two", descriptor.key.clone())
        .expect("open read")
    else {
        panic!("open")
    };
    assert_eq!(
        cleaner.cleanup(Some(Duration::from_secs(1)), 100),
        Err(ArtifactError::Busy)
    );
    assert_eq!(
        store.read("two", &read, 0, 3).expect("read chunk"),
        ArtifactValue::Chunk {
            offset: 0,
            bytes: b"abc".to_vec(),
            eof: true
        }
    );
    assert!(
        store.read("two", &read, 0, 3).is_ok(),
        "last chunk remains retryable"
    );
    assert_eq!(
        store
            .metadata(&descriptor.key)
            .expect("metadata")
            .last_put_at,
        10
    );
    store
        .execute("shell/one", "two", ArtifactOp::Close { read }, 100)
        .expect("close read");
    put(&mut store, "other/two", "three", b"abc", 100);
    cleaner
        .cleanup(Some(Duration::from_secs(10)), 105)
        .expect("cleanup before expiry");
    assert!(store.descriptor(&descriptor.key).is_ok());
    cleaner
        .cleanup(Some(Duration::from_secs(10)), 110)
        .expect("cleanup at expiry");
    assert_eq!(
        store.open("one", descriptor.key),
        Err(ArtifactError::Unavailable)
    );
}

/// Session deletion cannot delete a shared original, but different roots do not
/// resolve the same digest unless bytes are explicitly put there.
#[test]
fn cross_session_lifetime_and_separate_roots() {
    let a = tempfile::tempdir().expect("first private root");
    let b = tempfile::tempdir().expect("second private root");
    fs::create_dir_all(a.path().join("sessions/one")).expect("seed session");
    let mut first = ArtifactStore::new(a.path());
    let (_, descriptor) = put(&mut first, "tool/one", "one", b"shared", 1);
    fs::remove_dir_all(a.path().join("sessions/one")).expect("delete session");
    let mut same = ArtifactStore::new(a.path());
    assert!(
        same.execute(
            "tool/two",
            "two",
            ArtifactOp::Stat {
                key: descriptor.key.clone()
            },
            2
        )
        .is_ok()
    );
    let mut other = ArtifactStore::new(b.path());
    assert_eq!(
        other.execute(
            "tool/two",
            "two",
            ArtifactOp::Stat {
                key: descriptor.key.clone()
            },
            2
        ),
        Err(ArtifactError::Unavailable)
    );
    assert_eq!(
        put(&mut other, "tool/two", "two", b"shared", 2).1,
        descriptor
    );
}

/// Cleanup preserves uncertain/future metadata; it must never substitute mtime
/// or epoch zero, and disabled retention leaves originals alone.
#[test]
fn cleanup_preserves_future_and_corrupt_metadata() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (_, descriptor) = put(&mut store, "tool/one", "one", b"future", 100);
    store
        .cleanup(Some(Duration::from_secs(1)), 10)
        .expect("future age");
    store.cleanup(None, 1000).expect("disabled cleanup");
    let object = store
        .root
        .join("blake3")
        .join(artifact_digest(&descriptor.key).expect("valid digest"));
    fs::write(object.join("meta.json"), b"{broken").expect("corrupt metadata");
    assert_eq!(
        store.cleanup(Some(Duration::from_secs(1)), 1000),
        Err(ArtifactError::Integrity)
    );
    assert!(object.join("data").exists());
    assert_eq!(
        put_corrupt_duplicate(&mut store),
        Err(ArtifactError::Integrity)
    );
}

fn put_corrupt_duplicate(store: &mut ArtifactStore) -> Result<ArtifactValue, ArtifactError> {
    let ArtifactValue::Upload { upload } =
        store.begin("tool/one", "one", tau_proto::ArtifactSize::new(6)?)?
    else {
        panic!("upload")
    };
    store.write("one", &upload, 0, b"future")?;
    store.finalize("tool/one", "one", &upload, 1001)
}

/// An interrupted finalization retains the original fixed timestamp and
/// identity; retrying its durable intent publishes once and later retries do
/// not renew it.
#[test]
fn durable_intent_recovery_and_completed_receipt_expiry() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let _lock = store.lock(true).expect("coordinate fixture");
    let id = ArtifactUploadId::parse(new_id()).expect("upload identity");
    let path = store.root.join("operations").join(id.as_str());
    private_dir(&path).expect("intent directory");
    let receipt = Receipt {
        version: 1,
        owner: "tool/session".to_owned(),
        descriptor: ArtifactDescriptor::new(
            ArtifactKey::parse(format!("blake3:{}", blake3::hash(b"bytes").to_hex()))
                .expect("valid key"),
            5,
        )
        .expect("valid descriptor"),
        put_at: 50,
        expires_at: 1000,
        complete: false,
    };
    write_synced(&path.join("data"), b"bytes").expect("intent bytes");
    write_json(&path.join("receipt.json"), &receipt).expect("intent metadata");
    sync_dir(&path).expect("sync intent");
    sync_dir(&store.root.join("operations")).expect("sync publication");
    drop(_lock);
    assert_eq!(
        store
            .finalize("tool/session", "restarted", &id, 100)
            .expect("recover intent"),
        ArtifactValue::Descriptor(receipt.descriptor.clone())
    );
    assert_eq!(
        store
            .metadata(&receipt.descriptor.key)
            .expect("metadata")
            .last_put_at,
        50
    );
    assert!(!path.join("data").exists());
    store.cleanup(None, 1000).expect("expire receipt");
    assert!(!path.exists());
    assert_eq!(
        store.finalize("tool/session", "restarted", &id, 1001),
        Err(ArtifactError::Unavailable)
    );
    assert!(store.descriptor(&receipt.descriptor.key).is_ok());
}

/// Disconnect, explicit abort, and non-renewable timeout release transfer
/// budgets without deleting any object whose publication already won.
#[test]
fn cancellation_expiry_and_admission_are_bounded() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (finished, descriptor) = put(&mut store, "tool/session", "one", b"keep", 1);
    store
        .execute(
            "tool/session",
            "one",
            ArtifactOp::Abort { upload: finished },
            2,
        )
        .expect("abort completed upload");
    for _ in 0..TRANSFER_LIMIT {
        store
            .begin(
                "tool/session",
                "one",
                tau_proto::ArtifactSize::new(ARTIFACT_MAX_BYTES).expect("size"),
            )
            .expect("admit bounded upload");
    }
    assert_eq!(
        store.begin(
            "tool/session",
            "one",
            tau_proto::ArtifactSize::new(0).expect("size")
        ),
        Err(ArtifactError::Busy)
    );
    store.disconnect("one");
    store
        .begin(
            "tool/session",
            "two",
            tau_proto::ArtifactSize::new(0).expect("size"),
        )
        .expect("capacity recovered");
    store.expire(Instant::now() + TRANSFER_LIFETIME);
    assert!(store.uploads.is_empty());
    let ArtifactValue::Opened { read, .. } = store
        .open("two", descriptor.key.clone())
        .expect("open read")
    else {
        panic!("read")
    };
    store.expire(Instant::now() + TRANSFER_LIFETIME);
    assert_eq!(
        store.read("two", &read, 0, 1),
        Err(ArtifactError::Unavailable)
    );
    assert!(store.descriptor(&descriptor.key).is_ok());
}

/// Fake client state machines exercise multi-chunk wire encoding, original-byte
/// verification, and response-lost retry without a provider, network, or shell.
#[test]
fn client_wire_store_roundtrip_preserves_original_bytes() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let bytes: Vec<u8> = (0..ARTIFACT_CHUNK_BYTES + 37)
        .map(|i| (i % 251) as u8)
        .collect();
    let mut upload = tau_client::ArtifactUpload::new(bytes.clone()).expect("bounded original");
    while let Some(op) = upload.next_op() {
        let request = tau_proto::ArtifactRequest {
            request_id: "request".parse().expect("valid request"),
            expected_session_id: "session".parse().expect("session identifier"),
            op,
        };
        let message = tau_proto::HarnessInputMessage::ArtifactRequest(request);
        assert!(tau_proto::artifact_frame_fits(&message));
        let mut encoded = Vec::new();
        tau_proto::encode_message(&mut encoded, &message).expect("encode request");
        let decoded =
            tau_proto::decode_message_from_slice::<tau_proto::HarnessInputMessage>(&encoded)
                .expect("decode request");
        let tau_proto::HarnessInputMessage::ArtifactRequest(request) = decoded else {
            panic!("request")
        };
        let response = store
            .execute("tool/session", "one", request.op.clone(), 100)
            .expect("execute request");
        if matches!(
            request.op,
            ArtifactOp::Write { .. } | ArtifactOp::Finalize { .. }
        ) {
            assert_eq!(
                store
                    .execute("tool/session", "one", request.op, 200)
                    .expect("retry request"),
                response
            );
        }
        upload.accept(response).expect("upload response");
    }
    let key = upload
        .descriptor()
        .expect("published descriptor")
        .key
        .clone();
    let mut download = tau_client::ArtifactDownload::new(key.clone());
    while let Some(op) = download.next_op() {
        let response =
            tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(tau_proto::ArtifactResult {
                request_id: "request".parse().expect("valid request"),
                result: store.execute("other/session", "two", op, 300),
            }));
        assert!(tau_proto::artifact_frame_fits(&response));
        let mut encoded = Vec::new();
        tau_proto::encode_message(&mut encoded, &response).expect("encode response");
        let tau_proto::HarnessOutputMessage::ArtifactResult(response) =
            tau_proto::decode_message_from_slice(&encoded).expect("decode response")
        else {
            panic!("result")
        };
        download
            .accept(response.result.expect("read response"))
            .expect("valid range");
    }
    assert_eq!(download.into_bytes().expect("verified original"), bytes);
    assert_eq!(store.metadata(&key).expect("metadata").last_put_at, 100);
}

/// Crash-detached originals and incomplete private publication directories are
/// recovered independently of session cleanup and with retention disabled.
#[test]
fn cleanup_recovers_detach_and_staging_without_references() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (_, descriptor) = put(&mut store, "tool/session", "one", b"old", 1);
    let object = store
        .root
        .join("blake3")
        .join(artifact_digest(&descriptor.key).expect("valid digest"));
    fs::rename(object, store.root.join(".cleanup").join(new_id())).expect("crash detach");
    let staged = store.root.join("operations/.staging-incomplete");
    private_dir(&staged).expect("staging directory");
    write_synced(&staged.join("data"), b"partial").expect("partial bytes");
    store.cleanup(None, 10).expect("recover detach");
    assert!(!staged.exists());
    assert_eq!(
        fs::read_dir(store.root.join(".cleanup"))
            .expect("cleanup directory")
            .count(),
        0
    );
}

/// Every published original and receipt is owner-private, not executable.
#[test]
fn published_storage_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (id, descriptor) = put(&mut store, "tool/session", "one", b"bytes", 1);
    let object = store
        .root
        .join("blake3")
        .join(artifact_digest(&descriptor.key).expect("valid digest"));
    assert_eq!(
        fs::metadata(&object)
            .expect("object permissions")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(object.join("data"))
            .expect("data permissions")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(
            store
                .root
                .join("operations")
                .join(id.as_str())
                .join("receipt.json")
        )
        .expect("receipt permissions")
        .permissions()
        .mode()
            & 0o777,
        0o600
    );
}

/// The stable lock protects active reads across actual OS processes, not merely
/// the worker's process-local maps; release permits a later cleanup pass.
#[test]
fn active_read_coordination_crosses_processes() {
    let temp = tempfile::tempdir().expect("private root");
    let mut store = ArtifactStore::new(temp.path());
    let (_, descriptor) = put(&mut store, "tool/session", "one", b"cross-process", 1);
    let ArtifactValue::Opened { read, .. } = store
        .open("one", descriptor.key.clone())
        .expect("open read")
    else {
        panic!("read")
    };
    for busy in [true, false] {
        if !busy {
            store
                .execute(
                    "tool/session",
                    "one",
                    ArtifactOp::Close { read: read.clone() },
                    2,
                )
                .expect("close read");
        }
        let result = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "artifact_store::tests::artifact_coordination_child",
                "--nocapture",
            ])
            .env("TAU_ARTIFACT_TEST_ROOT", temp.path())
            .env("TAU_ARTIFACT_TEST_BUSY", if busy { "yes" } else { "no" })
            .output()
            .expect("run coordination child");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(
        store.descriptor(&descriptor.key),
        Err(ArtifactError::Unavailable)
    );
}

/// Child half of the cross-process lock oracle; ordinary test runs do no work.
#[test]
fn artifact_coordination_child() {
    let Some(root) = std::env::var_os("TAU_ARTIFACT_TEST_ROOT") else {
        return;
    };
    let store = ArtifactStore::new(Path::new(&root));
    let result = store.cleanup(Some(Duration::from_secs(1)), 10);
    if std::env::var("TAU_ARTIFACT_TEST_BUSY").expect("child mode") == "yes" {
        assert_eq!(result, Err(ArtifactError::Busy));
    } else {
        assert_eq!(result, Ok(()));
    }
}
