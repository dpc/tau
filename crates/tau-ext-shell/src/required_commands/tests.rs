#[cfg(unix)]
use std::fs::Permissions;

use super::*;

/// Missing ripgrep should request an actionable warning without changing
/// process-global PATH or preventing extension startup.
#[test]
fn missing_ripgrep_requests_warning() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let notices = missing_command_notices_on_path(Some(dir.path().as_os_str()));
    assert_eq!(notices.len(), 1);
    let HarnessInputMessage::ExtensionNoticeRequest(notice) = &notices[0] else {
        panic!("expected extension notice");
    };
    assert_eq!(notice.level, NoticeLevel::Warning);
    assert!(notice.message.contains("`rg`"));
    assert!(notice.message.contains("`grep`"));
    assert!(notice.message.contains("PATH"));
    assert_eq!(missing_command_notices_on_path(None).len(), 1);
}

/// An executable ripgrep on a later PATH entry should not produce a
/// warning.
#[cfg(unix)]
#[test]
fn executable_ripgrep_on_path_needs_no_warning() {
    use std::os::unix::fs::PermissionsExt;

    let first = tempfile::tempdir().expect("first PATH entry");
    let second = tempfile::tempdir().expect("second PATH entry");
    let rg = second.path().join("rg");
    std::fs::write(&rg, b"").expect("fixture executable");
    std::fs::set_permissions(&rg, Permissions::from_mode(0o755)).expect("executable permissions");
    let path = std::env::join_paths([first.path(), second.path()]).expect("fixture PATH");
    assert!(missing_command_notices_on_path(Some(&path)).is_empty());
}

/// A non-executable file or directory named rg cannot satisfy the grep
/// tool's executable dependency.
#[cfg(unix)]
#[test]
fn non_executable_ripgrep_is_missing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("fixture PATH");
    let rg = dir.path().join("rg");
    std::fs::write(&rg, b"").expect("fixture file");
    std::fs::set_permissions(&rg, Permissions::from_mode(0o644))
        .expect("non-executable permissions");
    assert_eq!(
        missing_command_notices_on_path(Some(dir.path().as_os_str())).len(),
        1
    );
    std::fs::remove_file(&rg).expect("remove file");
    std::fs::create_dir(&rg).expect("fixture directory");
    assert_eq!(
        missing_command_notices_on_path(Some(dir.path().as_os_str())).len(),
        1
    );
}
