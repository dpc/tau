use super::*;

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
            tau_proto::ArtifactKey::parse(format!("blake3:{}", blake3::hash(bytes).to_hex()))
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
    let descriptor = ArtifactDescriptor::new(
        tau_proto::ArtifactKey::parse(format!("blake3:{}", blake3::hash(b"original").to_hex()))
            .expect("valid key"),
        8,
    )
    .expect("bounded descriptor");
    upload
        .accept(ArtifactValue::Descriptor(descriptor.clone()))
        .expect("final response");
    assert_eq!(upload.descriptor(), Some(&descriptor));
    assert!(upload.next_op().is_none());
}
