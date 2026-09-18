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

fn result_field<'a>(result: &'a CborValue, name: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = result else {
        panic!("expected map");
    };
    entries
        .iter()
        .find_map(|(key, value)| (key == &CborValue::Text(name.to_owned())).then_some(value))
}

/// Default and both explicit single-range forms still accept dense sources.
/// Verify the saved at-cap prefix incrementally, without a giant full oracle.
#[test]
fn read_dense_single_ranges_preserve_output_and_saved_prefix() {
    use crate::shell_output_spool::MAX_SAVED_OUTPUT_BYTES;
    use crate::truncate::MAX_OUTPUT_BYTES;

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("dense");
    std::fs::write(&path, vec![b'\n'; MAX_READ_FILE_BYTES]).expect("write at-cap dense input");
    for extra in [
        vec![],
        vec![("start_line", CborValue::Integer(1.into()))],
        vec![(
            "end_line",
            CborValue::Integer((MAX_READ_FILE_BYTES as i64 + 1).into()),
        )],
        vec![(
            "ranges",
            CborValue::Array(vec![map(vec![
                ("start_line", CborValue::Integer(1.into())),
                (
                    "end_line",
                    CborValue::Integer((MAX_READ_FILE_BYTES as i64 + 1).into()),
                ),
            ])]),
        )],
    ] {
        let mut args = vec![("path", CborValue::Text(path.display().to_string()))];
        args.extend(extra);
        let output =
            read_file(&map(args), &mut ShellWorld::real()).expect("read accepted single range");
        let result = &output.result;
        assert_eq!(
            result_field(result, "truncated"),
            Some(&CborValue::Bool(true))
        );
        for field in ["total_lines", "total_bytes"] {
            assert_eq!(
                result_field(result, field),
                Some(&CborValue::Integer((MAX_READ_FILE_BYTES as i64).into()))
            );
        }
        assert!(result_field(result, "valid_utf8").is_none());
        assert!(result_field(result, "full_output_path").is_none());
        assert_eq!(
            result_field(result, "saved_output_truncated"),
            Some(&CborValue::Bool(true))
        );
        assert_eq!(
            result_field(result, "saved_output_bytes"),
            Some(&CborValue::Integer((MAX_SAVED_OUTPUT_BYTES as i64).into()))
        );
        let Some(CborValue::Text(content)) = result_field(result, "line-numbered content") else {
            panic!("missing visible content");
        };
        assert!(content.len() <= MAX_OUTPUT_BYTES);
        assert!(content.starts_with("1 \n2 \n"));
        assert!(content.contains("\n...\n"));
        assert!(content.ends_with(&format!("{MAX_READ_FILE_BYTES} ")));
        let Some(CborValue::Text(saved_path)) = result_field(result, "saved_output_path") else {
            panic!("missing saved prefix");
        };
        let saved = std::fs::read(saved_path).expect("read saved prefix");
        assert_eq!(saved.len(), MAX_SAVED_OUTPUT_BYTES);
        let mut offset = 0;
        for number in 1..=MAX_READ_FILE_BYTES {
            let record = if number == 1 {
                "1 ".to_owned()
            } else {
                format!("\n{number} ")
            };
            let take = record.len().min(saved.len() - offset);
            assert_eq!(&saved[offset..offset + take], &record.as_bytes()[..take]);
            offset += take;
            if offset == saved.len() {
                break;
            }
        }
        assert_eq!(offset, saved.len());
    }
}

/// Whole-source UTF-8 validity and total-field presence are independent of the
/// selected range, and a past-EOF request remains an error rather than output.
#[test]
fn read_single_range_preserves_validation_and_metadata_presence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("source");
    std::fs::write(&path, b"\xff\nok\n").expect("write invalid unselected line");
    let args = |start| {
        map(vec![
            ("path", CborValue::Text(path.display().to_string())),
            ("start_line", CborValue::Integer(start)),
        ])
    };
    let output =
        read_file(&args(2.into()), &mut ShellWorld::real()).expect("read valid selected line");
    assert_eq!(
        result_field(&output.result, "valid_utf8"),
        Some(&CborValue::Bool(false))
    );
    assert_eq!(
        result_field(&output.result, "line-numbered content"),
        Some(&CborValue::Text("2 ok".to_owned()))
    );
    for field in [
        "total_lines",
        "total_bytes",
        "truncated",
        "full_output_path",
        "saved_output_path",
        "truncation_warning",
    ] {
        assert!(result_field(&output.result, field).is_none(), "{field}");
    }
    let error = read_file(&args(3.into()), &mut ShellWorld::real()).expect_err("reject past EOF");
    assert!(error.message.contains("past end of file (total_lines: 2)"));
}
