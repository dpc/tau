use std::error::Error as _;
use std::ffi::OsString;
use std::io::{ErrorKind, Read as _};

use tempfile::TempDir;

/// Ensures the façade preserves categorical matching while pinning the
/// contextual outer error's intentional loss of raw errno and public source.
#[test]
fn missing_file_preserves_kind_but_not_outer_raw_os_error() {
    let temp = TempDir::new().expect("temporary directory");
    let path = temp.path().join("missing");

    let error = super::read(&path).expect_err("missing file must fail");

    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert_eq!(error.raw_os_error(), None);
    assert!(error.source().is_none());
}

/// Ensures direct display retains the OS reason and names both the attempted
/// operation and supplied path, which is the façade's primary contract.
#[test]
fn display_names_operation_and_path() {
    let temp = TempDir::new().expect("temporary directory");
    let path = temp.path().join("missing");
    let os_reason = std::fs::read(&path)
        .expect_err("missing file must fail without the façade")
        .to_string();

    let display = super::read(&path)
        .expect_err("missing file must fail")
        .to_string();

    assert!(display.contains("failed to open file"));
    assert!(display.contains(&path.display().to_string()));
    assert!(display.contains(&os_reason));
}

/// Ensures two-path operations retain both endpoints instead of reporting only
/// the underlying operating-system reason.
#[test]
fn two_path_display_names_source_and_destination() {
    let temp = TempDir::new().expect("temporary directory");
    let source = temp.path().join("missing-source");
    let destination = temp.path().join("destination");

    let display = super::rename(&source, &destination)
        .expect_err("missing source must fail")
        .to_string();

    assert!(display.contains("failed to rename file from"));
    assert!(display.contains(&source.display().to_string()));
    assert!(display.contains(&destination.display().to_string()));
}

/// Ensures errors from I/O after a successful open retain the path stored by
/// the façade's file-handle wrapper.
#[test]
fn file_handle_io_display_retains_opened_path() {
    let temp = TempDir::new().expect("temporary directory");
    let path = temp.path().join("write-only");
    let mut file = super::File::create(&path).expect("create write-only file handle");
    let mut bytes = Vec::new();

    let error = file
        .read_to_end(&mut bytes)
        .expect_err("reading a write-only file handle must fail");

    assert!(error.to_string().contains(&path.display().to_string()));
}

/// Ensures callers do not mistake diagnostic display for byte-faithful or
/// terminal-safe path rendering.
#[cfg(unix)]
#[test]
fn non_utf8_and_control_bytes_are_lossy_and_unescaped() {
    use std::os::unix::ffi::OsStringExt as _;

    let temp = TempDir::new().expect("temporary directory");
    let name = OsString::from_vec(b"missing-\xff\nname".to_vec());
    let path = temp.path().join(name);

    let display = super::read(&path)
        .expect_err("missing file must fail")
        .to_string();

    assert!(display.contains('\u{fffd}'));
    assert!(display.contains('\n'));
}
