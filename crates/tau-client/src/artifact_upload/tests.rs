use super::*;

/// A server must return the requested metadata, not a valid but substituted
/// metadata/payload tuple; rejection must leave Finalize retryable.
#[test]
fn upload_rejects_valid_server_selected_metadata() {
    let mut upload = ArtifactUpload::with_metadata(
        b"bytes".to_vec(),
        Some("example.txt".into()),
        Some("text/plain".into()),
    )
    .unwrap();
    upload
        .accept(ArtifactValue::Upload {
            upload: "id".parse().unwrap(),
        })
        .unwrap();
    upload
        .accept(ArtifactValue::Written { next_offset: 5 })
        .unwrap();
    let requested = upload.identity.clone();
    for identity in [
        tau_proto::ArtifactIdentity::LegacyPayload,
        tau_proto::ArtifactIdentity::anonymous(),
        tau_proto::ArtifactIdentity::metadata(Some("other.txt".into()), Some("text/plain".into()))
            .unwrap(),
        tau_proto::ArtifactIdentity::metadata(
            Some("example.txt".into()),
            Some("application/x-test".into()),
        )
        .unwrap(),
    ] {
        let descriptor = ArtifactDescriptor::from_bytes(b"bytes", identity).unwrap();
        assert_eq!(
            upload.accept(ArtifactValue::Descriptor(descriptor)),
            Err(ArtifactError::Integrity)
        );
        assert!(matches!(
            upload.next_op(),
            Some(ArtifactOp::Finalize { .. })
        ));
    }
    upload
        .accept(ArtifactValue::Descriptor(
            ArtifactDescriptor::from_bytes(b"bytes", requested).unwrap(),
        ))
        .unwrap();
    assert!(upload.next_op().is_none());
}

/// Rejections at every upload stage preserve the next retryable operation.
#[test]
fn rejected_responses_do_not_advance_upload() {
    let mut upload = ArtifactUpload::new(b"original".to_vec()).expect("bounded original");
    let before = upload.next_op();
    assert_eq!(
        upload.accept(ArtifactValue::Done),
        Err(ArtifactError::Invalid)
    );
    assert_eq!(upload.next_op(), before);
    // Malformed transfer identities are rejected before entering the state
    // machine.
    assert!(ArtifactUploadId::parse("../upload").is_err());
    upload
        .accept(ArtifactValue::Upload {
            upload: "upload".parse().expect("valid upload"),
        })
        .expect("begin response");
    let before = upload.next_op();
    for value in [
        ArtifactValue::Done,
        ArtifactValue::Written { next_offset: 7 },
        ArtifactValue::Written { next_offset: 9 },
    ] {
        assert_eq!(upload.accept(value), Err(ArtifactError::Invalid));
        assert_eq!(upload.next_op(), before);
    }
    upload
        .accept(ArtifactValue::Written { next_offset: 8 })
        .expect("chunk response");
    let before = upload.next_op();
    for (bytes, size) in [(b"original".as_slice(), 7), (b"different".as_slice(), 8)] {
        let descriptor = ArtifactDescriptor::new(
            tau_proto::ArtifactKey::from_digest(blake3::hash(bytes).to_hex().as_str())
                .expect("valid key"),
            size,
        )
        .expect("bounded descriptor");
        assert_eq!(
            upload.accept(ArtifactValue::Descriptor(descriptor)),
            Err(ArtifactError::Integrity)
        );
        assert_eq!(upload.next_op(), before);
        assert!(upload.descriptor().is_none());
    }
    assert_eq!(
        upload.accept(ArtifactValue::Done),
        Err(ArtifactError::Invalid)
    );
    assert_eq!(upload.next_op(), before);
    let descriptor = ArtifactDescriptor::from_bytes(
        b"original",
        tau_proto::ArtifactIdentity::metadata(None, None).expect("metadata"),
    )
    .expect("bounded descriptor");
    upload
        .accept(ArtifactValue::Descriptor(descriptor.clone()))
        .expect("final response");
    assert_eq!(upload.descriptor(), Some(&descriptor));
    assert!(upload.next_op().is_none());
}
