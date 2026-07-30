use std::ffi as path_std_ffi;

use super::*;
use crate::tools::world as path_crate_tools_world;
use crate::truncate::MAX_OUTPUT_BYTES;

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                Some(value.as_str())
            }
            _ => None,
        })
}

fn cbor_map_bool(value: &CborValue, key: &str) -> Option<bool> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Bool(value)) if entry_key == key => {
                Some(*value)
            }
            _ => None,
        })
}

fn cbor_map_int(value: &CborValue, key: &str) -> Option<i64> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Integer(value)) if entry_key == key => {
                i128::from(*value).try_into().ok()
            }
            _ => None,
        })
}

fn ls_args(path: &std::path::Path) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )])
}

#[test]
fn ls_rejects_wrong_type_path() {
    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Integer(1.into()),
    )]);
    let mut world = path_crate_tools_world::ShellWorld::real();

    let err = run_ls(&args, &mut world).expect_err("integer path should be rejected");

    assert_eq!(err.message, "argument `path` must be a string");
}

#[test]
fn empty_ls_display_uses_zero_line_and_byte_stats() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&ls_args(tempdir.path()), &mut world).expect("ls output");

    assert!(output.display.info_chips.is_empty());
    assert_eq!(output.display.stats.lines, Some(0));
    assert_eq!(output.display.stats.bytes, Some(0));
    assert_eq!(cbor_map_text(&output.result, "output"), Some(""));
    assert!(cbor_map_int(&output.result, "total_lines").is_none());
    assert!(cbor_map_int(&output.result, "total_bytes").is_none());
}

#[test]
fn ls_display_uses_line_and_byte_stats_instead_of_entry_chip() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha"), "a").expect("write alpha");
    std::fs::write(tempdir.path().join("beta"), "b").expect("write beta");

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&ls_args(tempdir.path()), &mut world).expect("ls output");

    assert!(output.display.info_chips.is_empty());
    assert_eq!(output.display.stats.lines, Some(2));
    assert_eq!(
        output.display.stats.bytes,
        Some("1 alpha\n2 beta".len() as u64)
    );
    let text = cbor_map_text(&output.result, "output").expect("output");
    assert_eq!(text, "1 alpha\n2 beta");
}

#[test]
fn ls_escapes_line_breaks_and_control_characters_in_names() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("line\nbreak"), "a").expect("write newline name");
    std::fs::write(tempdir.path().join("tab\tname"), "b").expect("write tab name");
    std::fs::write(tempdir.path().join("back\\slash"), "c").expect("write backslash name");
    std::fs::write(tempdir.path().join("carriage\rreturn"), "d")
        .expect("write carriage return name");
    std::fs::write(tempdir.path().join("escape\u{1b}char"), "e").expect("write escape char name");
    std::fs::create_dir(tempdir.path().join("dir\nname")).expect("create escaped dir");

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&ls_args(tempdir.path()), &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert_eq!(text.lines().count(), 6);
    assert!(text.contains("(escaped) line\\nbreak"));
    assert!(text.contains("(escaped) tab\\tname"));
    assert!(text.contains("(escaped) back\\\\slash"));
    assert!(text.contains("(escaped) carriage\\rreturn"));
    assert!(text.contains("(escaped) escape\\u{1b}char"));
    assert!(text.contains("(escaped) dir\\nname/"));
    assert!(!text.contains("line\nbreak"));
}

#[cfg(unix)]
#[test]
fn ls_marks_invalid_utf8_names_and_shows_replacement_characters() {
    use std::os::unix::ffi::OsStringExt;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let invalid_name = path_std_ffi::OsString::from_vec(vec![b'a', 0xff, b'b']);
    std::fs::write(tempdir.path().join(invalid_name), "a").expect("write invalid name");

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&ls_args(tempdir.path()), &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert_eq!(text, "1(invalid-utf8) a�b");
}

#[test]
fn ls_rejects_non_positive_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let failure = run_ls(&args, &mut world).expect_err("limit should fail");

    assert_eq!(failure.message, "limit must be >= 1");
}

#[test]
fn ls_rejects_limit_above_output_cap() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer((MAX_LS_LIMIT as i64 + 1).into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let failure = run_ls(&args, &mut world).expect_err("limit should fail");

    assert_eq!(failure.message, format!("limit must be <= {MAX_LS_LIMIT}"));
}

/// Ensures limit truncation reports an explicit lower-bound marker instead of
/// presenting capped entry counts as exact total line/byte metadata.
#[test]
fn ls_limit_truncation_reports_limit_reached_without_exact_totals() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha"), "a").expect("write alpha");
    std::fs::write(tempdir.path().join("beta"), "b").expect("write beta");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&args, &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert_eq!(text, "1 alpha");
    assert_eq!(cbor_map_int(&output.result, "entries"), Some(1));
    assert_eq!(cbor_map_bool(&output.result, "truncated"), Some(true));
    assert_eq!(cbor_map_bool(&output.result, "limit_reached"), Some(true));
    assert_eq!(cbor_map_int(&output.result, "total_lines"), None);
    assert_eq!(cbor_map_int(&output.result, "total_bytes"), None);
}

/// Ensures ls only asks the world for one entry past the requested limit, which
/// bounds directory-entry collection while still detecting that the limit was
/// reached.
#[test]
fn ls_limit_bounds_world_directory_collection() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha"), "a").expect("write alpha");
    std::fs::write(tempdir.path().join("beta"), "b").expect("write beta");
    std::fs::write(tempdir.path().join("gamma"), "g").expect("write gamma");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&args, &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert_eq!(text.lines().count(), 1);
    assert_eq!(cbor_map_int(&output.result, "entries"), Some(1));
    assert_eq!(cbor_map_bool(&output.result, "truncated"), Some(true));
    assert_eq!(cbor_map_bool(&output.result, "limit_reached"), Some(true));
    assert_eq!(cbor_map_int(&output.result, "total_lines"), None);
}

/// Ensures byte-truncated listings expose exact totals and a readable complete
/// native rendering through the saved-output path.
#[test]
fn ls_byte_budget_truncation_reports_standard_total_headers() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    for index in 0..700 {
        let name = format!("entry-{index:04}-{}", "x".repeat(90));
        std::fs::write(tempdir.path().join(name), "x").expect("write entry");
    }
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(700.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&args, &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert!(text.len() <= MAX_OUTPUT_BYTES);
    assert_eq!(cbor_map_bool(&output.result, "truncated"), Some(true));
    assert_eq!(cbor_map_int(&output.result, "total_lines"), Some(700));
    assert!(
        (MAX_OUTPUT_BYTES as i64)
            < cbor_map_int(&output.result, "total_bytes").expect("total bytes")
    );
    let path = cbor_map_text(&output.result, "full_output_path").expect("saved path");
    let saved = std::fs::read_to_string(path).expect("saved listing");
    assert!(MAX_OUTPUT_BYTES < saved.len());
}

/// Ensures line-count truncation retains native head/separator/tail context and
/// reports complete totals.
#[test]
fn ls_line_count_truncation_keeps_head_tail_separator_and_totals() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    for index in 0..2001 {
        std::fs::write(tempdir.path().join(format!("entry-{index:04}")), "x").expect("write entry");
    }
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(MAX_LS_LIMIT.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = run_ls(&args, &mut world).expect("ls output");
    let text = cbor_map_text(&output.result, "output").expect("output");

    assert!(text.contains("\n...\n"));
    assert_eq!(cbor_map_bool(&output.result, "truncated"), Some(true));
    assert_eq!(cbor_map_int(&output.result, "total_lines"), Some(2001));
    assert!(text.lines().count() <= 2001);
}
