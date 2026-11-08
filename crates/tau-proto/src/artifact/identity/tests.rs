use super::*;
use crate::ArtifactDescriptor;

/// Independently framed UTF-8 vectors pin field order, byte lengths, optional
/// tags, domain separation, and incremental hashing for all transfer peers.
#[test]
fn canonical_framing_is_unambiguous_and_streamable() {
    let identity =
        ArtifactIdentity::metadata(Some("é.txt".into()), Some("text/plain".into())).unwrap();
    let mut preimage = b"tau.artifact\0\x01\x01".to_vec();
    preimage.extend_from_slice(&6u64.to_be_bytes());
    preimage.extend_from_slice("é.txt".as_bytes());
    preimage.push(1);
    preimage.extend_from_slice(&10u64.to_be_bytes());
    preimage.extend_from_slice(b"text/plain");
    preimage.extend_from_slice(&7u64.to_be_bytes());
    preimage.extend_from_slice(b"payload");
    let expected = blake3::hash(&preimage);
    assert_eq!(identity.digest(b"payload"), expected);
    let mut incremental = identity.hasher(7);
    incremental.update(b"pay");
    incremental.update(b"load");
    assert_eq!(incremental.finalize(), expected);
    let identities = [
        identity,
        ArtifactIdentity::LegacyPayload,
        ArtifactIdentity::anonymous(),
        ArtifactIdentity::metadata(Some("".into()), None).unwrap(),
        ArtifactIdentity::metadata(None, Some("".into())).unwrap(),
        ArtifactIdentity::metadata(Some("ab".into()), Some("c".into())).unwrap(),
        ArtifactIdentity::metadata(Some("a".into()), Some("bc".into())).unwrap(),
    ];
    let digests: std::collections::BTreeSet<_> = identities
        .iter()
        .map(|identity| identity.digest(b"payload").to_hex().to_string())
        .collect();
    assert_eq!(digests.len(), identities.len());
    assert_ne!(
        identities[0].digest(b"payload"),
        identities[0].digest(b"payloaD")
    );
}

/// Deserialization cannot bypass shared metadata bounds; omitted historical
/// descriptors retain the original raw-payload hash scheme.
#[test]
fn metadata_bounds_and_legacy_descriptors_survive_serialization() {
    let oversized =
        serde_json::json!({"MetadataV1": {"filename": "x".repeat(256), "mime_type": null}});
    assert!(serde_json::from_value::<ArtifactIdentity>(oversized).is_err());
    let identity =
        ArtifactIdentity::metadata(Some("x".repeat(255)), Some("text/plain".into())).unwrap();
    assert_eq!(
        serde_json::from_value::<ArtifactIdentity>(serde_json::to_value(&identity).unwrap())
            .unwrap(),
        identity
    );
    let descriptor =
        ArtifactDescriptor::from_bytes(b"old", ArtifactIdentity::LegacyPayload).unwrap();
    let mut json = serde_json::to_value(&descriptor).unwrap();
    json.as_object_mut().unwrap().remove("identity");
    let restored: ArtifactDescriptor = serde_json::from_value(json).unwrap();
    assert_eq!(restored, descriptor);
    restored.verify(b"old").unwrap();
}
