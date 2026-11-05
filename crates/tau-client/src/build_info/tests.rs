use super::resolve_revision;

/// A stamped package overrides missing or unrelated build-environment metadata.
#[test]
fn package_stamp_takes_precedence_and_preserves_dirty_state() {
    let sha = b"abcdef0123456789abcdef0123456789abcdef01";
    assert_eq!(
        resolve_revision(sha, b"modified_________", Some("1234567"), Some(false)),
        Some("abcdef0-modified".to_owned())
    );
    assert_eq!(
        resolve_revision(sha, b"clean____________", None, None),
        Some("abcdef0".to_owned())
    );
}

/// Unstamped builds use only caller metadata; archives explicitly lack a hash.
#[test]
fn source_build_and_archive_fallbacks_are_honest() {
    let placeholder = b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER____";
    assert_eq!(
        resolve_revision(
            placeholder,
            b"__TAU_BUILD_DIRTY",
            Some("987654321"),
            Some(true)
        ),
        Some("9876543-modified".to_owned())
    );
    assert_eq!(
        resolve_revision(placeholder, b"__TAU_BUILD_DIRTY", None, None),
        None
    );
    assert_eq!(
        resolve_revision(
            placeholder,
            b"__TAU_BUILD_DIRTY",
            Some("/secret/path"),
            None
        ),
        None
    );
}
