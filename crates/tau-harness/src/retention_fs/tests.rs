#[cfg(unix)]
use std::os::unix::fs as unix_fs;

use tempfile::TempDir;

/// Staging preparation rejects a final-component symlink without creating or
/// synchronizing content through its target.
#[cfg(unix)]
#[test]
fn staging_root_symlink_is_rejected_without_following_target() {
    let temp = TempDir::new().expect("temp root");
    let target = temp.path().join("target");
    let staging = temp.path().join("staging");
    std::fs::create_dir(&target).expect("target directory");
    unix_fs::symlink(&target, &staging).expect("staging symlink");
    let mut sync_called = false;

    let result = super::prepare_staging_directory(&staging, &mut |_| {
        sync_called = true;
        Ok(())
    });

    assert!(result.is_err());
    assert!(!sync_called);
    assert_eq!(
        std::fs::read_dir(&target).expect("target remains").count(),
        0
    );
}
