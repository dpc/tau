use super::*;

/// Corrupted originals must never become usable output, even when length and
/// chunk framing are otherwise valid; a best-effort close remains available.
#[test]
fn corrupt_original_is_not_exposed() {
    let key = ArtifactKey::parse(format!("blake3:{}", blake3::hash(b"good").to_hex()))
        .expect("valid key");
    let mut download = ArtifactDownload::new(key.clone());
    download
        .accept(ArtifactValue::Opened {
            read: "read".parse().expect("valid read"),
            descriptor: ArtifactDescriptor::new(key, 4).expect("valid descriptor"),
        })
        .expect("open response");
    assert_eq!(
        download.accept(ArtifactValue::Chunk {
            offset: 0,
            bytes: b"evil".to_vec(),
            eof: true
        }),
        Err(ArtifactError::Integrity)
    );
    assert!(download.close_op().is_some());
    assert_eq!(download.into_bytes(), Err(ArtifactError::Invalid));
}

/// Download range validation rejects out-of-order, oversized, and false-EOF
/// responses before they can make a completed original available.
#[test]
fn incorrect_range_and_eof_are_rejected() {
    let key = ArtifactKey::parse(format!("blake3:{}", blake3::hash(b"good").to_hex()))
        .expect("valid key");
    let mut download = ArtifactDownload::new(key.clone());
    download
        .accept(ArtifactValue::Opened {
            read: "read".parse().expect("valid read"),
            descriptor: ArtifactDescriptor::new(key, 4).expect("valid descriptor"),
        })
        .expect("open response");
    for value in [
        ArtifactValue::Chunk {
            offset: 1,
            bytes: b"good".to_vec(),
            eof: true,
        },
        ArtifactValue::Chunk {
            offset: 0,
            bytes: b"good".to_vec(),
            eof: false,
        },
        ArtifactValue::Chunk {
            offset: 0,
            bytes: Vec::new(),
            eof: false,
        },
        ArtifactValue::Chunk {
            offset: 0,
            bytes: b"good-extra".to_vec(),
            eof: true,
        },
    ] {
        assert_eq!(download.accept(value), Err(ArtifactError::Integrity));
    }
    download
        .accept(ArtifactValue::Chunk {
            offset: 0,
            bytes: b"good".to_vec(),
            eof: true,
        })
        .expect("valid original chunk");
    download
        .accept(ArtifactValue::Done)
        .expect("close response");
    assert_eq!(download.into_bytes().expect("verified bytes"), b"good");
}
