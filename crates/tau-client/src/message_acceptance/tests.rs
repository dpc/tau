use super::*;

/// Unknown and older peers must never receive an unsupported request shape.
#[test]
fn unsupported_versions_fail_closed() {
    for version in [
        None,
        Some(ProtocolVersion::new(23, 1)),
        Some(ProtocolVersion::new(22, 99)),
        Some(ProtocolVersion::new(24, 0)),
    ] {
        assert!(validate_version(version).is_err());
    }
    assert!(validate_version(Some(ProtocolVersion::new(23, 2))).is_ok());
    assert!(validate_version(Some(ProtocolVersion::new(23, 3))).is_ok());
}
