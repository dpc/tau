use super::format_patched_revision;

/// Ensures fixed-width package metadata preserves both the real revision and
/// an explicit dirty-source label instead of fabricating a clean-looking SHA.
#[test]
fn patched_dirty_revision_is_explicit() {
    let revision = b"0123456789abcdef0123456789abcdef01234567";
    let state = b"modified_________";

    assert_eq!(format_patched_revision(revision, state), "0123456-modified");
}

/// Ensures clean package metadata continues to expose the short real revision.
#[test]
fn patched_clean_revision_stays_clean() {
    let revision = b"0123456789abcdef0123456789abcdef01234567";
    let state = b"clean____________";

    assert_eq!(format_patched_revision(revision, state), "0123456");
}
