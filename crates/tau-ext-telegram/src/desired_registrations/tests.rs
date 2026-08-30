use super::*;

/// Malformed and unknown-version files must fail closed instead of silently
/// dropping desired routes after an extension restart.
#[test]
fn durable_file_decode_is_strict() {
    assert!(decode_file(b"not cbor").is_err());
    let mut unknown = Vec::new();
    ciborium::into_writer(
        &DesiredRegistrationFile {
            schema: 1,
            agents: BTreeSet::new(),
        },
        &mut unknown,
    )
    .expect("encode unknown schema");
    assert!(decode_file(&unknown).is_err());
}
