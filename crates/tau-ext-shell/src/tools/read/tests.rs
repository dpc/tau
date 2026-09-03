use super::*;

fn map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

/// Ensures optional read line arguments reject wrong CBOR types instead of
/// silently falling back to the default range.
#[test]
fn read_rejects_wrong_type_optional_line_arguments() {
    let err = parse_read_request(&map(vec![("start_line", CborValue::Text("2".to_owned()))]))
        .expect_err("string start_line should be rejected");

    assert_eq!(err.message, "argument `start_line` must be an integer");
}

/// Ensures range entries reject wrong CBOR line types instead of reporting
/// them as missing integer fields.
#[test]
fn read_ranges_reject_wrong_type_line_arguments() {
    let err = parse_read_request(&map(vec![(
        "ranges",
        CborValue::Array(vec![map(vec![
            ("start_line", CborValue::Text("1".to_owned())),
            ("end_line", CborValue::Integer(2.into())),
        ])]),
    )]))
    .expect_err("string range start_line should be rejected");

    assert_eq!(err.message, "argument `start_line` must be an integer");
}

/// Ensures successful parsing retains validated nonzero coordinates without
/// changing the original range display that the read tool reports.
#[test]
fn read_ranges_retain_validated_line_coordinates_and_display() {
    let request = parse_read_request(&map(vec![(
        "ranges",
        CborValue::Array(vec![map(vec![
            ("start_line", CborValue::Integer(2.into())),
            ("end_line", CborValue::Integer(3.into())),
        ])]),
    )]))
    .expect("positive range should parse");

    assert_eq!(request.ranges.len(), 1);
    assert_eq!(request.ranges[0].start_line.get(), 2);
    assert_eq!(request.ranges[0].end_line.map(LineNumber::get), Some(3));
    assert_eq!(request.display_ranges, vec!["2..3"]);
}
/// Ensures the read tool refuses inputs above its safety cap before loading
/// the whole file into memory.
#[test]
fn read_rejects_files_over_input_cap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("huge.txt");
    std::fs::write(&path, vec![b'x'; MAX_READ_FILE_BYTES + 1]).expect("write huge file");
    let mut world = ShellWorld::real();

    let err = read_file(
        &map(vec![("path", CborValue::Text(path.display().to_string()))]),
        &mut world,
    )
    .expect_err("huge file should be rejected");

    assert!(
        err.message.contains("file is too large to read safely"),
        "unexpected error: {}",
        err.message
    );
}

/// Ensures missing-path diagnostics give a nearby sibling only when the
/// sibling scan stays within the hard bound, preventing expensive or
/// nondeterministic directory-wide suggestions.
#[test]
fn read_missing_path_suggestion_is_bounded() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("target_file.rs"), "content").expect("write target");
    let mut world = ShellWorld::real();

    let err = read_file(
        &map(vec![(
            "path",
            CborValue::Text(temp.path().join("target_fiel.rs").display().to_string()),
        )]),
        &mut world,
    )
    .expect_err("misspelled path should fail");

    assert!(
        err.message.contains("did you mean") && err.message.contains("target_file.rs"),
        "unexpected error: {}",
        err.message
    );

    for idx in 0..=MAX_PATH_SUGGESTION_SIBLINGS {
        std::fs::write(temp.path().join(format!("sibling-{idx}.txt")), "x").expect("write sibling");
    }
    let mut world = ShellWorld::real();
    let err = read_file(
        &map(vec![(
            "path",
            CborValue::Text(temp.path().join("target_fiel.rs").display().to_string()),
        )]),
        &mut world,
    )
    .expect_err("misspelled path should still fail");

    assert!(
        !err.message.contains("did you mean"),
        "suggestion should be suppressed past sibling bound: {}",
        err.message
    );
}

/// Ensures overlapping multi-range reads cannot expand a modest input into
/// very large intermediate rendered strings before normal output
/// truncation.
#[test]
fn read_rejects_multi_range_render_expansion_over_cap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("wide.txt");
    std::fs::write(&path, "x".repeat(32 * 1024)).expect("write wide file");
    let ranges = (0..100)
        .map(|_| {
            map(vec![
                ("start_line", CborValue::Integer(1.into())),
                ("end_line", CborValue::Integer(1.into())),
            ])
        })
        .collect::<Vec<_>>();
    let mut world = ShellWorld::real();

    let err = read_file(
        &map(vec![
            ("path", CborValue::Text(path.display().to_string())),
            ("ranges", CborValue::Array(ranges)),
        ]),
        &mut world,
    )
    .expect_err("range expansion should be rejected");

    assert!(
        err.message
            .contains("read ranges expand to too much rendered content"),
        "unexpected error: {}",
        err.message
    );
}
