//! Tests for filesystem tools behavior.

use super::*;

#[test]
fn edit_adds_missing_line_ending_before_following_content() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\nafter\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement without line ending should be normalized")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nreplacement\nafter\n"
    );
}

#[test]
fn edit_preserves_final_newline_at_end_of_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("last line replacement should preserve final newline")
    .result;

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nreplacement\n"
    );
}

#[test]
fn edit_preserves_original_crlf_when_adding_missing_line_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"before\r\ntarget\r\nafter\r\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement should reuse original line ending")
    .result;
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"before\r\nreplacement\r\nafter\r\n"
    );
}

#[test]
fn edit_noop_after_normalization_reports_unchanged() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "a\nb\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "a", "a")],
    ))
    .expect("normalization may make an edit a no-op")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(false));
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), "a\nb\n");
}

#[test]
fn edit_deletion_before_following_content_does_not_add_newline_header() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\nafter\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "", "target")],
    ))
    .expect("deletion should not be normalized")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nafter\n"
    );
}

#[test]
fn edit_preserves_original_cr_when_adding_missing_line_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"before\rtarget\rafter\r").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement should reuse original CR line ending")
    .result;
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"before\rreplacement\rafter\r"
    );
}
#[test]
fn edit_rejects_legacy_line_count() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(2.into())),
            ("line_count", CborValue::Integer(1.into())),
            ("newText", CborValue::Text("x".to_owned())),
        ])],
    ))
    .expect_err("line_count should fail");

    assert_eq!(
        error.message,
        "line_count is no longer supported; use end_line_exclusive"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_uses_original_line_numbers_for_multiple_replacements() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "a\nb\nc\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![
                    context_line_edit(1, 1, "x\ny\n", "a"),
                    context_line_edit(3, 3, "z\n", "c"),
                ],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(cbor_map_int(&result.result, "edits"), Some(2));
    assert_eq!(
        cbor_map_int(&result.result, "new_max_valid_start_line"),
        Some(5)
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "x\ny\nb\nz\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_replaces_exact_line_range() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\nfish\nfish\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_line_edit(2, 2, "cat\n", "fish")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    let expected_args = format!("{} 2..<3", file_path.display());
    assert_eq!(
        result.display.as_ref().map(|display| display.args.as_str()),
        Some(expected_args.as_str())
    );
    assert_eq!(cbor_map_int(&result.result, "edits"), Some(1));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\nfish\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_appends_to_line_after_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "cat\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\n"
    );
}

#[test]
fn edit_half_open_replaces_line_range() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\nthree\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 3, "TWO", "two")],
    ))
    .expect("boundary replacement should edit line 2")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn edit_half_open_inserts_at_top_and_middle() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\n").expect("write");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_half_open_edit(1, 1, "zero", "one"),
            context_half_open_edit(2, 2, "middle", "two"),
        ],
    ))
    .expect("empty half-open ranges should insert");

    assert_eq!(
        output.display.args,
        format!("{} 1..<1,2..<2", file_path.display())
    );

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none\nmiddle\ntwo\n"
    );
}

#[test]
fn edit_half_open_context_lines_empty_insertion_with_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "zero\n", "one")],
    ))
    .expect("empty insertion at BOF should accept start-line context_line");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none\n"
    );
}

#[test]
fn edit_half_open_appends_after_file_with_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "two\n", "")],
    ))
    .expect("EOF insertion should not add blank line after existing line ending");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_half_open_inserts_before_line_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "zero", "one")],
    ))
    .expect("insertion before unterminated content should stay line-oriented")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none"
    );
}

#[test]
fn edit_half_open_insertion_preserves_following_crlf() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"one\r\ntwo\r\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "middle", "two")],
    ))
    .expect("boundary insertion should use following line ending style");

    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"one\r\nmiddle\r\ntwo\r\n"
    );
}

#[test]
fn edit_half_open_appends_after_file_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "two\n", "")],
    ))
    .expect("EOF insertion should keep line boundary")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_half_open_creates_empty_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("missing.txt");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "hello\n", "")],
    ))
    .expect("half-open insertion should create missing file");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_legacy_start_line_and_end_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line", CborValue::Integer(1.into())),
            ("newText", CborValue::Text("x\n".to_owned())),
            ("context_line", CborValue::Text("one".to_owned())),
        ])],
    ))
    .expect_err("legacy edit ranges should fail");

    assert_eq!(
        error.message,
        "edit uses end_line_exclusive; to replace read output lines A through B, use start_line A and end_line_exclusive B+1"
    );
}

#[test]
fn edit_rejects_legacy_after_line_and_before_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("after_line", CborValue::Integer(0.into())),
            ("before_line", CborValue::Integer(2.into())),
            ("newText", CborValue::Text("x\n".to_owned())),
            ("context_line", CborValue::Text("one".to_owned())),
        ])],
    ))
    .expect_err("legacy boundary edit ranges should fail");

    assert_eq!(
        error.message,
        "after_line and before_line are no longer supported; use start_line and end_line_exclusive"
    );
}

#[test]
fn edit_replaces_empty_file_line_one() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "hello\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(2));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_end_line_exclusive_past_end() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(2, 4, "x", "")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(
        error
            .message
            .contains("end_line_exclusive 4 is past end of file")
    );
    assert!(error.message.contains("max_valid_start_line: 2"));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_range_past_end_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 2, "x", "hello")],
    ))
    .expect_err("range should fail");

    assert_eq!(
        error.message,
        "end_line_exclusive 3 is past end of file (max_valid_start_line: 2)"
    );
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), "hello");
}

#[test]
fn edit_context_line_allows_matching_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write");

    // Regression coverage: context_line must match start_line's original content,
    // not the previous line.
    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "BETA\n", "beta")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(4));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nBETA\ngamma\n"
    );
}

#[test]
fn edit_context_line_rejects_previous_line_for_replacement() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write");

    // Prevents restoring the rejected behavior where line 2 replacements used
    // line 1 as their context_line anchor.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "BETA\n", "alpha")],
    ))
    .expect_err("previous-line context should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 2, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(2));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nbeta\ngamma\n"
    );
}

#[test]
fn edit_context_line_rejects_stale_line_number_and_returns_context_line_context() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    let original = (1..=100)
        .map(|line| format!("line {line:03}\n"))
        .collect::<String>();
    fs::write(&file_path, &original).expect("write");

    // On mismatch, the file stays untouched and the agent gets read-like
    // details around the line whose context_line failed. This gives enough context
    // to recover from stale line numbers without dumping context into the UI
    // payload.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_line_edit(1, 1, "LINE 001\n", "line 001"),
            context_line_edit(12, 40, "replacement\n", "wrong"),
        ],
    ))
    .expect_err("context_line mismatch should fail");

    let expected_context = (2..=22)
        .map(|line| format!("{line} line {line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 12, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some(expected_context.as_str())
    );
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(12));
    assert_eq!(error.display.payload, None);
    assert_eq!(error.display.stats.lines, Some(21));
    assert_eq!(
        error.display.stats.bytes,
        Some(expected_context.len() as u64)
    );
    assert_eq!(error.display.stats.matches, None);
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), original);
}

#[test]
fn edit_rejects_non_empty_context_line_for_missing_file_insertion() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("missing.txt");

    // Missing files have no start_line content, so creation must use an empty
    // context_line.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "created\n", "not-empty")],
    ))
    .expect_err("non-empty context_line on missing-file insertion should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal \"\" for missing line 1, see current content in the response"
    );
    assert!(!file_path.exists());
}

#[test]
fn edit_rejects_wrong_context_line_at_file_start_with_recovery_context() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "ALPHA\n", "not-empty")],
    ))
    .expect_err("wrong context_line at file start should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 1, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(1));
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some("1 alpha\n2 beta")
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nbeta\n"
    );
}

#[test]
fn edit_empty_insertion_rejects_previous_line_as_context_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "middle\n", "one")],
    ))
    .expect_err("empty insertion should reject previous-line context");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 2, see current content in the response"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_rejects_missing_context_line_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![line_edit(1, 1, "ALPHA\n")],
    ))
    .expect_err("missing context_line should fail");

    assert_eq!(error.message, "each edit must have a string context_line");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\n"
    );
}

#[test]
fn edit_context_line_rejects_non_string_context_line_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(2.into())),
            ("newText", CborValue::Text("ALPHA\n".to_owned())),
            ("context_line", CborValue::Integer(1.into())),
        ])],
    ))
    .expect_err("non-string context_line should fail");

    assert_eq!(error.message, "context_line must be a string");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\n"
    );
}

#[test]
fn edit_context_line_trims_trailing_newline_characters() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");

    for context_line in ["beta\n", "beta\r", "beta\r\n", "beta\n\n"] {
        fs::write(&file_path, "alpha\nbeta\n").expect("write");
        edit_file(&edit_arguments(
            &file_path,
            vec![context_line_edit(2, 2, "BETA\n", context_line)],
        ))
        .expect("context_line with trailing newline should match");

        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "alpha\nBETA\n"
        );
    }
}

#[test]
fn edit_context_line_rejects_embedded_newline_characters_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\n").expect("write");

    // Context lines describe one line's content only, so embedded line endings are
    // malformed instead of being treated as ordinary mismatching text.
    for context_line in ["al\npha", "al\rpha", "al\r\npha"] {
        let error = edit_file(&edit_arguments(
            &file_path,
            vec![context_line_edit(2, 2, "BETA\n", context_line)],
        ))
        .expect_err("context_line with embedded newline should fail");

        assert_eq!(
            error.message,
            "context_line must not include embedded newline characters"
        );
        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "alpha\nbeta\n"
        );
    }
}
#[test]
fn edit_context_line_matches_crlf_line_without_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\r\ntwo\r\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "TWO\r\n", "two")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\r\nTWO\r\n"
    );
}

#[test]
fn edit_context_line_allows_empty_append_line_after_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "cat\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\n"
    );
}

#[test]
fn edit_handles_crlf_line_ranges() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\r\ntwo\r\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "TWO\r\n", "two")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\r\nTWO\r\n"
    );
}

#[test]
fn truncate_head_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_head(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

/// Ensures line-count truncation retains bounded native head/tail context and
/// explicit truncation state.
#[test]
fn truncate_head_limits_by_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("line 1\n"));
    assert!(result.content.contains("\n...\n"));
    assert!(
        result
            .content
            .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
    );
    assert!(result.content.lines().count() <= MAX_OUTPUT_LINES + 1);
}

/// Ensures combined line/byte truncation keeps a marker for a huge multibyte
/// head, the literal separator, and a distinctive tail record.
#[test]
fn truncate_head_keeps_multibyte_head_separator_and_tail() {
    let mut lines = vec![format!("1 {}", "€".repeat(MAX_OUTPUT_BYTES))];
    lines.extend((2..=MAX_OUTPUT_LINES).map(|line| format!("{line} x")));
    lines.push("2501 distinctive-tail".to_owned());

    let result = truncate_head(&lines.join("\n"));
    assert!(result.content.starts_with("1(truncated)"));
    assert!(result.content.contains("\n...\n"));
    assert!(result.content.ends_with("2501 distinctive-tail"));
    assert!(result.content.len() <= MAX_OUTPUT_BYTES);
}

#[test]
fn truncate_head_limits_by_bytes() {
    // Create input that's within line count but exceeds byte limit.
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\n{big_line}\nthird");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.starts_with("first"));
    assert!(result.content.contains("(truncated)"));
}

#[test]
fn grep_result_map_omits_request_context() {
    // The agent already knows the grep request arguments it sent. Do not echo
    // pattern/path/glob in the result headers; keep only execution outcome and
    // payload metadata.
    let result = grep_result_map(Some(0), 3, "src/a.rs:1:foo".to_owned());
    assert!(cbor_map_text(&result, "pattern").is_none());
    assert!(cbor_map_text(&result, "path").is_none());
    assert!(cbor_map_text(&result, "glob").is_none());
    assert_eq!(cbor_int_field(&result, "status"), Some(0));
    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
    assert_eq!(cbor_map_text(&result, "output"), Some("src/a.rs:1:foo"));
    assert_eq!(cbor_int_field(&result, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&result, "output_bytes"), Some(14));

    let no_matches = grep_result_map(Some(1), 0, "no matches found".to_owned());
    assert_eq!(cbor_int_field(&no_matches, "status"), Some(1));
    assert_eq!(cbor_int_field(&no_matches, "matches"), Some(0));
    assert_eq!(cbor_int_field(&no_matches, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&no_matches, "output_bytes"), Some(16));
}

#[test]
fn run_grep_counts_matches_across_directory() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\nbeta\nalpha\n").expect("write a");
    fs::write(tempdir.path().join("b.txt"), "alpha\n").expect("write b");

    let args = grep_args("alpha", &tempdir.path().display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
}

#[test]
fn run_grep_no_matches_uses_plain_ok_status() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\n").expect("write a");

    let args = grep_args("beta", &tempdir.path().display().to_string(), vec![]);
    let output = run_grep(&args).expect("grep");

    assert_eq!(output.display.status_text, "ok");
    assert_eq!(output.display.stats.matches, Some(0));
}

#[test]
fn run_grep_counts_matches_in_single_file() {
    // `path` may point at a single file; the renderer must still emit the
    // path heading and count every match in that file.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(&file, "alpha\nbeta\nalpha\ngamma\nalpha\n").expect("write");

    let args = grep_args("alpha", &file.display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains(&file.display().to_string()),
        "expected path heading, got: {output}"
    );
    assert!(
        output.contains("1:alpha"),
        "expected LINE:CONTENT match, got: {output}"
    );
}

#[test]
fn run_grep_with_context_counts_only_match_lines() {
    // Context lines (`LINE-CONTENT`) must not be counted as
    // matches. Search a single file so we also exercise the
    // single-file rendering path.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(
        &file,
        "filler 1\nfiller 2\nalpha\nfiller 3\nfiller 4\nalpha\nfiller 5\n",
    )
    .expect("write");

    let args = grep_args(
        "alpha",
        &file.display().to_string(),
        vec![(
            CborValue::Text("context".to_owned()),
            CborValue::Integer(1.into()),
        )],
    );
    let result = run_grep(&args).expect("grep").result;

    // Two matches; surrounding context lines are present in output
    // but must not inflate the count.
    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "pattern").is_none());
    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains("3:alpha"), "first match missing: {output}");
    assert!(output.contains("6:alpha"), "second match missing: {output}");
    assert!(
        output.contains("2-filler 2"),
        "context line missing: {output}"
    );
}

#[test]
fn truncate_tail_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_tail(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

#[test]
fn truncate_tail_keeps_last_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(
        result
            .content
            .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
    );
    assert!(result.content.contains("\n...\n"));
    assert!(result.content.contains("line 1\n"));
}

#[test]
fn truncate_tail_limits_by_bytes() {
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\nsecond\n{big_line}\nlast");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("last"));
    assert!(result.content.contains("(truncated)"));
}

#[test]
fn truncate_tail_keeps_suffix_for_one_huge_line() {
    // Regression coverage for an oversized single-line stream: tail truncation
    // used to keep zero lines and report an impossible `lines 2-1 of 1` range.
    let input = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let result = truncate_tail(&input);

    assert!(result.was_truncated);
    assert_eq!(result.content, "(truncated)");
}

#[test]
fn truncate_tail_keeps_suffix_for_huge_final_line() {
    // When the final line alone exceeds the byte cap, the useful tail is a
    // suffix of that line rather than an empty line range.
    let final_line = format!("{}TAIL", "x".repeat(MAX_OUTPUT_BYTES + 100));
    let input = format!("first\n{final_line}");
    let result = truncate_tail(&input);

    assert!(result.was_truncated);
    assert!(result.content.starts_with("first\n"));
    assert!(result.content.contains("(truncated)"));
    assert!(!result.content.contains("TAIL"));
}

#[test]
fn truncate_tail_preserves_utf8_boundary_for_huge_line_suffix() {
    // Byte fallback must never slice through a multibyte codepoint; otherwise
    // shell output truncation can panic or manufacture invalid UTF-8.
    let input = "€".repeat(MAX_OUTPUT_BYTES / "€".len() + 100);
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert_eq!(result.content, "(truncated)");
}

#[test]
fn read_file_honors_start_line_and_end_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "line 1\nline 2\nline 3\nline 4\nline 5\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(4.into()),
        ),
    ]);
    let output = read_file(&args).expect("read");
    let result = output.result;
    assert_eq!(output.display.args, format!("{} 2..4", path.display()));
    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("2 line 2\n3 line 3\n4 line 4")
    );
    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_clips_end_line_past_eof() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(99.into()),
        ),
    ]);

    let output = read_file(&args).expect("read");
    assert_eq!(output.display.args, format!("{} 2..99", path.display()));
    assert_eq!(
        cbor_map_text(&output.result, "line-numbered content"),
        Some("2 two\n3 three")
    );
}

#[test]
fn read_file_reads_multiple_disjoint_ranges_with_blank_separator() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(2, 3), read_range(5, 5)]),
        ),
    ]);
    let output = read_file(&args).expect("read");
    let result = output.result;

    assert_eq!(output.display.args, format!("{} 2..3,5..5", path.display()));
    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("2 two\n3 three\n\n5 five")
    );
    assert!(cbor_map_field(&result, "total_lines").is_none());
}

#[test]
fn read_file_allows_overlapping_ranges_with_redundant_chunks() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(1, 2), read_range(2, 3)]),
        ),
    ]);

    let output = read_file(&args).expect("overlap should be returned redundantly");
    assert_eq!(output.display.args, format!("{} 1..2,2..3", path.display()));
    assert_eq!(
        cbor_map_text(&output.result, "line-numbered content"),
        Some("1 one\n2 two\n\n2 two\n3 three")
    );
}

#[test]
fn read_file_rejects_range_request_over_cap_before_reading_file() {
    let ranges = (0..=100).map(|_| read_range(1, 1)).collect::<Vec<_>>();
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("/definitely/missing/read-target.txt".to_owned()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(ranges),
        ),
    ]);

    let error = read_file(&args).expect_err("read should reject arguments first");
    assert_eq!(error.message, "requested range count exceeds limit of 100");
}

#[test]
fn read_file_rejects_ranges_combined_with_top_level_range() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(1.into()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(1, 1)]),
        ),
    ]);

    let error = read_file(&args).expect_err("mixed range styles should fail");
    assert_eq!(
        error.message,
        "ranges cannot be combined with start_line or end_line"
    );
}

#[test]
fn format_read_range_reports_requested_ranges() {
    assert_eq!(format_read_range(None, None), "..");
    assert_eq!(format_read_range(Some(11), None), "11..");
    assert_eq!(format_read_range(None, Some(100)), "1..100");
    assert_eq!(format_read_range(Some(11), Some(11)), "11..11");
    assert_eq!(format_read_range(Some(11), Some(100)), "11..100");
}

#[test]
fn read_file_errors_when_start_line_is_past_eof() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(3.into()),
        ),
    ]);

    let error = read_file(&args).expect_err("start_line past EOF should fail");
    assert_eq!(
        error.message,
        "start_line 3 is past end of file (total_lines: 2)"
    );
}

#[test]
fn read_file_reports_empty_file_as_zero_lines() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(cbor_map_text(&result, "line-numbered content"), Some(""));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(0));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(0));
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_rejects_start_line_after_empty_file() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);

    let error = read_file(&args).expect_err("start_line after empty file should fail");
    assert_eq!(
        error.message,
        "start_line 2 is past end of file (total_lines: 0)"
    );
}

#[test]
fn read_file_reports_no_trailing_newline_as_one_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("no-newline.txt");
    std::fs::write(&path, "text").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(no_nl) text")
    );
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

/// Ensures truncated sliced reads retain source numbering in both visible and
/// complete saved rendering.
#[test]
fn read_file_truncation_notice_uses_source_line_numbers() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("big-slice.txt");
    let lines: Vec<String> = (1..=2105).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(100.into()),
        ),
    ]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content field");

    assert!(content.contains("100 line 100"));
    assert!(content.len() <= MAX_OUTPUT_BYTES);
    let saved_path =
        cbor_map_text(&result, "full_output_path").expect("complete saved output path");
    let saved = std::fs::read_to_string(saved_path).expect("saved sliced output");
    assert!(saved.contains("line 2105"));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(2105));
}

#[test]
fn read_file_reports_crlf_line_endings() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("crlf.txt");
    std::fs::write(&path, "one\r\ntwo\r\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(crlf) one\n2(crlf) two")
    );
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_reports_cr_only_line_endings() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("cr.txt");
    std::fs::write(&path, b"one\rtwo\r").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(cr) one\n2(cr) two")
    );
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_does_not_mark_lf_when_line_endings_are_evenly_mixed() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("mixed-even.txt");
    std::fs::write(&path, b"one\ntwo\r\nthree").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1 one\n2(crlf) two\n3(no_nl) three")
    );
}

#[test]
fn read_file_marks_line_ending_outliers() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("mixed.txt");
    std::fs::write(&path, b"one\ntwo\nthree\r\nfour\rfive").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1 one\n2 two\n3(crlf) three\n4(cr) four\n5(no_nl) five")
    );
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
}

#[test]
fn read_file_handles_invalid_utf8_per_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("invalid.bin");
    std::fs::write(&path, b"abc\xffdef\nsecond\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(invalid-utf8) abc�def\n2 second")
    );
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_bool_field(&result, "valid_utf8"), Some(false));
    assert!(cbor_map_field(&result, "total_bytes").is_none());
}

#[test]
fn read_file_truncates_single_long_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("longline.txt");
    std::fs::write(&path, format!("{}\nsecond\n", "x".repeat(60 * 1024))).expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content");

    assert_eq!(content, "1(truncated)\n2 second");
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_int_field(&result, "total_bytes").is_some());
}

#[test]
fn edit_context_line_rejects_invalid_utf8_bytes_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.bin");
    fs::write(&file_path, b"abc\xffdef\nsecond\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "FIRST\n", "abc�def")],
    ))
    .expect_err("invalid UTF-8 context_line should fail");

    assert_eq!(
        error.message,
        "context_line wrong - current line 1 is not valid UTF-8, so no context_line string can match it; see current content in the response"
    );
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"abc\xffdef\nsecond\n"
    );
}
#[test]
fn run_find_double_star_matches_top_level_files() {
    // Regression: `**/*.rs` should match both nested AND
    // top-level Rust files. `globset`'s native `**` requires one
    // path separator; we work around that in `compile_find_glob`.
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("top.rs"), "fn top() {}\n").expect("write top");
    fs::write(tempdir.path().join("src/lib.rs"), "fn nested() {}\n").expect("write nested");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write readme");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains("top.rs"),
        "top-level match missing: {output}"
    );
    assert!(
        output.contains("src/lib.rs"),
        "nested match missing: {output}"
    );
    assert!(!output.contains("README.md"));
}

#[test]
fn run_find_returns_matching_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "pattern").is_none());
    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));
}

#[test]
fn run_find_no_matches_uses_plain_ok_status() {
    // Regression: the UI already renders the zero-match count, so the
    // success chip should stay the generic `ok` instead of repeating
    // `no matches` in the status text.
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let output = run_find(&args).expect("find");

    assert_eq!(output.display.status_text, "ok");
    assert_eq!(output.display.stats.matches, Some(0));
    assert!(cbor_map_field(&output.result, "path").is_none());
    assert!(cbor_map_field(&output.result, "pattern").is_none());
    assert_eq!(cbor_int_field(&output.result, "matches"), Some(0));
}

#[test]
fn run_ls_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(tempdir.path().display().to_string()),
    )]);
    let mut world = path_crate_tools_world::ShellWorld::real();
    let result = run_ls(&args, &mut world).expect("ls").result;

    assert!(cbor_map_field(&result, "path").is_none());
    assert_eq!(cbor_int_field(&result, "entries"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));
}

/// Ensures the public shell tool path reads a real local image and keeps pixels
/// exclusively in typed provider content while returning safe text metadata.
#[test]
fn read_image_returns_typed_provider_content() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("fixture.png");
    let source = image::DynamicImage::new_rgb8(4, 3);
    let mut bytes = path_std_io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");

    let output = read_image(&cbor_text_map(vec![(
        "path",
        path.to_str().expect("temporary path is UTF-8"),
    )]))
    .expect("read image");
    assert_eq!(output.provider_content.len(), 1);
    let tau_proto::ToolResultContentPart::Image(image) = &output.provider_content[0];
    assert_eq!(image.media_type, tau_proto::ImageMediaType::Png);
    assert_eq!((image.width, image.height), (4, 3));
    assert_eq!(cbor_map_text(&output.result, "mode"), Some("high"));
    assert_eq!(cbor_map_int(&output.result, "patches"), Some(1));
    assert_eq!(
        cbor_map_int(&output.result, "bytes"),
        i64::try_from(image.data.len()).ok()
    );
    assert_eq!(output.display.stats.bytes, Some(image.data.len() as u64));
    assert_eq!(
        cbor_map_value(&output.result, "region"),
        Some(&CborValue::Map(vec![
            (
                CborValue::Text("x".to_owned()),
                CborValue::Integer(0.into())
            ),
            (
                CborValue::Text("y".to_owned()),
                CborValue::Integer(0.into())
            ),
            (
                CborValue::Text("width".to_owned()),
                CborValue::Integer(4.into())
            ),
            (
                CborValue::Text("height".to_owned()),
                CborValue::Integer(3.into())
            ),
        ]))
    );
    let decoded = image::load_from_memory_with_format(&image.data, image::ImageFormat::Png)
        .expect("prepared image remains a decodable PNG");
    assert_eq!((decoded.width(), decoded.height()), (4, 3));
    assert!(matches!(output.result, CborValue::Map(_)));
    assert!(!format!("{:?}", output.result).contains("137, 80, 78, 71"));
}

/// Locks bare-call compatibility against explicit high at a resizing boundary,
/// including canonical bytes, dimensions, and patch accounting.
#[test]
fn read_image_bare_call_matches_explicit_high_for_large_image() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("large.png");
    let source = image::DynamicImage::new_rgb8(1601, 1601);
    let mut bytes = path_std_io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");
    let path = path.display().to_string();
    let bare = read_image(&cbor_text_map(vec![("path", &path)])).expect("bare high");
    let explicit = read_image(&CborValue::Map(vec![
        (CborValue::Text("path".to_owned()), CborValue::Text(path)),
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text("high".to_owned()),
        ),
    ]))
    .expect("explicit high");
    assert_eq!(
        (provider_image(&bare).width, provider_image(&bare).height),
        (1569, 1569)
    );
    assert_eq!(cbor_map_int(&bare.result, "patches"), Some(2500));
    assert_eq!(provider_image(&bare).data, provider_image(&explicit).data);
    assert_eq!(bare.result, explicit.result);
}

/// Ensures the public tool reports exact transform metadata and directs only
/// the cropped canonical raster into provider content.
#[test]
fn read_image_overview_crop_reports_transform_metadata() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("fixture.png");
    let source = image::DynamicImage::new_rgb8(100, 80);
    let mut bytes = path_std_io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");
    let region = CborValue::Map(vec![
        (
            CborValue::Text("x".to_owned()),
            CborValue::Integer(10.into()),
        ),
        (
            CborValue::Text("y".to_owned()),
            CborValue::Integer(20.into()),
        ),
        (
            CborValue::Text("width".to_owned()),
            CborValue::Integer(64.into()),
        ),
        (
            CborValue::Text("height".to_owned()),
            CborValue::Integer(32.into()),
        ),
    ]);
    let output = read_image(&CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text("overview".to_owned()),
        ),
        (CborValue::Text("region".to_owned()), region.clone()),
    ]))
    .expect("read overview crop");

    assert_eq!(cbor_map_text(&output.result, "mode"), Some("overview"));
    assert_eq!(cbor_map_int(&output.result, "source_width"), Some(100));
    assert_eq!(cbor_map_int(&output.result, "source_height"), Some(80));
    assert_eq!(cbor_map_int(&output.result, "oriented_width"), Some(100));
    assert_eq!(cbor_map_int(&output.result, "oriented_height"), Some(80));
    assert_eq!(cbor_map_int(&output.result, "width"), Some(64));
    assert_eq!(cbor_map_int(&output.result, "height"), Some(32));
    assert_eq!(cbor_map_int(&output.result, "patches"), Some(2));
    let result_region = cbor_map_value(&output.result, "region");
    assert_eq!(result_region, Some(&region));
    let tau_proto::ToolResultContentPart::Image(image) = &output.provider_content[0];
    assert_eq!((image.width, image.height), (64, 32));
    assert_eq!(image.detail, tau_proto::ImageDetail::High);
    assert_eq!(
        cbor_map_int(&output.result, "bytes"),
        i64::try_from(image.data.len()).ok()
    );
    assert_eq!(output.display.stats.bytes, Some(image.data.len() as u64));
}

/// Locks the model-visible experimental mode and complete oriented-region
/// schema.
#[test]
fn read_image_schema_exposes_bounded_overview_and_complete_region() {
    let tool = registered_tool_specs(false)
        .into_iter()
        .find(|tool| tool.name == READ_IMAGE_TOOL_NAME)
        .expect("read_image tool");
    let parameters = tool.parameters.expect("parameters");
    assert_eq!(
        parameters["properties"]["mode"]["enum"],
        serde_json::json!(["high", "overview"])
    );
    assert_eq!(
        parameters["properties"]["region"]["required"],
        serde_json::json!(["x", "y", "width", "height"])
    );
    assert_eq!(
        parameters["properties"]["region"]["additionalProperties"],
        serde_json::json!(false)
    );
}

/// Locks the internal `replace` implementation to its visible `edit` alias and
/// strict snapshot-replacement schema so model repair cannot widen the surface.
#[test]
fn replace_schema_is_strict_and_default_disabled() {
    let tool = registered_tool_specs(false)
        .into_iter()
        .find(|tool| tool.name == REPLACE_TOOL_NAME)
        .expect("replace tool");
    assert!(!tool.enabled_by_default);
    assert_eq!(tool.model_visible_name.as_deref(), Some(EDIT_TOOL_NAME));
    assert_eq!(
        tool.parameters.expect("parameters"),
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "minLength": 1 },
                            "newText": { "type": "string" }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    );
}

#[test]
fn edit_result_reports_minimal_status_without_model_diff() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("output.txt");
    fs::write(&file_path, "alpha beta gamma\nsame\n").expect("write fixture");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(
            1,
            1,
            "alpha BETA gamma\n",
            "alpha beta gamma",
        )],
    ))
    .expect("edit");

    assert!(cbor_map_field(&output.result, "path").is_none());
    assert_eq!(cbor_int_field(&output.result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&output.result, "changed"), Some(true));
    assert_eq!(
        cbor_int_field(&output.result, "new_max_valid_start_line"),
        Some(3)
    );
    assert!(cbor_map_field(&output.result, "available_lines").is_none());
    assert!(cbor_map_field(&output.result, "max_valid_start_line").is_none());
    assert_eq!(cbor_int_field(&output.result, "total_bytes"), Some(22));
    assert!(cbor_map_text(&output.result, "output").is_none());
    assert!(cbor_map_text(&output.result, "diff").is_none());
    assert!(matches!(
        output.display.payload,
        Some(ToolUsePayload::Diff(_))
    ));
}

#[test]
fn edit_self_replacement_counts_without_diff() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "same\n").expect("write fixture");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "same\n", "same")],
    ))
    .expect("edit");

    assert!(cbor_map_field(&output.result, "path").is_none());
    assert_eq!(cbor_int_field(&output.result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&output.result, "changed"), Some(false));
    assert_eq!(
        cbor_int_field(&output.result, "new_max_valid_start_line"),
        Some(2)
    );
    assert_eq!(cbor_int_field(&output.result, "total_bytes"), Some(5));
    assert!(cbor_map_text(&output.result, "output").is_none());
    assert!(output.display.payload.is_none());
}

#[test]
fn edit_new_file_reports_created_as_changed() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("new.txt");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "created\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(2));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(8));
    assert!(cbor_map_text(&result, "output").is_none());
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "created\n"
    );
}

#[test]
fn edit_existing_symlink_updates_target() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("target.txt");
    let link_path = tempdir.path().join("link.txt");
    fs::write(&target_path, "old\n").expect("write fixture");
    symlink("target.txt", &link_path).expect("symlink");

    let result = edit_file(&edit_arguments(
        &link_path,
        vec![context_line_edit(1, 1, "new\n", "old")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(
        fs::read_to_string(&target_path).expect("read target"),
        "new\n"
    );
}

#[test]
fn edit_dangling_symlink_creates_target() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("target.txt");
    let link_path = tempdir.path().join("link.txt");
    symlink("target.txt", &link_path).expect("symlink");

    let result = edit_file(&edit_arguments(
        &link_path,
        vec![context_half_open_edit(1, 1, "", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(0));
    assert_eq!(fs::read_to_string(&target_path).expect("read target"), "");
}

#[test]
fn edit_context_line_rejects_invalid_utf8_original_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("invalid.bin");
    fs::write(&file_path, b"abc\xffdef\nsecond\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "FIRST\n", "abc�def")],
    ))
    .expect_err("invalid UTF-8 context_line should fail");

    assert_eq!(
        error.message,
        "context_line wrong - current line 1 is not valid UTF-8, so no context_line string can match it; see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some("1(invalid-utf8) abc�def\n2 second")
    );
    assert_eq!(cbor_bool_field(details, "valid_utf8"), Some(false));
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"abc\xffdef\nsecond\n"
    );
}
#[test]
fn edit_rejects_edit_request_over_cap() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    let edits = (0..=100)
        .map(|_| context_half_open_edit(1, 1, "x", ""))
        .collect::<Vec<_>>();

    let error = edit_file(&edit_arguments(&file_path, edits))
        .expect_err("edit should reject over-cap request");

    assert_eq!(error.message, "requested edit count exceeds limit of 100");
    assert!(!file_path.exists());
}

#[test]
fn edit_rejects_edit_request_over_cap_before_reading_file() {
    let edits = (0..=100)
        .map(|_| context_half_open_edit(1, 1, "x", ""))
        .collect::<Vec<_>>();
    let args = cbor_map(vec![
        (
            "path",
            CborValue::Text("/definitely/missing/edit-target.txt".to_owned()),
        ),
        ("edits", CborValue::Array(edits)),
    ]);

    let error = edit_file(&args).expect_err("edit should reject arguments first");

    assert_eq!(error.message, "requested edit count exceeds limit of 100");
}

#[test]
fn edit_rejects_overlapping_ranges_without_partial_write() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "aa\nbb\ncc\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_line_edit(1, 2, "x\n", "aa"),
            context_line_edit(2, 2, "y\n", "aa"),
        ],
    ))
    .expect_err("overlap should fail");

    assert_eq!(error.message, "overlapping edits");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "aa\nbb\ncc\n"
    );
}

#[test]
fn edit_rejects_missing_new_text() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![cbor_map(vec![
                    ("start_line", CborValue::Integer(1.into())),
                    ("end_line_exclusive", CborValue::Integer(2.into())),
                ])],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(error.message, "each edit must have a string newText");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_negative_start_line_with_path_args() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![line_edit(-1, 1, "x")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(error.message, "start_line must be at least 1");
    assert!(
        error.details.is_none(),
        "edit errors should not echo arguments"
    );
    assert_eq!(
        error.display.expect("display").args,
        file_path.display().to_string()
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_zero_end_line_exclusive() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(0.into())),
            ("newText", CborValue::Text("x".to_owned())),
            ("context_line", CborValue::Text("hello".to_owned())),
        ])],
    ))
    .expect_err("end_line_exclusive=0 should fail");

    assert_eq!(error.message, "end_line_exclusive must be at least 1");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_end_line_exclusive_before_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(3, 1, "x", "world")],
    ))
    .expect_err("end_line_exclusive before start_line should fail");

    assert_eq!(
        error.message,
        "end_line_exclusive must be at least start_line"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\nworld\n"
    );
}
#[test]
fn mark_line_merges_existing_markers_when_truncating() {
    // Truncating an already marked rendered line should preserve the single
    // marker group grammar used by read/shell output.
    assert_eq!(
        mark_line("1(no_nl) hello", "truncated"),
        "1(no_nl,truncated)"
    );
    assert_eq!(mark_line("2(crlf) hello", "truncated"), "2(crlf,truncated)");
    assert_eq!(
        mark_line("out(no_nl) hello", "truncated"),
        "out(no_nl,truncated)"
    );
}
#[test]
fn classify_ripgrep_stderr_recognizes_stable_prefixes() {
    // Bad regex from the agent. The trailing `error: <diagnostic>`
    // line is the useful one — the header and caret lines aren't.
    let parsed = classify_ripgrep_stderr(
        "regex parse error:\n    (?:Result<(.*Address.*TweakIdx)\n    ^\nerror: unclosed group",
    );
    assert!(
        matches!(parsed, RipgrepError::Usage { .. }),
        "got: {parsed:?}"
    );
    assert_eq!(parsed.to_string(), "regex parse error: unclosed group");
    // Missing path / file.
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)"),
        RipgrepError::NotFound,
    );
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)").to_string(),
        "no such file or directory",
    );
    // Permission denied.
    assert_eq!(
        classify_ripgrep_stderr("Permission denied (os error 13)"),
        RipgrepError::Permission,
    );
    // Anything else (genuine runtime fault) keeps the first stderr
    // line so the chip still carries a useful signal.
    assert_eq!(
        classify_ripgrep_stderr("some unfamiliar ripgrep failure").to_string(),
        "ripgrep error: some unfamiliar ripgrep failure",
    );
}

/// Ensures a large read stays visibly bounded while exposing the exact complete
/// line-numbered rendering through its private saved path.
#[test]
fn read_file_truncates_large_output() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("big.txt");
    let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content field");
    assert!(content.contains("line 1\n"));
    assert!(content.len() <= MAX_OUTPUT_BYTES);
    let saved_path =
        cbor_map_text(&result, "full_output_path").expect("complete saved output path");
    let saved = std::fs::read_to_string(saved_path).expect("saved read output");
    assert!(saved.contains("line 3000"));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(3000));
}
