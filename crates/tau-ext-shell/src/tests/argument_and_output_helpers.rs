//! Tests for argument and output helpers behavior.

use super::*;

#[test]
fn run_grep_rejects_string_bool_argument() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\n").expect("write a");

    let args = grep_args(
        "alpha",
        &tempdir.path().display().to_string(),
        vec![(
            CborValue::Text("ignoreCase".to_owned()),
            CborValue::Text("True".to_owned()),
        )],
    );
    let err = run_grep(&args).expect_err("string bool should fail");

    assert_eq!(err.message, "argument `ignoreCase` must be a boolean");
}

#[test]
fn read_file_rejects_invalid_line_arguments() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("start_line=0 should fail")
            .message,
        "start_line must be >= 1"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("end_line=0 should fail")
            .message,
        "end_line must be >= 1"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(3.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("end_line before start_line should fail")
            .message,
        "end_line must be >= start_line"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("line_count should be rejected")
            .message,
        "line_count is no longer supported; use end_line"
    );
}

#[test]
fn optional_argument_bool_rejects_present_non_bool_values() {
    let args = CborValue::Map(vec![(
        CborValue::Text("ignoreCase".to_owned()),
        CborValue::Text("True".to_owned()),
    )]);

    let err = optional_argument_bool(&args, "ignoreCase").expect_err("non-bool should fail");

    assert_eq!(err, "argument `ignoreCase` must be a boolean");
}

#[test]
fn optional_argument_text_rejects_present_non_string_values() {
    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Integer(123.into()),
    )]);

    let err = optional_argument_text(&args, "path").expect_err("non-string should fail");

    assert_eq!(err, "argument `path` must be a string");
}

/// Protects the scheduler admission path from allocating/debug-formatting huge
/// CBOR arguments before applying the queued-byte budget.
#[test]
fn approximate_tool_bytes_caps_large_cbor_without_debug_rendering() {
    let Event::ToolStarted(invoke) = tool_started(
        "large-args",
        SHELL_TOOL_NAME,
        CborValue::Bytes(vec![0; crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1024]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };

    assert_eq!(
        approximate_tool_bytes(&invoke, crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT),
        crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1
    );
    let raised_estimate =
        approximate_tool_bytes(&invoke, crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT * 2);
    assert!(raised_estimate > crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1024);
    assert!(raised_estimate < crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT * 2);
}
#[test]
fn slice_lines_returns_requested_window() {
    let sliced = slice_lines("a\nb\nc\nd", 2, Some(3));
    assert_eq!(sliced.content, "2 b\n3 c");
    assert_eq!(sliced.line_count, 2);
}

#[test]
fn slice_lines_clamps_past_end() {
    let sliced = slice_lines("a\nb\nc", 10, Some(14));
    assert_eq!(sliced.content, "");
    assert_eq!(sliced.line_count, 0);
}
#[test]
fn shell_tool_ignores_legacy_wrong_type_mode_argument() {
    // `mode` is no longer part of the schema. If a stale caller sends it
    // anyway, shell execution is controlled by ext-shell's inferred mode
    // argument.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Integer(1.into()),
        ),
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("legacy mode ignored") else {
        panic!("expected finished shell outcome");
    };
    assert_eq!(output.display.mode, "");
}
