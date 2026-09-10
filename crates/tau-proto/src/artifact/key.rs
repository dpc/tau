use super::identifier::artifact_id;

artifact_id!(
    ArtifactKey,
    "Canonical BLAKE3 original-byte identity, not a path or transfer token.",
    valid_key
);

fn valid_key(text: &str) -> bool {
    super::artifact_digest(text).is_some()
}
