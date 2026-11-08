use std::path::Path;

use tau_proto::CborValue;

use super::{output, status_output};

/// Successful reads omit the routine availability chip while retaining
/// semantic status.
#[test]
fn available_read_uses_path_only_display() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let output = status_output(Some(dir.path()));

    assert_eq!(output.display.mode, "get");
    assert_eq!(output.display.args, dir.path().display().to_string());
    assert!(matches!(
        output.result,
        CborValue::Map(ref entries)
            if entries.contains(&(
                CborValue::Text("status".to_owned()),
                CborValue::Text("available".to_owned()),
            ))
    ));
}

/// Successful setters preserve the committed canonical path in compact
/// metadata without adding an empty payload region.
#[test]
fn setter_uses_path_only_display_and_canonical_receipt() {
    let path = Path::new("/tmp/result");
    let output = output(path);

    assert_eq!(output.display.mode, "set");
    assert_eq!(output.display.args, "/tmp/result");
    assert_eq!(
        tau_proto::ToolResponse::from_cbor(&output.result).render(),
        "status: changed\npath: /tmp/result\n"
    );
}

/// Missing and invalid persisted values retain compact, explicit non-happy
/// statuses without changing the established semantic status vocabulary.
#[test]
fn unavailable_reads_show_compact_status() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let missing = dir.path().join("missing");

    let missing_output = status_output(Some(&missing));
    assert_eq!(
        missing_output.display.args,
        format!("{} (missing)", missing.display())
    );
    assert!(matches!(
        missing_output.result,
        CborValue::Map(ref entries)
            if entries.contains(&(
                CborValue::Text("status".to_owned()),
                CborValue::Text("unavailable".to_owned()),
            ))
    ));
    assert_eq!(status_output(None).display.args, "<invalid> (invalid)");
}

/// A persisted regular file is distinguished from a missing directory.
#[test]
fn non_directory_read_has_specific_status() {
    let file = tempfile::NamedTempFile::new().expect("temp file");
    let output = status_output(Some(file.path()));

    assert_eq!(
        output.display.args,
        format!("{} (not-directory)", file.path().display())
    );
}
