//! Tests for extension lifecycle behavior.

use super::*;

#[test]
fn extension_finds_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(FIND_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("pattern".to_owned()),
                    CborValue::Text("**/*.rs".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(tempdir.path().display().to_string()),
                ),
            ]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, FIND_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "matches"), Some(2));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn startup_declares_exact_shell_subscriptions_and_ready_after_publications() {
    let (mut reader, mut writer) = spawn_extension();

    let hello = reader
        .read_raw_message()
        .expect("read hello")
        .expect("hello frame");
    assert!(matches!(hello, HarnessInputMessage::Hello(_)));

    let subscribe = reader
        .read_raw_message()
        .expect("read subscribe")
        .expect("subscribe frame");
    let HarnessInputMessage::Subscribe(subscribe) = subscribe else {
        panic!("expected Subscribe after Hello");
    };
    assert_eq!(
        subscribe.live_selectors,
        [
            EventSelector::Exact(EventName::TOOL_STARTED),
            EventSelector::Exact(EventName::TOOL_CANCEL_REQUEST),
            EventSelector::Exact(EventName::ACTION_INVOKE),
            EventSelector::Exact(EventName::SESSION_STARTED),
            EventSelector::Exact(EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(EventName::SESSION_AGENT_UNLOADED),
            EventSelector::Exact(EventName::AGENT_REPLAY_COMPLETE),
            EventSelector::Exact(EventName::AGENT_METADATA_SET),
            EventSelector::Exact(EventName::AGENT_METADATA_UNSET),
            EventSelector::Exact(EventName::SESSION_SHUTDOWN),
            EventSelector::Exact(EventName::AGENT_START_ACCEPTED),
            EventSelector::Exact(EventName::AGENT_START_RESULT),
            EventSelector::Exact(EventName::UI_SHELL_COMMAND),
        ]
    );
    assert_eq!(
        subscribe.historical_selectors,
        [
            EventSelector::Exact(EventName::SESSION_STARTED),
            EventSelector::Exact(EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(EventName::SESSION_AGENT_UNLOADED),
            EventSelector::Exact(EventName::AGENT_METADATA_SET),
            EventSelector::Exact(EventName::AGENT_METADATA_UNSET),
        ]
    );

    for expected_tool in [
        ECHO_TOOL_NAME,
        READ_TOOL_NAME,
        READ_IMAGE_TOOL_NAME,
        EDIT_TOOL_NAME,
        REPLACE_TOOL_NAME,
        APPLY_PATCH_TOOL_NAME,
        DIR_LOCK_TOOL_NAME,
        GREP_TOOL_NAME,
        FIND_TOOL_NAME,
        LS_TOOL_NAME,
        WORKDIR_TOOL_NAME,
        SHELL_TOOL_NAME,
        GPT_SHELL_TOOL_NAME,
    ] {
        let message = reader
            .read_raw_message()
            .expect("read startup tool")
            .expect("startup tool frame");
        let HarnessInputMessage::Emit(emit) = message else {
            panic!("expected tool registration before Ready, got {message:?}");
        };
        let Event::ToolRegistrationDeclared(register) = *emit.event else {
            panic!(
                "expected tool registration before Ready, got {:?}",
                emit.event
            );
        };
        assert_eq!(register.tool.name, expected_tool);
    }

    for expected in [
        EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER,
        EventName::EXTENSION_SESSION_CONTEXT_PROVIDER_REGISTER,
        EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
        EventName::ACTION_SCHEMA_DECLARED,
    ] {
        let message = reader
            .read_raw_message()
            .expect("read startup publication")
            .expect("startup publication frame");
        let HarnessInputMessage::Emit(emit) = message else {
            panic!("expected startup Emit before Ready, got {message:?}");
        };
        assert_eq!(emit.event.name(), expected);
    }

    let ready = reader
        .read_raw_message()
        .expect("read ready")
        .expect("ready frame");
    assert!(
        matches!(ready, HarnessInputMessage::Ready(_)),
        "Ready must follow all startup publications, got {ready:?}"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures the deterministic skill, AGENTS.md, and readiness batch uses
/// `persist=false` wire metadata for every session-discovery event.
#[test]
fn extension_reads_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("README.txt");
    fs::write(&file_path, "hello from file").expect("write fixture");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(file_path.display().to_string()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, READ_TOOL_NAME);
    assert_eq!(
        optional_argument_text(&result.result, "line-numbered content"),
        Ok(Some("1(no_nl) hello from file".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_read_missing_file_reports_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("/definitely/missing/file.txt".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(!error.message.contains("failed to read"));
    assert!(error.message.contains("No such file or directory"));
    assert!(
        error.details.is_none(),
        "read errors should not echo arguments"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures an advertised replace invocation passes runtime admission and emits
/// one terminal result instead of being silently ignored by the dispatcher.
#[test]
fn extension_replace_dispatches_to_a_terminal_result() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("source.txt");
    fs::write(&path, "before\n").expect("write source");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&tool_started(
            "replace-terminal",
            REPLACE_TOOL_NAME,
            replace_arguments(&path, "before", "after"),
            "agent-replace",
        ))
        .expect("replace");
    writer.flush().expect("flush replace");

    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::ToolResult(result) if result.call_id.as_str() == "replace-terminal" => {
                assert_eq!(result.tool_name, REPLACE_TOOL_NAME);
                break;
            }
            Event::ToolError(error) if error.call_id.as_str() == "replace-terminal" => {
                panic!("replace unexpectedly failed: {error:?}");
            }
            _ => {}
        }
    }
    assert_eq!(fs::read_to_string(&path).expect("read result"), "after\n");
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_creates_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("output.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![context_half_open_edit(1, 1, "written content", "")],
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
    assert_eq!(result.tool_name, EDIT_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "written content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_rejects_oversized_existing_file_before_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("large.txt");
    let file = fs::File::create(&file_path).expect("create large file");
    file.set_len(TEST_SAFE_FILE_READ_LIMIT + 1)
        .expect("make sparse large file");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-oversized-edit".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(1, 1, "x", "")]),
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
    assert!(error.message.contains("file is too large to read safely"));
    assert_eq!(
        fs::metadata(&file_path).expect("metadata").len(),
        TEST_SAFE_FILE_READ_LIMIT + 1
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_missing_parent_reports_short_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_parent = tempdir.path().join("missing-parent");
    let file_path = missing_parent.join("child.txt");
    fs::write(&missing_parent, "not a dir").expect("write blocker");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(1, 1, "x", "")]),
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
    assert!(!error.message.contains("failed to create directories"));
    assert!(!error.message.contains(file_path.to_string_lossy().as_ref()));
    assert!(error.message.contains("Not a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_directory_reports_short_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                Path::new("/tmp"),
                vec![context_half_open_edit(1, 1, "x", "")],
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
    assert!(!error.message.contains("failed to write"));
    assert!(error.message.contains("Is a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_creates_directories() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("a/b/c/deep.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![context_half_open_edit(1, 1, "deep content", "")],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "deep content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures a completed one-file freeform patch exposes the changed file only
/// through UI-only structured metadata, allowing the terminal to label hunks
/// without duplicating the compact model-visible result.
#[test]
fn extension_apply_patch_updates_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-1".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "after\n"
    );
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nM {}",
            file_path.display()
        ))
    );
    let display = result.display.expect("apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("single-file apply_patch must retain its structured display path");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, file_path.display().to_string());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_reports_context_mismatch_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-missing\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-2".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to find expected lines"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch semantically escapes paths in model-visible summaries
/// and UI-only diffs so malicious filenames cannot inject fake records or
/// headers.
#[test]
fn extension_apply_patch_escapes_control_characters_in_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("line\tbreak.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-success".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    let CborValue::Text(output) = result.result else {
        panic!("expected text result");
    };
    assert!(
        output.contains("line\\tbreak.txt"),
        "escaped path missing: {output}"
    );
    assert!(
        !output.contains("line\tbreak.txt"),
        "path tab should be escaped in output: {output}"
    );
    let display = result.display.expect("apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("apply_patch display must preserve escaped path metadata");
    };
    assert_eq!(files.len(), 1);
    assert!(files[0].path.contains("line\\tbreak.txt"));
    assert!(!files[0].path.contains("line\tbreak.txt"));

    let created_path = tempdir.path().join("created\tfile.txt");
    let missing_path = tempdir.path().join("missing\tfile.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        created_path.display(),
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-partial".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("missing\\tfile.txt"),
        "escaped error path missing: {}",
        error.message
    );
    let display = error.display.expect("partial apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("partial apply_patch display must preserve the changed file path");
    };
    assert_eq!(files.len(), 1);
    assert!(files[0].path.contains("created\\tfile.txt"));
    assert!(!files[0].path.contains("created\tfile.txt"));
    let details = error.details.expect("partial changes details");
    let CborValue::Map(entries) = details else {
        panic!("expected structured partial change details");
    };
    let partial_changes = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Array(changes)) if key == "partial_changes" => {
                Some(changes)
            }
            _ => None,
        })
        .expect("partial_changes detail");
    let CborValue::Map(change) = &partial_changes[0] else {
        panic!("expected partial change map");
    };
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value))
            if key == "path" && value.contains("created\\tfile.txt")
    )));

    let dir_path = tempdir.path().join("dir\tname");
    fs::create_dir(&dir_path).expect("create tab dir");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        dir_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-io-error".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("dir\\tname"),
        "escaped diagnostic path missing: {}",
        error.message
    );
    assert!(
        !error.message.contains("dir\tname"),
        "embedded I/O diagnostic should not keep raw tabs: {}",
        error.message
    );

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-invalid-op".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(
                "*** Begin Patch\n*** Bad\tOperation\n*** End Patch".to_owned(),
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
    assert!(
        error.message.contains("Bad\\tOperation"),
        "escaped invalid operation missing: {}",
        error.message
    );
    assert!(
        !error.message.contains("Bad\tOperation"),
        "invalid operation should not keep raw tabs: {}",
        error.message
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures a successful move labels its destination in UI-only diff metadata,
/// so the terminal describes the file that now owns the changed contents.
#[test]
fn extension_apply_patch_move_renames_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let src = tempdir.path().join("old.txt");
    let dst = tempdir.path().join("new.txt");
    fs::write(&src, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
        src.display(),
        dst.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-3".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    let display = result.display.expect("apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("moved apply_patch file must retain its destination path");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, dst.display().to_string());
    assert!(!src.exists(), "source path should be removed after move");
    assert_eq!(fs::read_to_string(&dst).expect("read back"), "after\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch moves do not silently clobber an existing destination.
/// Move patches must fail before mutating either path so accidental data loss
/// is reported as a tool error instead of hidden in UI diff metadata.
#[test]
fn extension_apply_patch_move_rejects_existing_destination() {
    let tempdir = TempDir::new().expect("tempdir");
    let src = tempdir.path().join("old.txt");
    let dst = tempdir.path().join("new.txt");
    fs::write(&src, "before\n").expect("write src");
    fs::write(&dst, "existing\n").expect("write dst");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
        src.display(),
        dst.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-move-existing".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Move destination already exists"));
    assert_eq!(fs::read_to_string(&src).expect("read src"), "before\n");
    assert_eq!(fs::read_to_string(&dst).expect("read dst"), "existing\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_operations() {
    let tempdir = TempDir::new().expect("tempdir");
    let add_path = tempdir.path().join("nested/new.txt");
    let modify_path = tempdir.path().join("modify.txt");
    let delete_path = tempdir.path().join("delete.txt");
    fs::write(&modify_path, "line1\nline2\n").expect("write modify");
    fs::write(&delete_path, "obsolete\n").expect("write delete");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+created\n*** Delete File: {}\n*** Update File: {}\n@@\n-line2\n+changed\n*** End Patch",
        add_path.display(),
        delete_path.display(),
        modify_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-4".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&add_path).expect("read added"),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(&modify_path).expect("read modified"),
        "line1\nchanged\n"
    );
    assert!(!delete_path.exists(), "deleted path should be removed");
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nA {}\nM {}\nD {}",
            add_path.display(),
            modify_path.display(),
            delete_path.display(),
        ))
    );
    let display = result.display.expect("apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("expected multi-file structured diff payload");
    };
    assert_eq!(files.len(), 3);
    assert!(
        files
            .iter()
            .any(|file| file.path == add_path.display().to_string())
    );
    assert!(
        files
            .iter()
            .any(|file| file.path == delete_path.display().to_string())
    );
    let modify_diff = files
        .iter()
        .find(|file| file.path == modify_path.display().to_string())
        .expect("modify diff");
    assert!(
        modify_diff
            .diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| matches!(line, tau_proto::DiffLine::Modify { .. }))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_chunks() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("multi.txt");
    fs::write(&target_path, "line1\nline2\nline3\nline4\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-line2\n+changed2\n@@\n-line4\n+changed4\n*** End Patch",
        target_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-5".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&target_path).expect("read back"),
        "line1\nchanged2\nline3\nchanged4\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures a later patch failure retains an already-added file as a one-entry,
/// path-labelled `Diffs` payload while reporting the later failed operation.
#[test]
fn extension_apply_patch_failure_after_partial_success_leaves_changes() {
    let tempdir = TempDir::new().expect("tempdir");
    let created_path = tempdir.path().join("created.txt");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        created_path.display(),
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-5b".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    let details = error.details.expect("partial changes details");
    let CborValue::Map(entries) = details else {
        panic!("expected structured partial change details");
    };
    let partial_changes = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Array(changes)) if key == "partial_changes" => {
                Some(changes)
            }
            _ => None,
        })
        .expect("partial_changes detail");
    assert_eq!(partial_changes.len(), 1);
    let CborValue::Map(change) = &partial_changes[0] else {
        panic!("expected partial change map");
    };
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value)) if key == "status" && value == "A"
    )));
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value))
            if key == "path" && value == &created_path.display().to_string()
    )));
    let display = error.display.expect("error display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("expected structured diff payload for partial apply_patch failure");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, created_path.display().to_string());
    let diff = &files[0].diff;
    assert_eq!(diff.added, 1);
    assert_eq!(diff.removed, 0);
    assert!(
        diff.hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .any(|line| matches!(line, tau_proto::DiffLine::Add { text } if text == "hello")),
        "partial add should be visible in structured diff"
    );
    assert_eq!(
        fs::read_to_string(&created_path).expect("created file should remain"),
        "hello\n"
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_rejects_oversized_update_before_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("large.txt");
    let file = fs::File::create(&file_path).expect("create large file");
    file.set_len(TEST_SAFE_FILE_READ_LIMIT + 1)
        .expect("make sparse large file");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        file_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-oversized-patch".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("file is too large to read safely"));
    assert_eq!(
        fs::metadata(&file_path).expect("metadata").len(),
        TEST_SAFE_FILE_READ_LIMIT + 1
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_requires_existing_file_for_update() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-6".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert!(
        error
            .message
            .contains(missing_path.to_string_lossy().as_ref())
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch Add File rejects existing files instead of silently
/// overwriting content that required an explicit Update File hunk.
#[test]
fn extension_apply_patch_add_rejects_existing_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("duplicate.txt");
    fs::write(&path, "old content\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+new content\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-7".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("Add File target already exists"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        "old content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_update_appends_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("no_newline.txt");
    fs::write(&path, "no newline at end").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-8".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    let contents = fs::read_to_string(&path).expect("read back");
    assert!(contents.ends_with('\n'));
    assert_eq!(contents, "first line\nsecond line\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(LS_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(tempdir.path().display().to_string()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, LS_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "entries"), Some(3));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn startup_registers_schema_valid_tool_examples() {
    // Provider-owned examples are shown after failed calls, so every startup
    // registration must keep them aligned with that tool's current schema.
    let (mut reader, mut writer) = spawn_extension();

    let mut checked = Vec::new();
    for _ in 0..13 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        tau_core::validate_tool_examples(&register.tool)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", register.tool.name));
        checked.push(register.tool.name.to_string());
    }
    for tool_name in [
        READ_TOOL_NAME,
        EDIT_TOOL_NAME,
        APPLY_PATCH_TOOL_NAME,
        DIR_LOCK_TOOL_NAME,
        GREP_TOOL_NAME,
        FIND_TOOL_NAME,
        LS_TOOL_NAME,
        WORKDIR_TOOL_NAME,
        SHELL_TOOL_NAME,
        GPT_SHELL_TOOL_NAME,
    ] {
        assert!(
            checked.iter().any(|name| name == tool_name),
            "expected to validate examples for {tool_name}"
        );
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A mandatory discovery write failure on the production client adapter must
/// escape the manual loop and terminate the extension connection.
#[test]
fn mandatory_discovery_write_failure_exits_production_manual_loop() {
    let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let runtime_reader = runtime_stream.try_clone().expect("runtime reader");
    let (done_tx, done_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let result = run_impl(
            runtime_reader,
            runtime_stream,
            DiscoverySourcePolicy::EmptyFixture,
            RuntimeCwdSource::Fixture(PathBuf::from("/tmp")),
        )
        .map_err(|error| error.to_string());
        let _ = done_tx.send(result);
    });
    let mut input = EventWriter::new(
        harness_stream
            .try_clone()
            .expect("harness input writer clone"),
    );
    input
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("extension name"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    input.flush().expect("flush configure");
    let mut output = HarnessInputReader::new(
        harness_stream
            .try_clone()
            .expect("harness output reader clone"),
    );
    while !matches!(
        output.read_message().expect("startup output"),
        Some(HarnessInputMessage::Ready(_))
    ) {}

    harness_stream
        .shutdown(Shutdown::Read)
        .expect("close harness output direction");
    input
        .write_event(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: tau_proto::SessionId::parse("session-write-failure").expect("session id"),
            reason: tau_proto::SessionStartReason::New,
        }))
        .expect("session start");
    input.flush().expect("flush session start");

    let result = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("extension must exit after mandatory write failure");
    assert!(result.is_err());
}

/// A production model-tool terminal flush failure must escape the manual loop
/// instead of leaving its routed call owned by a connected extension.
#[test]
fn mandatory_model_tool_terminal_failure_exits_production_manual_loop() {
    let event = tool_started(
        "call-mandatory-terminal-failure",
        ECHO_TOOL_NAME,
        cbor_text_map(vec![("text", "settle me")]),
        "main",
    );
    assert_mandatory_frame_failure_exits(event, b"tool.result_reported", "model tool terminal");
}

/// Ensures configured command enforcement replaces the startup fragment with
/// the effective selector pairs that the harness projects into agent prompts.
#[test]
fn configured_allowlist_publishes_effective_prompt_fragment() {
    let workdir = TempDir::new().expect("workdir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_shell_regex_allowlist_config(
        &mut writer,
        vec![(
            workdir.path().to_str().expect("UTF-8 workdir"),
            r"printf allowed",
        )],
    );

    let Event::ExtPromptFragmentPublish(publish) = reader
        .read_event()
        .expect("read configured prompt fragment")
        .expect("configured prompt fragment")
    else {
        panic!("expected configured prompt fragment publication");
    };
    assert_eq!(publish.fragment.name, "shell.workdir");
    let template = publish.fragment.template.as_str();
    assert!(template.contains("### Shell command allowlist"));
    assert!(template.contains("canonical effective workdir must both match one selector pair"));
    assert!(template.contains(&format!(
        r#"command_regex: "printf allowed"; workdir: "{}""#,
        workdir.path().display()
    )));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_applies_configured_prefix_and_command() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![
                    (
                        CborValue::Text("prefix".to_owned()),
                        CborValue::Array(vec![
                            CborValue::Text("env".to_owned()),
                            CborValue::Text("TAU_SHELL_PREFIX_TEST=ok".to_owned()),
                        ]),
                    ),
                    (
                        CborValue::Text("command".to_owned()),
                        CborValue::Text("sh".to_owned()),
                    ),
                ]),
            )]),
            state_dir: None,
            secrets: path_std_collections::BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf %s \"$TAU_SHELL_PREFIX_TEST\"".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let _progress = reader.read_event().expect("read").expect("progress");
    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) ok".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_extension_rejects_invalid_config() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("prefix".to_owned()),
                    CborValue::Text("nope".to_owned()),
                )]),
            )]),
            state_dir: None,
            secrets: path_std_collections::BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let HarnessInputMessage::ConfigError(error) = message {
            break error;
        }
    };
    assert!(error.message.contains("invalid type"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_working_directory_cannot_change_after_startup() {
    let current = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/one")),
        ..Default::default()
    };
    let same = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/one")),
        ..Default::default()
    };
    let changed = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/two")),
        ..Default::default()
    };

    apply_working_directory(&current, &same, false).expect("same cwd is idempotent");
    let err = apply_working_directory(&current, &changed, false).expect_err("cwd change rejected");
    assert!(err.contains("cannot be changed after startup"));
}

#[test]
fn shell_extension_reports_invalid_working_directory_config() {
    // `working_directory` is applied by ext-shell itself after Configure. A bad
    // path should surface as ConfigError instead of silently leaving relative
    // filesystem tools rooted at an unexpected directory.
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let td = TempDir::new().expect("tempdir");
    let missing_dir = td.path().join("missing");

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: cbor_text_map(vec![(
                "working_directory",
                missing_dir.to_str().expect("utf8 temp path"),
            )]),
            state_dir: None,
            secrets: path_std_collections::BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let HarnessInputMessage::ConfigError(error) = message {
            break error;
        }
    };
    assert!(
        error
            .message
            .contains("failed to set ext-shell working_directory")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Covers startup registration defaults, groups, names, and core tool schemas
/// through the real extension protocol publication path.
#[test]
fn startup_registers_echo_disabled_by_default_and_gpt_shell_visible_name() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_echo_disabled = false;
    let mut found_gpt_shell_visible_name = false;
    let mut found_read_schema = false;
    let mut found_read_image_foreground_only = false;
    let mut found_edit_schema = false;
    let mut found_write = false;
    for _ in 0..13 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == ECHO_TOOL_NAME {
            assert!(!register.tool.enabled_by_default);
            assert_eq!(
                register
                    .tool_group
                    .as_ref()
                    .map(|group| group.name.as_str()),
                Some("test")
            );
            found_echo_disabled = true;
        }
        if register.tool.name == GPT_SHELL_TOOL_NAME {
            assert_eq!(
                register.tool.model_visible_name,
                Some(tau_proto::ToolName::new("shell_command"))
            );
            found_gpt_shell_visible_name = true;
        }
        if register.tool.name == READ_TOOL_NAME {
            assert_eq!(
                register
                    .tool_group
                    .as_ref()
                    .map(|group| group.name.as_str()),
                Some("shell")
            );
            let tags = register
                .tool
                .tags
                .iter()
                .map(|tag| tag.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                tags,
                vec!["shell:read", tau_proto::TURN_DATA_FETCH_TOOL_TAG]
            );
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let range_item = &parameters["properties"]["ranges"]["items"];
            assert_eq!(
                range_item["required"],
                serde_json::json!(["start_line", "end_line"])
            );
            found_read_schema = true;
        }
        if register.tool.name == READ_IMAGE_TOOL_NAME {
            assert_eq!(
                register.tool.description.as_deref(),
                Some("Read one local image for visual inspection.")
            );
            assert_eq!(
                register.tool.background_support,
                Some(tau_proto::BackgroundSupport::Never)
            );
            found_read_image_foreground_only = true;
        }
        if register.tool.name == EDIT_TOOL_NAME {
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let edit_item = &parameters["properties"]["edits"]["items"];
            assert_eq!(
                edit_item["required"],
                serde_json::json!([
                    "start_line",
                    "end_line_exclusive",
                    "newText",
                    "context_line"
                ])
            );
            assert_eq!(
                edit_item["properties"]["after_line"],
                serde_json::Value::Null
            );
            assert_eq!(
                edit_item["properties"]["before_line"],
                serde_json::Value::Null
            );
            assert_eq!(edit_item["properties"]["end_line"], serde_json::Value::Null);
            assert_eq!(edit_item["properties"]["oldText"], serde_json::Value::Null);
            assert_eq!(
                edit_item["properties"]["context_line"]["type"],
                serde_json::json!("string")
            );
            found_edit_schema = true;
        }
        if register.tool.name == "write" {
            found_write = true;
        }
    }
    assert!(found_echo_disabled, "expected echo tool registration");
    assert!(
        found_gpt_shell_visible_name,
        "expected gpt_shell tool registration"
    );
    assert!(found_read_schema, "expected multi-range read schema");
    assert!(
        found_read_image_foreground_only,
        "expected foreground-only read_image registration"
    );
    assert!(found_edit_schema, "expected line-oriented edit schema");
    assert!(!found_write, "write tool should not be registered");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A user-shell completion flush failure must exit the extension loop so the
/// harness can release its private route and command-id reservation.
#[test]
fn mandatory_user_shell_completion_failure_exits_production_manual_loop() {
    assert_mandatory_frame_failure_exits(
        ui_shell_command("ui-mandatory-completion-failure", "printf done"),
        b"shell.command_finished_reported",
        "user shell completion",
    );
}

/// Observing the first worker output error must not clear the sticky marker
/// that keeps call ownership retained until disconnect cleanup.
#[test]
fn mandatory_output_failure_remains_sticky_after_loop_observation() {
    let (tx, rx) = path_std_sync::mpsc::channel();
    drop(rx);
    let output = Output::channel(tx);
    output
        .send_checked(HarnessInputMessage::emit_transient(Event::TermBell(
            tau_proto::TermBell {},
        )))
        .expect_err("closed output must fail");
    output
        .take_mandatory_failure()
        .expect_err("manual loop observes first failure");
    assert!(output.mandatory_output_failed());
}
