use std::error::Error as _;
use std::io::ErrorKind;

use tempfile::TempDir;

/// Ensures a fixture-owned invalid parent produces a self-locating error
/// without relying on host mount permissions or a read-only build sandbox.
#[test]
fn temporary_directory_failure_names_operation_and_parent() {
    let root = TempDir::new().expect("test-owned temporary root");
    let failing_parent = root.path().join("regular-file-parent");
    std::fs::write(&failing_parent, b"not a directory").expect("create failing parent");

    let error = super::create_fixture_tempdir_in(&failing_parent)
        .expect_err("a regular file cannot contain a temporary directory");

    let source = error
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("fixture error retains its io::Error source");
    assert_eq!(source.kind(), ErrorKind::NotADirectory);
    let display = error.to_string();
    assert!(display.contains("failed to create fixture temporary directory in parent"));
    assert!(display.contains(&failing_parent.display().to_string()));
}

/// Ensures the mode-preserving directory builder adds operation and path
/// context while retaining the underlying categorical error.
#[test]
fn private_directory_failure_names_operation_and_path() {
    let root = TempDir::new().expect("test-owned temporary root");
    let failing_parent = root.path().join("regular-file-parent");
    std::fs::write(&failing_parent, b"not a directory").expect("create failing parent");
    let path = failing_parent.join("private");

    let error =
        super::create_private_directory(&path).expect_err("directory below a file must fail");

    assert_eq!(error.kind(), ErrorKind::NotADirectory);
    let display = error.to_string();
    assert!(display.contains("failed to create private fixture directory"));
    assert!(display.contains(&path.display().to_string()));
}
