use super::*;

fn request(path: &std::path::Path, edits: &[(&str, &str)]) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(
                edits
                    .iter()
                    .map(|(old_text, new_text)| {
                        CborValue::Map(vec![
                            (
                                CborValue::Text("oldText".to_owned()),
                                CborValue::Text((*old_text).to_owned()),
                            ),
                            (
                                CborValue::Text("newText".to_owned()),
                                CborValue::Text((*new_text).to_owned()),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// Ensures one call validates all snapshot matches before writing, so a later
/// failed edit cannot leave an earlier replacement on disk.
#[test]
fn replace_is_atomic_when_a_later_target_is_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    std::fs::write(&path, "one\ntwo\n").expect("write source");
    let mut world = ShellWorld::real();

    let error = replace_file(
        &request(&path, &[("one", "ONE"), ("missing", "MISSING")]),
        &mut world,
    )
    .expect_err("missing target must fail");

    assert_eq!(error.message, "each oldText must match exactly once");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read source"),
        "one\ntwo\n"
    );
}

/// Ensures matching normalizes line endings and preserves BOM plus unrelated
/// mixed-ending bytes while inserted text inherits its target's ending.
#[test]
fn replace_preserves_bom_and_untouched_mixed_line_endings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    std::fs::write(&path, b"\xef\xbb\xbffirst\r\nsecond\rthird\n").expect("write source");
    let mut world = ShellWorld::real();

    replace_file(
        &request(&path, &[("first\nsecond", "ONE\nTWO")]),
        &mut world,
    )
    .expect("replace normalized target");

    assert_eq!(
        std::fs::read(&path).expect("read result"),
        b"\xef\xbb\xbfONE\r\nTWO\rthird\n"
    );
}

/// Ensures a replacement after a CRLF line inherits that logical ending once,
/// rather than treating the LF byte inside CRLF as a closer standalone ending.
#[test]
fn replace_uses_nearby_crlf_for_a_target_without_an_ending() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    std::fs::write(&path, b"before\r\ntarget").expect("write source");
    let mut world = ShellWorld::real();

    replace_file(&request(&path, &[("target", "one\ntwo")]), &mut world).expect("replace target");

    assert_eq!(
        std::fs::read(&path).expect("read result"),
        b"before\r\none\r\ntwo"
    );
}

/// Ensures duplicate and overlapping snapshot targets fail without exposing
/// request text or modifying the existing file.
#[test]
fn replace_rejects_non_unique_and_overlapping_targets_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    std::fs::write(&path, "abcdef").expect("write source");
    let mut world = ShellWorld::real();
    let duplicate = replace_file(&request(&path, &[("a", "A"), ("a", "A")]), &mut world)
        .expect_err("duplicate targets overlap");
    assert_eq!(duplicate.message, "replacement targets overlap");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read source"),
        "abcdef"
    );

    let mut world = ShellWorld::real();
    let overlapping = replace_file(&request(&path, &[("abc", "A"), ("bcd", "B")]), &mut world)
        .expect_err("overlapping targets must fail");
    assert_eq!(overlapping.message, "replacement targets overlap");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read source"),
        "abcdef"
    );
}

/// Ensures a textual no-op returns compact result metadata without writing a
/// diff payload, avoiding a spurious durable display event.
#[test]
fn replace_noop_has_no_diff_payload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    std::fs::write(&path, "same\n").expect("write source");
    let mut world = ShellWorld::real();

    let output =
        replace_file(&request(&path, &[("same", "same")]), &mut world).expect("no-op replacement");

    assert_eq!(output.display.payload, None);
    assert_eq!(
        output.result,
        CborValue::Map(vec![
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("changed".to_owned()),
                CborValue::Bool(false)
            ),
            (
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer(5.into()),
            ),
        ])
    );
}

/// Ensures strict local parsing rejects unrecognized fields rather than
/// accepting legacy aliases or JSON-string preprocessing.
#[test]
fn replace_rejects_unknown_request_fields() {
    let request = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(Vec::new()),
        ),
        (
            CborValue::Text("oldText".to_owned()),
            CborValue::Text("legacy".to_owned()),
        ),
    ]);

    let error = ReplaceRequest::parse(&request).expect_err("legacy field must fail");

    assert_eq!(error.message, "request contains an unknown field");
}

/// Ensures invalid UTF-8 rejects before matching or writing so replacement
/// semantics never lossy-decode bytes that must remain untouched.
#[test]
fn replace_rejects_invalid_utf8_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    let source = b"before\xffafter";
    std::fs::write(&path, source).expect("write source");
    let mut world = ShellWorld::real();

    let error = replace_file(&request(&path, &[("before", "after")]), &mut world)
        .expect_err("invalid UTF-8 must fail");

    assert_eq!(error.message, "file is not valid UTF-8");
    assert_eq!(std::fs::read(&path).expect("read source"), source);
}

/// Ensures the bounded read authority rejects an oversized existing file before
/// matching and leaves its contents intact.
#[test]
fn replace_rejects_oversized_file_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source.txt");
    let source = vec![b'x'; MAX_SAFE_FILE_READ_BYTES + 1];
    std::fs::write(&path, &source).expect("write source");
    let mut world = ShellWorld::real();

    let error = replace_file(&request(&path, &[("x", "y")]), &mut world)
        .expect_err("oversized file must fail");

    assert_eq!(error.message, "file could not be read");
    assert_eq!(std::fs::read(&path).expect("read source"), source);
}
