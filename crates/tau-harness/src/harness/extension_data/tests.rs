use std::fs as path_std_fs;

use super::*;

/// Ensures extension data reads reject oversized files before allocating the
/// whole contents on the harness request path.
#[test]
fn read_file_rejects_files_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("too-large.bin");
    path_std_fs::File::create(&file_path)
        .expect("create file")
        .set_len(MAX_EXTENSION_DATA_FILE_BYTES + 1)
        .expect("make sparse oversized file");

    let err = run_extension_data_read_file(tempdir.path(), "too-large.bin".to_owned())
        .expect_err("oversized read must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}

/// Ensures extension data writes refuse payloads that would exceed the
/// harness-enforced disk quota for a single extension-owned file.
#[test]
fn write_file_rejects_payloads_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let contents = vec![0; MAX_EXTENSION_DATA_FILE_BYTES as usize + 1];

    let err = run_extension_data_write_file(tempdir.path(), "too-large.bin".to_owned(), contents)
        .expect_err("oversized write must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
    assert!(!tempdir.path().join("too-large.bin").exists());
}

/// Ensures exclusive create enforces the same single-file quota as replace
/// writes and leaves no destination file after refusing the payload.
#[test]
fn create_file_rejects_payloads_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let contents = vec![0; MAX_EXTENSION_DATA_FILE_BYTES as usize + 1];

    let err = run_extension_data_create_file(tempdir.path(), "too-large.bin".to_owned(), contents)
        .expect_err("oversized create must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
    assert!(!tempdir.path().join("too-large.bin").exists());
}

/// Ensures appending to an existing file cannot grow extension data beyond the
/// single-file quota even when each individual append request is small.
#[test]
fn append_file_rejects_growth_beyond_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("nearly-full.bin");
    path_std_fs::File::create(&file_path)
        .expect("create file")
        .set_len(MAX_EXTENSION_DATA_FILE_BYTES)
        .expect("make sparse quota-sized file");

    let err = run_extension_data_append_file(tempdir.path(), "nearly-full.bin".to_owned(), vec![0])
        .expect_err("append beyond quota must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}

/// Ensures directory listing has a hard collection cap before sorting entries
/// for extension-controlled data.
#[test]
fn list_files_rejects_directories_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    for index in 0..=MAX_EXTENSION_DATA_LIST_ENTRIES {
        std::fs::write(tempdir.path().join(format!("entry-{index}")), b"")
            .expect("create list entry");
    }

    let err = run_extension_data_list_files(tempdir.path(), String::new())
        .expect_err("oversized list must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}
