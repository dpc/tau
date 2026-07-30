use std::os::unix as path_std_os_unix;

use super::*;

/// Regression guard: armed temp files are deleted on failure paths before
/// rename.
#[test]
fn pending_temp_file_removes_armed_file_on_drop() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let temp_path = temp_dir.path().join("partial.tmp");
    std::fs::write(&temp_path, b"partial secret").expect("write temp");

    drop(PendingTempFile::new(temp_path.clone()));

    assert!(!temp_path.exists(), "armed temp file should be removed");
}

/// Regression guard: successful rename paths can disarm temp cleanup.
#[test]
fn pending_temp_file_disarm_preserves_file_on_drop() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let temp_path = temp_dir.path().join("complete.tmp");
    std::fs::write(&temp_path, b"complete").expect("write temp");
    let mut pending = PendingTempFile::new(temp_path.clone());

    pending.disarm();
    drop(pending);

    assert!(temp_path.exists(), "disarmed temp file should remain");
}

/// Ensures writes follow destination symlinks and preserve target permissions.
#[test]
#[cfg(unix)]
fn replaces_symlink_target_and_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let target = temp_dir.path().join("target.json5");
    let link = temp_dir.path().join("config.json5");
    std::fs::write(&target, b"{}").expect("write target");
    path_std_os_unix::fs::symlink(&target, &link).expect("symlink");
    std::fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("set perms");

    atomic_write_following_symlink(&link, b"{\"updated\":true}", None).expect("atomic write");

    assert!(
        fs::symlink_metadata(&link)
            .expect("symlink metadata")
            .file_type()
            .is_symlink(),
        "the symlink itself must not be replaced"
    );
    let body = std::fs::read_to_string(&target).expect("read");
    assert!(body.contains("updated"));
    assert_eq!(
        fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o777,
        0o640,
        "existing permissions on the target file are preserved"
    );
}

/// Ensures newly created files use the caller-provided default permissions.
#[test]
#[cfg(unix)]
fn applies_default_permissions_when_file_is_new() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let path = temp_dir.path().join("auth.json");

    atomic_write_following_symlink(&path, b"{}", Some(fs::Permissions::from_mode(0o600)))
        .expect("atomic write");

    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600,
    );
}
