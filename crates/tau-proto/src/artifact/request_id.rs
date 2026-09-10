use super::identifier::artifact_id;

artifact_id!(
    ArtifactRequestId,
    "Bounded caller-selected Artifact RPC correlation, not an object key.",
    super::artifact_identifier
);
