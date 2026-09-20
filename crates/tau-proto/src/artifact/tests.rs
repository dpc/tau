use super::*;

/// Semantic identities and immutable descriptors reject malformed wire values
/// during decoding, before any producer or storage state can accept them.
#[test]
fn artifact_identity_and_descriptor_decode_is_checked() {
    assert!(
        serde_json::from_value::<ArtifactOp>(
            serde_json::json!({"op": "begin", "size": ARTIFACT_MAX_BYTES + 1})
        )
        .is_err()
    );
    for text in ["", "../data", "spaces forbidden"] {
        let json = serde_json::to_string(text).expect("JSON string");
        assert!(serde_json::from_str::<ArtifactRequestId>(&json).is_err());
        assert!(serde_json::from_str::<ArtifactUploadId>(&json).is_err());
        assert!(serde_json::from_str::<ArtifactReadId>(&json).is_err());
    }
    let key = format!("blake3:{}", "a".repeat(64));
    for value in [
        serde_json::json!({"key": "../data", "size": 0}),
        serde_json::json!({"key": key, "size": ARTIFACT_MAX_BYTES + 1}),
    ] {
        assert!(serde_json::from_value::<ArtifactDescriptor>(value).is_err());
    }
    let descriptor = ArtifactDescriptor::new(
        ArtifactKey::parse(key).expect("valid key"),
        ARTIFACT_MAX_BYTES,
    )
    .expect("inclusive limit");
    let json = serde_json::to_value(&descriptor).expect("descriptor JSON");
    assert_eq!(
        serde_json::from_value::<ArtifactDescriptor>(json).expect("valid descriptor"),
        descriptor
    );
}

/// Digest spelling is canonical and cannot become an arbitrary filesystem path.
#[test]
fn keys_and_operation_bounds_are_canonical() {
    assert!(artifact_digest(&format!("blake3:{}", "a".repeat(64))).is_some());
    for key in [
        "../data",
        "blake3:abc",
        &format!("blake3:{}", "A".repeat(64)),
    ] {
        assert!(artifact_digest(key).is_none());
    }
    let mut request = ArtifactRequest {
        request_id: "one".parse().expect("valid request"),
        expected_session_id: "session".parse().expect("valid session"),
        op: ArtifactOp::Write {
            upload: "upload".parse().expect("valid upload"),
            offset: 0,
            bytes: vec![0; ARTIFACT_CHUNK_BYTES],
        },
    };
    assert!(request.is_bounded());
    if let ArtifactOp::Write { bytes, .. } = &mut request.op {
        bytes.push(0);
    }
    assert!(!request.is_bounded());
    request.op = ArtifactOp::Read {
        read: "read".parse().expect("valid read"),
        offset: 0,
        length: ARTIFACT_CHUNK_BYTES as u32,
    };
    assert!(request.is_bounded());
    assert!(ArtifactRequestId::parse("x".repeat(129)).is_err());
}

/// Model-visible references round-trip through one shared formatter/parser,
/// while consumers may continue accepting a bare internal content key.
#[test]
fn artifact_references_round_trip_and_accept_bare_keys() {
    let key =
        ArtifactKey::parse(format!("blake3:{}", "a".repeat(64))).expect("canonical artifact key");
    let reference = artifact_reference(&key);
    assert_eq!(reference, format!("<tau-artifact:{key}>"));
    assert_eq!(
        parse_artifact_reference(&reference).expect("canonical reference"),
        key
    );
    assert_eq!(
        parse_artifact_reference(key.as_str()).expect("bare compatibility key"),
        key
    );
    for invalid in [
        format!("tau-artifact:{key}"),
        format!("<tau-data:{key}>"),
        format!("<tau-artifact:{key}"),
        format!("<tau-artifact:{key}>>"),
    ] {
        assert!(parse_artifact_reference(&invalid).is_err(), "{invalid}");
    }
}

/// The complete CBOR frame limit is inclusive and independent of raw chunk
/// limits, so neither direction can accidentally consume the client's cap.
#[test]
fn encoded_frame_bound_is_exact_and_bidirectional() {
    let exact = vec![0; ARTIFACT_FRAME_BYTES - 5];
    assert!(artifact_frame_fits(&serde_bytes::Bytes::new(&exact)));
    let above = vec![0; ARTIFACT_FRAME_BYTES - 4];
    assert!(!artifact_frame_fits(&serde_bytes::Bytes::new(&above)));
    let request = crate::HarnessInputMessage::ArtifactRequest(ArtifactRequest {
        request_id: "request".parse().expect("valid request"),
        expected_session_id: "session".parse().expect("valid session"),
        op: ArtifactOp::Write {
            upload: "upload".parse().expect("valid upload"),
            offset: 0,
            bytes: vec![0xff; ARTIFACT_CHUNK_BYTES],
        },
    });
    let response = crate::HarnessOutputMessage::ArtifactResult(Box::new(ArtifactResult {
        request_id: "request".parse().expect("valid request"),
        result: Ok(ArtifactValue::Chunk {
            offset: 0,
            bytes: vec![0xff; ARTIFACT_CHUNK_BYTES],
            eof: false,
        }),
    }));
    assert!(artifact_frame_fits(&request));
    assert!(artifact_frame_fits(&response));
    assert!(!format!("{request:?}").contains("255"));
    assert!(!format!("{response:?}").contains("255"));
}
