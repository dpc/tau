//! Tests for directory locking behavior.

use super::*;
use crate::tool_started_identity::ownership_probe;

#[cfg(unix)]
#[test]
fn shell_extension_reports_config_error_for_insecure_dir_lock_state_dir() {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::set_permissions(tempdir.path(), path_std_fs::Permissions::from_mode(0o755))
        .expect("chmod tempdir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: CborValue::Map(vec![(
                CborValue::Text("dir_lock".to_owned()),
                CborValue::Map(vec![
                    (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
                    (
                        CborValue::Text("backend".to_owned()),
                        CborValue::Text("filesystem".to_owned()),
                    ),
                    (
                        CborValue::Text("state_dir".to_owned()),
                        CborValue::Text(tempdir.path().display().to_string()),
                    ),
                ]),
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
    assert!(error.message.contains("must be private"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures the directory-lock mechanism remains opt-in while read-only bind
/// mount enforcement defaults on once that mechanism is enabled.
#[test]
fn shell_enforce_ro_bind_defaults_true_under_dir_lock_config() {
    assert!(ExtConfig::default().dir_lock.enforce_ro_bind);

    let config = serde_json::from_value::<ExtConfig>(serde_json::json!({
        "dir_lock": {
            "enforce_ro_bind": false,
        },
    }))
    .expect("parse enforce_ro_bind config");
    assert!(!config.dir_lock.enforce_ro_bind);
}

#[test]
fn reported_lock_wait_duration_seconds_rounds_only_slow_waits() {
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_secs(5)),
        None
    );
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_millis(5001)),
        Some(6)
    );
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_secs(6)),
        Some(6)
    );
}

#[test]
fn lock_wait_duration_header_wraps_non_map_results() {
    let event = with_lock_wait_duration(
        Event::ToolResult(tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "call-lock-wait".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("changed".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Some(6),
    );

    let Event::ToolResult(result) = event else {
        panic!("expected tool result");
    };
    assert_eq!(
        cbor_int_field(&result.result, LOCK_WAIT_DURATION_SECONDS_HEADER),
        Some(6)
    );
    assert_eq!(cbor_map_text(&result.result, "output"), Some("changed"));
}

#[test]
fn lock_wait_duration_header_extends_tool_error_details() {
    let event = with_lock_wait_duration(
        Event::ToolError(tau_proto::ToolError {
            presentation: Default::default(),
            call_id: "call-lock-wait".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            message: "failed".to_owned(),
            details: Some(cbor_text_map(vec![("output", "start failed")])),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Some(7),
    );

    let Event::ToolError(error) = event else {
        panic!("expected tool error");
    };
    let details = error.details.expect("error details");
    assert_eq!(
        cbor_int_field(&details, LOCK_WAIT_DURATION_SECONDS_HEADER),
        Some(7)
    );
    assert_eq!(cbor_map_text(&details, "output"), Some("start failed"));
}

/// Configured instance identity, not a process id or prefix, namespaces
/// metadata.
#[test]
fn configure_instance_name_changes_workdir_metadata_key() {
    let cwd_state = CwdState::new();
    assert_eq!(cwd_state.key().as_str(), "ext_core-shell_cwd");
    let instance_name =
        tau_proto::ExtensionName::parse("project-shell").expect("known-safe extension name");
    assert_eq!(instance_name.to_string(), "project-shell");
    assert_eq!(
        format!("{instance_name:?}"),
        r#"ExtensionName("project-shell")"#
    );
    cwd_state.set_instance_name(instance_name);
    assert_eq!(cwd_state.key().as_str(), "ext_project-shell_cwd");
}

/// Two configured shell instances keep independent metadata namespaces and
/// publish distinct prefix-associated context contributions for the same agent.
#[test]
fn two_shell_instance_workdirs_are_independent_and_prefix_associated() {
    let agent_id = tau_proto::AgentId::parse("agent-two-shells").expect("agent id");
    let first_dir = TempDir::new().expect("first");
    let second_dir = TempDir::new().expect("second");
    let first = CwdState::new();
    first.set_instance_name(
        tau_proto::ExtensionName::parse("core-shell").expect("known-safe extension name"),
    );
    first.set_context_label(None);
    first.set(
        agent_id.clone(),
        first_dir.path().canonicalize().expect("first canonical"),
    );
    let second = CwdState::new();
    second.set_instance_name(
        tau_proto::ExtensionName::parse("prod-shell").expect("known-safe extension name"),
    );
    let prod = tau_proto::ToolNamePrefix::parse("prod").expect("prefix");
    second.set_context_label(Some(&prod));
    second.set(
        agent_id.clone(),
        second_dir.path().canonicalize().expect("second canonical"),
    );
    assert_ne!(first.key(), second.key());
    let Event::ExtAgentContextPublish(first_context) = cwd_context_event(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id.clone(),
        tau_proto::AgentInitializationId::parse("init-1").expect("test identifier must be valid"),
        &first.get(&agent_id).expect("first path"),
        &first,
    ) else {
        unreachable!()
    };
    let Event::ExtAgentContextPublish(second_context) = cwd_context_event(
        "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        agent_id,
        tau_proto::AgentInitializationId::parse("init-1").expect("test identifier must be valid"),
        &second
            .get(&tau_proto::AgentId::parse("agent-two-shells").expect("agent id"))
            .expect("second path"),
        &second,
    ) else {
        unreachable!()
    };
    assert_eq!(first_context.value.0["label"], "default");
    assert_eq!(second_context.value.0["label"], "prod");
    assert_ne!(
        first_context.value.0["path"],
        second_context.value.0["path"]
    );
}

/// Regression guard: the removed GPT `cwd` spelling is not accepted as an
/// unadvertised runtime compatibility alias.
#[test]
fn gpt_shell_does_not_accept_legacy_cwd_as_call_local_workdir() {
    let legacy_override = TempDir::new().expect("legacy override");
    let args = cbor_text_map(vec![
        ("command", "pwd"),
        ("cwd", &legacy_override.path().display().to_string()),
    ]);

    let error = path_crate_tools::shell::run_command_live_for_surface(
        path_crate_tools::ShellSurface::ChatGpt,
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("legacy cwd must be rejected");
    assert_eq!(
        error.message,
        "argument `cwd` is not supported by `shell_command`; use call-local `workdir`"
    );
}

/// Regression guard: a present non-string GPT `workdir` survives admission
/// rewriting for parser rejection and cannot mutate persistent state.
#[test]
fn malformed_gpt_workdir_fails_without_side_effects_through_full_dispatch() {
    let remembered = TempDir::new().expect("remembered");
    let agent_id = tau_proto::AgentId::parse("agent-malformed-gpt-workdir").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(remembered.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");

    writer
        .write_event(&tool_started(
            "call-malformed-gpt-workdir",
            GPT_SHELL_TOOL_NAME,
            CborValue::Map(vec![
                (
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("pwd".to_owned()),
                ),
                (
                    CborValue::Text("workdir".to_owned()),
                    CborValue::Integer(1.into()),
                ),
            ]),
            agent_id.as_str(),
        ))
        .expect("malformed GPT shell call");
    writer.flush().expect("flush call");

    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSetRequest(metadata) => {
                panic!("malformed call-local workdir emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("malformed call-local workdir emitted notice: {notice:?}")
            }
            Event::ToolError(error) if error.call_id.as_str() == "call-malformed-gpt-workdir" => {
                assert_eq!(error.message, "argument `workdir` must be a string");
                break;
            }
            Event::ToolResult(result)
                if result.call_id.as_str() == "call-malformed-gpt-workdir" =>
            {
                panic!("malformed workdir unexpectedly executed: {result:?}")
            }
            _ => {}
        }
    }
}

/// Regression guard: GPT lock inference follows the advertised `workdir`
/// spelling so the frozen lock path and child process directory stay aligned.
#[test]
fn gpt_shell_workdir_selects_automatic_lock_directory() {
    let workdir = TempDir::new().expect("workdir");
    let arguments = cbor_text_map(vec![
        ("command", "pwd"),
        ("workdir", &workdir.path().display().to_string()),
    ]);

    let dirs = crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
        GPT_SHELL_TOOL_NAME,
        &arguments,
        Path::new("/"),
    )
    .expect("GPT shell lock directory");
    assert_eq!(
        dirs,
        vec![workdir.path().canonicalize().expect("canonical workdir")]
    );
}

/// Ensures replace selects the existing file parent for automatic update-lock
/// coordination instead of rejecting lock-enabled dispatch.
#[test]
fn replace_selects_existing_file_parent_for_automatic_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("source.txt");
    fs::write(&path, "before\n").expect("write source");

    let dirs = crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
        REPLACE_TOOL_NAME,
        &replace_arguments(&path, "before", "after"),
        tempdir.path(),
    )
    .expect("replace lock directory");

    assert_eq!(
        dirs,
        vec![tempdir.path().canonicalize().expect("canonical parent")]
    );
}

/// Relative GPT `workdir` admission must freeze one canonical path used by
/// both later lock inference and command execution.
#[test]
fn relative_gpt_workdir_freezes_canonical_lock_path_at_admission() {
    let remembered = TempDir::new().expect("remembered");
    let child = remembered.path().join("child");
    fs::create_dir(&child).expect("child");
    let Event::ToolStarted(invoke) = tool_started(
        "call-relative-gpt-workdir",
        GPT_SHELL_TOOL_NAME,
        cbor_text_map(vec![("command", "pwd"), ("workdir", "child")]),
        "agent-relative-gpt-workdir",
    ) else {
        unreachable!();
    };

    let rewritten = rewrite_invoke_for_cwd(invoke, remembered.path());
    let canonical = child.canonicalize().expect("canonical child");
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "workdir").expect("workdir argument"),
        Some(canonical.display().to_string())
    );
    assert_eq!(
        crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
            GPT_SHELL_TOOL_NAME,
            &rewritten.arguments,
            remembered.path(),
        )
        .expect("lock directory"),
        vec![canonical]
    );
}

/// Ensures omitted workdir path reads state without emitting a metadata
/// transaction.
#[test]
fn workdir_without_path_reports_current_status_without_mutation() {
    let remembered = TempDir::new().expect("remembered");
    let agent_id = tau_proto::AgentId::parse("agent-workdir-read").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(remembered.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    writer
        .write_event(&tool_started(
            "call-workdir-read",
            WORKDIR_TOOL_NAME,
            CborValue::Map(Vec::new()),
            agent_id.as_str(),
        ))
        .expect("workdir read");
    writer.flush().expect("flush read");
    let result = loop {
        let event = reader.read_event().expect("read").expect("result");
        match event {
            Event::ToolResult(result) => break result,
            Event::AgentMetadataSetRequest(metadata) => {
                panic!("workdir read emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("workdir read emitted persistent-change notice: {notice:?}")
            }
            _ => {}
        }
    };
    assert_eq!(
        tau_proto::cbor_text_field(&result.result, "path"),
        Some(remembered.path().display().to_string())
    );
    assert_eq!(
        tau_proto::cbor_text_field(&result.result, "status"),
        Some("available".to_owned())
    );
}

/// Malformed present metadata must fail closed while an absolute setter remains
/// a repair path.
#[test]
fn absolute_workdir_setter_repairs_malformed_metadata() {
    let repaired = TempDir::new().expect("repair directory");
    let agent_id = tau_proto::AgentId::parse("agent-workdir-repair").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(".".to_owned()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("malformed metadata");
    writer.flush().expect("flush malformed");

    writer
        .write_event(&tool_started(
            "call-workdir-repair",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![(
                "path",
                repaired.path().to_str().expect("UTF-8 path"),
            )]),
            agent_id.as_str(),
        ))
        .expect("absolute repair");
    writer.flush().expect("flush repair");
    let metadata = loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSetRequest(metadata) => break metadata,
            Event::ToolResult(result) => panic!("repair completed before commit: {result:?}"),
            _ => {}
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit repair");
    writer.flush().expect("flush commit");
    loop {
        if matches!(
            reader.read_event().expect("read").expect("event"),
            Event::ToolResult(result) if result.call_id.as_str() == "call-workdir-repair"
        ) {
            break;
        }
    }
}

/// Relative filesystem paths resolve from the immutable admitted workdir.
#[test]
fn relative_path_tools_use_admission_workdir() {
    let temp = TempDir::new().expect("tempdir");
    let subdir = temp.path().join("src");
    std::fs::create_dir(&subdir).expect("create src");
    let agent_id = tau_proto::AgentId::parse("agent-cwd-relative").expect("agent id");
    let cwd_state = CwdState::new();
    cwd_state.set(
        agent_id.clone(),
        temp.path().canonicalize().expect("canonical temp"),
    );
    let Event::ToolStarted(invoke) = tool_started(
        "call-find",
        FIND_TOOL_NAME,
        cbor_text_map(vec![("path", "src")]),
        agent_id.as_str(),
    ) else {
        unreachable!();
    };

    let base = cwd_state.get_or_default(&agent_id).expect("remembered cwd");
    let rewritten = rewrite_invoke_for_cwd(invoke, &base);
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "path").expect("path arg"),
        Some(
            subdir
                .canonicalize()
                .expect("canonical src")
                .display()
                .to_string()
        )
    );
}

/// A setter remains pending until the requested canonical value is committed.
#[test]
fn workdir_setter_waits_for_committed_metadata_before_notice_and_result() {
    let temp = TempDir::new().expect("tempdir");
    let start = temp.path().join("start");
    let next = temp.path().join("next");
    fs::create_dir_all(&start).expect("start dir");
    fs::create_dir_all(&next).expect("next dir");
    let agent_id = tau_proto::AgentId::parse("agent-cd-order").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(start.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");

    writer
        .write_event(&tool_started(
            "call-cd-order",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", next.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("cd invoke");
    writer.flush().expect("flush cd");

    let metadata = loop {
        let event = reader
            .read_event()
            .expect("read metadata")
            .expect("metadata");
        assert!(
            !matches!(event, Event::ToolResult(_)),
            "cd result before metadata commit"
        );
        assert!(
            !matches!(event, Event::AgentUserMessageInjected(_)),
            "cd notice before metadata commit"
        );
        if let Event::AgentMetadataSetRequest(metadata) = event {
            break metadata;
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit cwd");
    writer.flush().expect("flush commit");

    let notice = reader.read_event().expect("read notice").expect("notice");
    let Event::AgentUserMessageInjected(notice) = notice else {
        panic!("expected cwd notice");
    };
    assert_eq!(notice.agent_id, agent_id);
    assert!(notice.text.contains(next.to_str().expect("utf8 next")));
    let result = reader.read_event().expect("read result").expect("result");
    assert!(
        matches!(result, Event::ToolResult(result) if result.call_id.as_str() == "call-cd-order")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// One agent and instance may have only one outstanding persistent setter.
#[test]
fn overlapping_same_agent_workdir_setter_is_rejected_until_first_commit() {
    let temp = TempDir::new().expect("tempdir");
    let start = temp.path().join("start");
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    fs::create_dir_all(&start).expect("start dir");
    fs::create_dir_all(&one).expect("one dir");
    fs::create_dir_all(&two).expect("two dir");
    let agent_id = tau_proto::AgentId::parse("agent-cd-overlap").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(start.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");

    writer
        .write_event(&tool_started(
            "call-cd-one",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", one.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("first cd");
    writer.flush().expect("flush first cd");
    let first_metadata = loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSetRequest(metadata) => break metadata,
            Event::ToolResult(result) => panic!("first cd completed before commit: {result:?}"),
            _ => {}
        }
    };

    writer
        .write_event(&tool_started(
            "call-cd-two",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", two.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("second cd");
    writer.flush().expect("flush second cd");
    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::ToolError(error) if error.call_id.as_str() == "call-cd-two" => {
                assert!(error.message.contains("workdir change is already pending"));
                break;
            }
            Event::AgentMetadataSetRequest(metadata) => {
                panic!("second cd emitted metadata while first was pending: {metadata:?}");
            }
            Event::ToolResult(result) if result.call_id.as_str() == "call-cd-one" => {
                panic!("first cd completed before commit: {result:?}");
            }
            _ => {}
        }
    }

    writer
        .write_event(&Event::AgentMetadataSet(first_metadata))
        .expect("commit first cwd");
    writer.flush().expect("flush commit");
    let _ = reader.read_event().expect("read notice").expect("notice");
    let result = reader.read_event().expect("read result").expect("result");
    assert!(
        matches!(result, Event::ToolResult(result) if result.call_id.as_str() == "call-cd-one")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Replayed `session.agent_loaded` catch-up snapshots must rebuild cwd context
/// and emit readiness for already-loaded agents without running live tools.
#[test]
fn replayed_session_agent_loaded_restores_workdir_context_and_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-replay-cwd").expect("agent id");
    let cwd = PathBuf::from("/tmp/replayed-cwd");

    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
                value: CborValue::Text(cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
        ))
        .expect("replay cwd metadata");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(2),
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        ))
        .expect("replay loaded snapshot");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush replay");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "workdir" =>
            {
                assert_eq!(
                    publish.value.0["path"],
                    serde_json::json!(cwd.display().to_string())
                );
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                assert!(saw_context, "cwd context should precede ready");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A live existing-agent load must wait for replayed cwd metadata before
/// publishing cwd context or falling back to process-default cwd metadata.
#[test]
fn live_loaded_existing_agent_uses_replayed_workdir_before_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-live-replay-cwd").expect("agent id");
    let stored_cwd = PathBuf::from("/tmp/stored-live-cwd");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("live load");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
                value: CborValue::Text(stored_cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
        ))
        .expect("replay cwd");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::AgentMetadataSetRequest(metadata) if metadata.agent_id == agent_id => {
                panic!("default cwd metadata emitted before stored cwd replay: {metadata:?}");
            }
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "workdir" =>
            {
                assert_eq!(
                    publish.value.0["path"],
                    serde_json::json!(stored_cwd.display().to_string())
                );
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                assert!(saw_context, "stored cwd context should precede ready");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A live load with no replayed cwd still falls back to process-default
/// metadata after the per-agent replay boundary, preserving new-agent
/// initialization.
#[test]
fn live_loaded_agent_defaults_workdir_after_replay_boundary_without_metadata() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-live-default-cwd").expect("agent id");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("live load");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush");

    loop {
        let event = reader.read_event().expect("read").expect("event");
        if let Event::AgentMetadataSetRequest(metadata) = event
            && metadata.agent_id == agent_id
        {
            assert_eq!(metadata.key.as_str(), "ext_test-extension_cwd");
            break;
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// An errored per-agent replay boundary must fail closed: shell must not invent
/// default cwd metadata or publish readiness when restore failed.
#[test]
fn live_loaded_agent_does_not_default_workdir_after_replay_error() {
    let (tx, rx) = path_std_sync::mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let agent_id = tau_proto::AgentId::parse("agent-live-error-cwd").expect("agent id");

    runtime
        .handle_event(
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
            false,
        )
        .expect("live load");
    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text("/tmp/restored-before-error".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
            true,
        )
        .expect("replay cwd metadata");
    runtime
        .handle_event(
            Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: Some("corrupt agent log".to_owned()),
            }),
            false,
        )
        .expect("errored replay boundary");

    while let Ok(message) = rx.try_recv() {
        let HarnessInputMessage::Emit(emit) = message else {
            continue;
        };
        match *emit.event {
            Event::AgentMetadataSetRequest(metadata) if metadata.agent_id == agent_id => {
                panic!("errored replay must not synthesize cwd metadata: {metadata:?}");
            }
            Event::ExtAgentContextPublish(publish) if publish.agent_id == agent_id => {
                panic!("errored replay must not publish cwd context: {publish:?}");
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                panic!("errored replay must not mark cwd context ready: {ready:?}");
            }
            _ => {}
        }
    }
    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text("/tmp/late".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
            false,
        )
        .expect("late metadata");
    runtime
        .handle_event(
            Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
            }),
            false,
        )
        .expect("late unset");
    runtime
        .handle_event(
            Event::UiShellCommand(tau_proto::UiShellCommand {
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                command_id: tau_proto::ShellCommandId::parse("replay-failed-shell")
                    .expect("test identifier must satisfy its grammar"),
                command: "pwd".to_owned(),
                include_in_context: false,
                target_agent_id: Some(agent_id),
            }),
            false,
        )
        .expect("replay-failed command is a local failure");
    let failure = rx.recv().expect("replay-failed terminal result");
    assert!(matches!(
        failure,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ShellCommandFinishedReported(finished)
                if finished.output.contains("replay failed"))
    ));
    runtime.final_shutdown();
}

/// Malformed live metadata publishes invalid status before context readiness.
#[test]
fn malformed_workdir_metadata_does_not_wedge_context_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-bad-cwd").expect("agent id");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("load");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(
                    "s1".parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                ),
                error: None,
            },
        )))
        .expect("agent replay boundary");
    writer.flush().expect("flush load");
    let _ = reader
        .read_event()
        .expect("read initial metadata")
        .expect("metadata");

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Bool(true),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("bad metadata");
    writer.flush().expect("flush bad metadata");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::ExtAgentContextPublish(publish) if publish.key.as_ref() == "workdir" => {
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) => {
                assert!(saw_context);
                assert_eq!(ready.agent_id, agent_id);
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}
/// Static dispatch and dynamic registration refreshes both preserve a
/// configured wire namespace while matching logical Shell tool names
/// internally.
#[test]
fn prefixed_shell_dispatch_and_dir_lock_refresh_use_wire_names() {
    let prefix = tau_proto::ToolNamePrefix::parse("work").expect("prefix");
    let (mut reader, mut writer, done_rx) =
        spawn_extension_with_exit_and_prefix(Some(prefix.clone()));
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: Some(prefix),
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(true))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("enable dir_lock");
    writer.flush().expect("flush config");
    let refreshed = reader.read_event().expect("read").expect("refresh");
    let Event::ToolRegistrationDeclared(register) = refreshed else {
        panic!("expected ToolRegistrationDeclared, got {refreshed:?}");
    };
    assert_eq!(register.tool.name.as_str(), "work_dir_lock");

    writer
        .write_event(&tool_started(
            "prefixed-echo",
            "work_echo",
            cbor_map(vec![("text", CborValue::Text("hello".to_owned()))]),
            "agent-a",
        ))
        .expect("invoke");
    writer.flush().expect("flush invocation");
    let result = loop {
        let event = reader.read_event().expect("read").expect("result");
        match event {
            Event::ToolResult(result) => break result,
            Event::AgentMetadataSetRequest(metadata) => {
                panic!("workdir read emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("workdir read emitted persistent-change notice: {notice:?}")
            }
            _ => {}
        }
    };
    assert_eq!(result.tool_name.as_str(), "work_echo");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension exit")
        .expect("extension ok");
}

/// A targeted `!`/`!!` command snapshots that agent's committed instance
/// workdir.
#[test]
fn targeted_user_shell_runs_from_agent_workdir() {
    let workdir = TempDir::new().expect("workdir");
    let agent_id = tau_proto::AgentId::parse("agent-user-shell-workdir").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(workdir.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    writer
        .write_event(&Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            command_id: tau_proto::ShellCommandId::parse("ui-targeted-pwd")
                .expect("test identifier must satisfy its grammar"),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id),
        }))
        .expect("targeted user shell");
    writer.flush().expect("flush command");
    let finished = wait_for_user_shell_finished(&mut reader, "ui-targeted-pwd");
    assert!(
        finished
            .output
            .contains(workdir.path().to_str().expect("UTF-8 path"))
    );
}

#[test]
fn startup_registers_dir_lock_disabled_by_default() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_dir_lock = false;
    for _ in 0..13 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == DIR_LOCK_TOOL_NAME {
            assert!(!register.tool.enabled_by_default);
            assert_eq!(
                register.tool.tags,
                vec![tau_proto::ToolTag::new(tau_proto::TURN_WAIT_TOOL_TAG)],
                "disabled dir_lock retains static activity classification"
            );
            found_dir_lock = true;
        }
    }
    assert!(found_dir_lock, "expected dir_lock registration");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn startup_publishes_shell_dir_force_unlock_action() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_schema = false;
    for _ in 0..17 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ActionSchemaDeclared(published) = event else {
            continue;
        };
        published.schema.validate().expect("schema validates");
        let root = published
            .schema
            .roots
            .iter()
            .find(|root| root.name == ":shell-dir-force-unlock")
            .expect("force unlock action root");
        assert_eq!(
            root.action_id.as_deref(),
            Some(SHELL_DIR_FORCE_UNLOCK_ACTION_ID)
        );
        assert_eq!(root.args.len(), 1);
        assert_eq!(root.args[0].name, "directory");
        assert!(matches!(
            root.args[0].kind,
            tau_actions::ActionArgKind::RestString
        ));
        found_schema = true;
    }
    assert!(found_schema, "expected shell action schema");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_dir_force_unlock_releases_overlapping_manual_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().join("root");
    let child_dir = lock_dir.join("child");
    fs::create_dir_all(&child_dir).expect("child dir");
    let edit_path = child_dir.join("file.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&action_invoke(
            "force-unlock-1",
            SHELL_DIR_FORCE_UNLOCK_ACTION_ID,
            &child_dir.display().to_string(),
        ))
        .expect("force unlock");
    writer.flush().expect("flush force unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ActionResultReported(result))
                if result.invocation_id.as_str() == "force-unlock-1" =>
            {
                let tau_proto::ActionOutput::Text { text } = result.output else {
                    panic!("expected text output");
                };
                assert!(text.contains("Force-unlocked 1 manual directory lock"));
                assert!(text.contains("owner=agent-a"));
                assert!(text.contains(&lock_dir.display().to_string()));
                break;
            }
            Some(Event::ActionErrorReported(error))
                if error.invocation_id.as_str() == "force-unlock-1" =>
            {
                panic!("force unlock failed: {}", error.message);
            }
            Some(_) => continue,
            None => panic!("extension closed before force unlock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "edit-after-force-unlock",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "edit-after-force-unlock" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "edit-after-force-unlock" =>
            {
                panic!("edit still waited after force unlock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit result"),
        }
    }
    assert_eq!(
        fs::read_to_string(&edit_path).expect("edited file"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_config_re_registers_tool_enabled_when_config_true() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolRegistrationDeclared(register))
                if register.tool.name == DIR_LOCK_TOOL_NAME =>
            {
                assert!(register.tool.enabled_by_default);
                assert!(
                    register
                        .tool
                        .tags
                        .iter()
                        .any(|tag| tag.as_str() == "shell:lock")
                );
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before dir_lock re-registration"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Initial configuration refreshes override static default declarations before
/// Ready, leaving the effective dir_lock registration enabled and tagged.
#[test]
fn initial_dir_lock_override_is_final_before_ready() {
    let mut input = Vec::new();
    let mut input_writer = tau_proto::HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(true))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    input_writer.flush().expect("flush input");
    let output = SharedWriter::default();
    let written = output.clone();
    run_impl(
        path_std_io::Cursor::new(input),
        output,
        DiscoverySourcePolicy::Environment,
        RuntimeCwdSource::Process,
    )
    .expect("run shell");

    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("frame") {
        frames.push(frame);
    }
    let registrations = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == DIR_LOCK_TOOL_NAME =>
                {
                    Some((index, &register.tool))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let (override_index, final_tool) = registrations.last().expect("dir_lock registration");
    assert!(final_tool.enabled_by_default);
    assert!(
        final_tool
            .tags
            .iter()
            .any(|tag| tag.as_str() == "shell:lock")
    );
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("Ready");
    assert!(*override_index < ready_index);
}

/// A live workdir setter's metadata prerequisite must fail the extension loop
/// while disconnect cleanup still owns the uncommitted setter transaction.
#[test]
fn mandatory_workdir_prerequisite_failure_exits_production_manual_loop() {
    let tempdir = TempDir::new().expect("workdir target");
    let event = tool_started(
        "call-workdir-prerequisite-failure",
        WORKDIR_TOOL_NAME,
        cbor_text_map(vec![(
            "path",
            tempdir.path().to_str().expect("UTF-8 workdir"),
        )]),
        "main",
    );
    assert_mandatory_frame_failure_exits(
        event,
        b"agent.metadata_set_request",
        "workdir metadata prerequisite",
    );
}

/// Directory-lock validation errors use the same checked sole-terminal path
/// after actual detached FIFO exhaustion.
#[test]
fn saturated_production_fifo_preserves_dir_lock_error() {
    let frames = run_after_production_fifo_saturation(
        vec![tool_started(
            "call-saturated-dir-lock-error",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![("command", "invalid"), ("directory", "/tmp")]),
            "main",
        )],
        false,
    );
    assert_eq!(
        count_reported_terminal(
            &frames,
            "call-saturated-dir-lock-error",
            EventName::TOOL_ERROR_REPORTED,
        ),
        1
    );
}

/// A live workdir setter admitted after actual detached FIFO exhaustion must
/// publish its sole correlated metadata prerequisite exactly once.
#[test]
fn saturated_production_fifo_preserves_workdir_prerequisite() {
    let target = TempDir::new().expect("workdir target");
    let frames = run_after_production_fifo_saturation(
        vec![tool_started(
            "call-saturated-workdir",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![(
                "path",
                target.path().to_str().expect("UTF-8 workdir"),
            )]),
            "main",
        )],
        true,
    );
    let requests = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(
                        emit.event.as_ref(),
                        Event::AgentMetadataSetRequest(request)
                            if request.mutation_id.is_some()
                    )
            )
        })
        .count();
    assert_eq!(requests, 1);
    assert_eq!(
        count_reported_terminal(
            &frames,
            "call-saturated-workdir",
            EventName::TOOL_RESULT_REPORTED,
        ),
        1
    );
}

/// Ensures cancellation after a directory-lock waiter becomes a held guard but
/// before effect registration prevents mutation and emits one cancelled
/// terminal.
#[test]
fn cancellation_after_lock_acquisition_prevents_mutation_once() {
    let tempdir = TempDir::new().expect("tempdir");
    let edit_path = tempdir.path().join("lock-handoff-edit.txt");
    fs::write(&edit_path, "old\n").expect("initial file");
    let (tx, rx) = path_std_sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(crate::scheduler::SchedulerConfig {
        control_workers: 0,
        user_workers: 0,
        cheap_workers: 0,
        general_workers: 1,
        ..Default::default()
    });
    let lifecycles = ToolLifecycleRegistry::default();
    let (reached_tx, reached_rx) = path_std_sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = path_std_sync::mpsc::channel();
    lifecycles.pause_after_lock(reached_tx, resume_rx);
    let lock_manager = DirLockManager::default();
    let blocker = lock_manager
        .acquire_auto(
            tau_proto::ToolCallId::new("lock-handoff-blocker"),
            tau_proto::AgentId::parse("agent-b").expect("agent id"),
            vec![tempdir.path().canonicalize().expect("canonical tempdir")],
            || {},
        )
        .expect("conflicting automatic lock");
    let Event::ToolStarted(invoke) = tool_started(
        "lock-handoff-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&edit_path, vec![line_edit(1, 2, "new\n")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let call_id = invoke.call_id.clone();
    let local_tool_name = invoke.tool_name.clone();
    let mut config = ExtConfig::default();
    config.dir_lock.enable = true;

    schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        config,
        lock_manager,
        ToolCancellationState {
            lifecycles: lifecycles.clone(),
            ..Default::default()
        },
        CwdState::new(),
    )
    .expect("edit scheduled");
    let HarnessInputMessage::Emit(waiting) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lock waiting progress")
    else {
        panic!("expected lock waiting emit");
    };
    assert!(matches!(*waiting.event, Event::ToolProgressReported(_)));
    drop(blocker);
    reached_rx.recv().expect("worker reached lock handoff");
    assert_eq!(
        lifecycles.cancel(&call_id),
        Some(crate::tool_lifecycle::CancelOutcome::PreventedEffect)
    );
    resume_tx.send(()).expect("resume worker");
    drop(scheduler);

    assert_cancelled_terminal_once(&rx, &call_id);
    assert_eq!(fs::read_to_string(&edit_path).expect("file"), "old\n");
}

#[test]
fn dir_lock_tool_can_be_disabled_by_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, false);

    writer
        .write_event(&tool_started(
            "lock-disabled",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &tempdir.path().display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush invoke");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "lock-disabled" => {
                assert!(error.message.contains("dir_lock is disabled"));
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before dir_lock error"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Keeps the concise model-facing purpose text separate from configuration and
/// argument details recorded elsewhere in the schema.
#[test]
fn dir_lock_tool_has_concise_model_facing_description() {
    assert_eq!(
        dir_lock_tool_spec(false).description.as_deref(),
        Some(
            "Lock or unlock a directory and its contents for updates. Waits for the lock when \
             necessary."
        )
    );
}

#[test]
fn dir_lock_blocks_conflicting_edit_until_unlock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("file.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);
    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-edit" =>
            {
                assert!(progress.message.as_deref().is_some_and(|message| {
                    message.contains(lock_dir.to_str().expect("lock dir path is UTF-8"))
                }));
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                panic!("edit completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit progress"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    let mut saw_unlock = false;
    let mut saw_edit = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(saw_unlock && saw_edit) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unlock/edit"
        );
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-root" => {
                saw_unlock = true;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                saw_edit = true;
            }
            Some(_) => continue,
            None => panic!("extension closed before edit result"),
        }
    }
    assert_eq!(
        fs::read_to_string(&edit_path).expect("edited file"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn disconnect_cancels_active_dir_lock_waiter_before_scheduler_join() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("file.txt");
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-edit" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                panic!("edit completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit progress"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("extension should exit promptly after disconnect")
        .expect("extension run should succeed");
}

/// A full-dispatch mutation waiting on a directory lock must retain the workdir
/// captured at admission even when metadata commits a different path meanwhile.
#[test]
fn locked_apply_patch_uses_workdir_frozen_at_admission() {
    let tempdir = TempDir::new().expect("tempdir");
    let cwd_a = tempdir.path().join("a");
    let cwd_b = tempdir.path().join("b");
    fs::create_dir_all(&cwd_a).expect("create a");
    fs::create_dir_all(&cwd_b).expect("create b");
    fs::write(cwd_a.join("file.txt"), "before\n").expect("write a");
    fs::write(cwd_b.join("file.txt"), "before\n").expect("write b");
    let agent_id = tau_proto::AgentId::parse("agent-patch-cwd-lock").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_a.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");

    writer
        .write_event(&tool_started(
            "lock-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch";
    writer
        .write_event(&tool_started(
            "blocked-patch",
            APPLY_PATCH_TOOL_NAME,
            CborValue::Text(patch.to_owned()),
            agent_id.as_str(),
        ))
        .expect("apply_patch");
    writer.flush().expect("flush patch");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-patch" =>
            {
                assert!(progress.message.as_deref().is_some_and(|message| {
                    message.contains(cwd_a.to_str().expect("cwd a path is UTF-8"))
                }));
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-patch" => {
                panic!("apply_patch completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before apply_patch progress"),
        }
    }

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_b.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("move cwd while waiting");
    writer.flush().expect("flush cwd b");

    writer
        .write_event(&tool_started(
            "unlock-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    let mut saw_unlock = false;
    let mut saw_patch = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(saw_unlock && saw_patch) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unlock/apply_patch"
        );
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-a" => {
                saw_unlock = true
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-patch" => {
                saw_patch = true
            }
            Some(_) => continue,
            None => panic!("extension closed before apply_patch result"),
        }
    }
    assert_eq!(
        fs::read_to_string(cwd_a.join("file.txt")).expect("read a"),
        "after\n"
    );
    assert_eq!(
        fs::read_to_string(cwd_b.join("file.txt")).expect("read b"),
        "before\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_without_manual_lock_does_not_wait_for_update_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let cwd_a = tempdir.path().join("a");
    let cwd_b = tempdir.path().join("b");
    fs::create_dir_all(&cwd_a).expect("create a");
    fs::create_dir_all(&cwd_b).expect("create b");
    let agent_id = tau_proto::AgentId::parse("agent-shell-cwd-lock").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_a.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");

    writer
        .write_event(&tool_started(
            "lock-shell-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-shell-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![("command", "printf after > shell.txt")]),
            agent_id.as_str(),
        ))
        .expect("shell");
    writer.flush().expect("flush shell");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-shell" => {
                let display = result.display.expect("display");
                assert_eq!(display.mode, "ro");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-shell-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for unlock");
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-shell-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }
    assert!(!cwd_b.join("shell.txt").exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_with_covering_manual_lock_displays_inferred_read_write_mode() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "rw-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "printf rw-ok"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("rw shell");
    writer.flush().expect("flush shell");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "rw-shell" =>
            {
                assert_eq!(progress.display.expect("display").mode, "rw");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell progress"),
        }
    }
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "rw-shell" => {
                assert_eq!(result.display.expect("display").mode, "rw");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_releases_delegate_locks_on_start_agent_result() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            start_id: tau_proto::StartOperationId(1),
            query_id: "delegate-locker".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent-locker").expect("agent id"),
        }))
        .expect("start accepted");
    writer
        .write_event(&tool_started(
            "lock-by-delegate",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush delegate lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-by-delegate" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before delegate lock result"),
        }
    }

    // Delegates can finish without issuing an explicit unlock. Tau keeps their
    // session agent loaded for history, so ext-shell must release manual locks
    // on the start-result lifecycle event rather than waiting only for a later
    // SessionAgentUnloaded event.
    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: "delegate-locker".to_owned(),
            text: "done".to_owned(),
            error: None,
        }))
        .expect("start result");
    writer
        .write_event(&tool_started(
            "lock-after-delegate-result",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-b",
        ))
        .expect("dir_lock update after result");
    writer.flush().expect("flush after result lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "lock-after-delegate-result" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-delegate-result" =>
            {
                panic!("lock waited after delegate lifecycle release: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before second lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures committed session unload is ext-shell's general agent cleanup
/// authority: it releases the unloaded agent's manual lock before a queued
/// waiter is granted ownership.
#[test]
fn dir_lock_releases_agent_locks_on_session_agent_unloaded() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-before-unload",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-unloaded",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush owner lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-before-unload" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before owner lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-after-unload",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-waiter",
        ))
        .expect("queued dir_lock update");
    writer.flush().expect("flush queued lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-unload" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-after-unload" => {
                panic!("waiter acquired lock before unload: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before queued lock progress"),
        }
    }

    writer
        .write_event(&Event::SessionAgentUnloaded(
            tau_proto::SessionAgentUnloaded {
                session_id: tau_proto::SessionId::parse("session-unload-lock")
                    .expect("known-safe SessionId must be valid"),
                agent_id: tau_proto::AgentId::parse("agent-unloaded").expect("agent id"),
            },
        ))
        .expect("session agent unloaded");
    writer.flush().expect("flush unload");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-after-unload" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before waiter lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_unlock_can_target_another_owner() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-owner",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-owner" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "force-unlock-owner",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
                ("owner_agent_id", "agent-a"),
            ]),
            "agent-b",
        ))
        .expect("dir_lock force unlock");
    writer.flush().expect("flush force unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "force-unlock-owner" => {
                break;
            }
            Some(Event::ToolError(error)) if error.call_id.as_str() == "force-unlock-owner" => {
                panic!("force unlock failed: {error:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before force unlock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-after-force-unlock",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-c",
        ))
        .expect("dir_lock after force unlock");
    writer.flush().expect("flush second lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "lock-after-force-unlock" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-force-unlock" =>
            {
                panic!("second lock waited after force unlock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before second lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_unlock_rejects_wrong_type_owner_agent_id() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "unlock-bad-owner-type",
            DIR_LOCK_TOOL_NAME,
            cbor_map(vec![
                ("command", CborValue::Text("unlock".to_owned())),
                ("directory", CborValue::Text(lock_dir.display().to_string())),
                ("owner_agent_id", CborValue::Integer(1.into())),
            ]),
            "agent-b",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "unlock-bad-owner-type" => {
                assert_eq!(error.message, "argument `owner_agent_id` must be a string");
                break;
            }
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "unlock-bad-owner-type" =>
            {
                panic!("unlock should fail, got {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_update_errors_when_same_agent_already_holds_overlapping_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let child_dir = lock_dir.join("child");
    fs::create_dir(&child_dir).expect("child dir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush first lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => {
                let display = result.display.as_ref().expect("lock display");
                assert_eq!(display.args, format!("update {}", lock_dir.display()));
                assert_eq!(display.status_text, "ok");
                assert!(display.payload.is_none());
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before first lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-child-again",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &child_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock duplicate update");
    writer.flush().expect("flush duplicate lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "lock-child-again" => {
                assert_eq!(error.message, "dir_lock_duplicate");
                assert!(
                    !error
                        .message
                        .contains(lock_dir.to_str().expect("utf8 path"))
                );
                let details = error.details.as_ref().expect("structured details");
                assert_eq!(
                    cbor_map_text(details, "blocking_directory"),
                    Some(lock_dir.to_str().expect("utf8 path"))
                );
                assert_eq!(
                    cbor_map_text(details, "requested_directory"),
                    Some(child_dir.to_str().expect("utf8 path"))
                );
                assert_eq!(cbor_map_text(details, "lock_owner_id"), Some("agent-a"));
                assert_eq!(
                    cbor_map_text(details, "output"),
                    Some(
                        "Directory lock already held by this agent. Unlock the existing lock before locking another overlapping directory."
                    )
                );
                let display = error.display.as_ref().expect("error display");
                assert_eq!(display.args, format!("update {}", child_dir.display()));
                assert_eq!(display.status_text, "dir_lock failed");
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-child-again" => {
                panic!("duplicate manual lock succeeded: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before duplicate lock error"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-edit",
            EDIT_TOOL_NAME,
            edit_arguments(
                &child_dir.join("file.txt"),
                vec![context_half_open_edit(1, 1, "hello", "")],
            ),
            "agent-a",
        ))
        .expect("same-agent edit");
    writer.flush().expect("flush same-agent edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-edit" => {
                assert_eq!(
                    fs::read_to_string(child_dir.join("file.txt")).expect("same-agent edit file"),
                    "hello\n"
                );
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-edit" =>
            {
                panic!("same-agent automatic edit waited on its own manual lock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before same-agent edit result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_waiting_progress_preserves_shell_mode() {
    let tempdir = TempDir::new().expect("tempdir");
    let Event::ToolStarted(invoke) = tool_started(
        "blocked-shell",
        SHELL_TOOL_NAME,
        cbor_text_map(vec![(
            "command",
            &format!("printf hello\n# {}", "x".repeat(1024 * 1024)),
        )]),
        "agent-b",
    ) else {
        panic!("expected tool started");
    };
    ownership_probe::start("blocked-shell");
    let progress = crate::dir_lock::waiting_progress(
        &invoke,
        &[tempdir.path().to_path_buf()],
        Some(path_crate_tools_shell::ShellCommandMode::visible(
            path_crate_tools_shell::ShellAccessMode::ReadWrite,
        )),
    );
    let display = progress.display.expect("waiting display");

    assert_eq!(display.mode, "rw");
    assert_eq!(display.args, tempdir.path().display().to_string());
    assert_eq!(display.info_chips, vec!["dir lock"]);
    assert_eq!(display.status, ToolUseStatus::InProgress);
    assert_eq!(display.status_text, "waiting");
    let tau_proto::ToolUsePayload::Text { text } = display.payload.expect("command payload") else {
        panic!("expected text payload");
    };
    assert!(text.len() <= 4096);
    let work = ownership_probe::finish("blocked-shell");
    assert!(work.lock_wait_snapshot_bytes <= 12_320);
}

/// Directory wait labels stream many multibyte paths directly into their cap
/// without splitting UTF-8 or first materializing the full join.
#[test]
fn directory_wait_display_bounds_many_unicode_paths() {
    let dirs = (0..100)
        .map(|index| PathBuf::from(format!("/tmp/{index}/{}", "🦀".repeat(100))))
        .collect::<Vec<_>>();
    let display = crate::dir_lock::bounded_dirs_display(&dirs, 4096);
    assert!(display.len() <= 4096);
    assert!(display.ends_with("..."));
    assert!(std::str::from_utf8(display.as_bytes()).is_ok());
}

/// Drives real admission, scheduler, alias scoping, and automatic lock waiting
/// with 1–8 MiB commands; the queued payload must move without a deep clone and
/// the independently retained wait snapshot must remain bounded.
#[test]
fn large_prefixed_edit_lock_wait_moves_one_payload_and_bounds_progress() {
    for mib in [1, 2, 4, 8] {
        let tempdir = TempDir::new().expect("tempdir");
        let canonical = tempdir.path().canonicalize().expect("canonical tempdir");
        let path = canonical.join("large.txt");
        fs::write(&path, "before").expect("seed edit target");
        let call_id = format!("large-lock-wait-{mib}");
        let agent_id = tau_proto::AgentId::parse("large-wait-agent").expect("agent id");
        let blocker_agent = tau_proto::AgentId::parse("large-wait-blocker").expect("agent id");
        let lock_manager = DirLockManager::default();
        let blocker = lock_manager
            .acquire_auto(
                tau_proto::ToolCallId::new(format!("blocker-{mib}")),
                blocker_agent,
                vec![canonical.clone()],
                || {},
            )
            .expect("blocking lock");
        let scheduler = WorkScheduler::new(crate::scheduler::SchedulerConfig {
            queued_bytes_limit: 16 * 1024 * 1024,
            control_workers: 0,
            user_workers: 0,
            cheap_workers: 0,
            general_workers: 1,
            ..Default::default()
        });
        let (tx, rx) = path_std_sync::mpsc::channel();
        let output = Output::channel(tx);
        let mut config = ExtConfig::default();
        config.dir_lock.enable = true;
        let cwd_state = CwdState::new_with_startup_cwd(canonical.clone());
        let invoke = tau_proto::ToolStarted {
            invocation_policy: Default::default(),
            call_id: tau_proto::ToolCallId::new(&call_id),
            tool_name: tau_proto::ToolName::new("prefix_replace"),
            arguments: replace_arguments(
                &path,
                "before",
                &format!("after{}", "x".repeat(mib * 1024 * 1024)),
            ),
            agent_id,
            originator: tau_proto::PromptOriginator::User,
        };
        ownership_probe::start(&call_id);
        schedule_tool_started(
            (invoke, &tau_proto::ToolName::new(REPLACE_TOOL_NAME)),
            &scheduler,
            &output,
            config,
            lock_manager.clone(),
            ToolCancellationState::default(),
            cwd_state,
        )
        .expect("large shell scheduled");

        let progress = loop {
            let HarnessInputMessage::Emit(progress) = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("waiting progress")
            else {
                continue;
            };
            if let Event::ToolProgressReported(progress) = *progress.event
                && progress
                    .message
                    .as_deref()
                    .is_some_and(|message| message.starts_with("waiting for directory lock"))
            {
                break progress;
            }
        };
        assert_eq!(progress.tool_name.as_str(), "prefix_replace");
        let display = progress.display.expect("waiting display");
        assert!(display.args.len() <= 4096);
        if let Some(tau_proto::ToolUsePayload::Text { text }) = display.payload {
            assert!(text.len() <= 4096, "wait payload bytes={}", text.len());
        }

        assert!(lock_manager.cancel_waiting_call(&tau_proto::ToolCallId::new(&call_id)));
        drop(blocker);
        drop(scheduler);
        let work = ownership_probe::finish(&call_id);
        assert_eq!(work.argument_clones, 0);
        assert_eq!(work.identity_clones, 1);
        assert_eq!(work.ingress_text_ptr, work.execution_text_ptr);
        assert!(mib * 1024 * 1024 <= work.queued_argument_bytes);
        assert!(work.lock_wait_snapshot_bytes <= 12_320);

        let terminal = rx
            .try_iter()
            .find_map(|message| match message {
                HarnessInputMessage::Emit(emit) => match *emit.event {
                    Event::ToolCancelledReported(cancelled) => Some(cancelled),
                    _ => None,
                },
                _ => None,
            })
            .expect("cancelled terminal");
        assert_eq!(terminal.tool_name.as_str(), "prefix_replace");
    }
}

#[test]
fn inferred_read_only_shell_bypasses_directory_update_lock() {
    // Shell commands without a covering manual lock behave like read tools for
    // advisory directory locking: they may run while another agent holds an
    // update lock.
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);
    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "read-only-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "printf ro-ok"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-b",
        ))
        .expect("ro shell");
    writer.flush().expect("flush shell");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "read-only-shell" =>
            {
                assert_ne!(
                    progress.message.as_deref(),
                    Some("waiting for directory lock")
                );
                assert!(
                    !progress
                        .message
                        .as_deref()
                        .is_some_and(|message| message.starts_with("waiting for directory lock")),
                    "ro shell unexpectedly waited on directory lock: {progress:?}"
                );
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "read-only-shell" => {
                assert_eq!(
                    optional_argument_text(&result.result, "output"),
                    Ok(Some("out(no_nl) ro-ok".to_owned()))
                );
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before ro shell result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn same_agent_edit_reenters_manual_lock_while_shell_auto_lock_is_active() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("while-shell-runs.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "sleep 1; printf shell-done"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("same-agent shell");
    writer.flush().expect("flush shell");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-shell" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                panic!("shell completed before test edit could be issued: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before shell progress"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-a",
        ))
        .expect("same-agent edit");
    writer.flush().expect("flush edit");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-edit" => {
                assert_eq!(
                    fs::read_to_string(&edit_path).expect("same-agent edit file"),
                    "hello\n"
                );
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-edit" =>
            {
                panic!("same-agent edit waited on its own active automatic lock: {progress:?}");
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                panic!("same-agent edit was blocked until shell finished: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before same-agent edit result"),
        }
    }

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// The two shell registrations must advertise noninteractive command guidance,
/// their respective provider-facing directory spellings, and timeout schema
/// differences.
#[test]
fn startup_registers_surface_specific_shell_workdir_schemas() {
    // The model-visible schema must advertise the implemented working-directory
    // argument and reject negative timeouts before invocation. Directory update
    // coordination is handled inside ext-shell when dir_lock is enabled, not by
    // harness execution modes.
    let (mut reader, mut writer) = spawn_extension();

    let mut found_shell = false;
    let mut found_gpt_shell = false;
    for _ in 0..13 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == SHELL_TOOL_NAME || register.tool.name == GPT_SHELL_TOOL_NAME {
            let description = register.tool.description.as_deref().expect("description");
            assert!(description.contains(
                "Stdin is closed and commands cannot receive interactive input. Stdout and stderr may be TTY-backed even though no controlling terminal exists. Use explicit noninteractive flags/messages; do not launch prompts, pagers, or editors."
            ));
            if register.tool.name == SHELL_TOOL_NAME {
                assert!(description.contains("structured command results"));
            }
            assert!(!description.contains("tool errors"));
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let properties = &parameters["properties"];
            assert!(properties["mode"].is_null());
            if register.tool.name == SHELL_TOOL_NAME {
                assert_eq!(properties["cwd"]["type"], serde_json::json!("string"));
                assert!(properties["workdir"].is_null());
                assert_eq!(properties["timeout"]["minimum"], serde_json::json!(0));
            } else {
                assert_eq!(
                    register.tool.model_visible_name.as_deref(),
                    Some("shell_command")
                );
                assert_eq!(properties["workdir"]["type"], serde_json::json!("string"));
                assert!(properties["cwd"].is_null());
                assert!(
                    properties["workdir"]["description"]
                        .as_str()
                        .expect("workdir description")
                        .contains("top-level workdir(path) tool")
                );
            }
            assert_eq!(
                properties["timeout"]["description"],
                serde_json::json!(
                    "Timeout in seconds. The command is killed if it exceeds this. Default: 300"
                )
            );
            assert!(matches!(
                register.tool.examples.as_slice(),
                [tau_proto::ToolExample {
                    arguments: CborValue::Map(arguments),
                    ..
                }]
                    if arguments.iter().any(|(key, value)| matches!(
                        (key, value),
                        (CborValue::Text(key), CborValue::Integer(value))
                            if key == "timeout" && *value == 300.into()
                    ))
            ));
            assert_eq!(parameters["required"], serde_json::json!(["command"]));
            found_shell |= register.tool.name == SHELL_TOOL_NAME;
            found_gpt_shell |= register.tool.name == GPT_SHELL_TOOL_NAME;
        }
    }
    assert!(found_shell, "expected shell tool registration");
    assert!(found_gpt_shell, "expected gpt_shell tool registration");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// The persistent workdir tool must expose one optional path, so an empty
/// object remains the provider-visible read operation.
#[test]
fn workdir_schema_keeps_path_optional() {
    let tool = registered_tool_specs(false)
        .into_iter()
        .find(|tool| tool.name.as_str() == WORKDIR_TOOL_NAME)
        .expect("workdir tool");
    assert_eq!(
        tool.description.as_deref(),
        Some(
            "Read or change your durable workdir. Omit `path` to read the current path and availability. A provided path is resolved from the last committed workdir, validated, canonicalized, and persisted. Do not combine a workdir change with shell or filesystem calls that rely on the new directory."
        )
    );
    let parameters = tool.parameters.expect("workdir parameters");
    assert!(parameters.get("required").is_none());
    assert_eq!(parameters["properties"]["path"]["type"], "string");
    assert_eq!(parameters["properties"]["path"]["minLength"], 1);
}

/// The shell-owned fragment must teach per-instance workdir use without
/// claiming a process-global cwd or an always-enabled directory wrapper.
#[test]
fn startup_registers_shell_workdir_prompt_fragment() {
    // The cwd prompt prose is owned by the shell extension rather than repeated
    // on every prefixed tool registration.
    let (mut reader, mut writer) = spawn_extension();

    let mut found_context_provider = false;
    let mut found_fragment = false;
    let mut saw_tool_fragment = false;
    for _ in 0..16 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        match event {
            Event::ToolRegistrationDeclared(register) => {
                saw_tool_fragment |= register.prompt_fragment.is_some();
            }
            Event::ExtensionContextProviderRegister(_) => {
                found_context_provider = true;
            }
            Event::ExtPromptFragmentPublish(publish) => {
                assert_eq!(publish.fragment.name, "shell.workdir");
                assert_eq!(
                    publish.fragment.priority,
                    tau_proto::PromptPriority::new(900)
                );
                assert!(
                    publish
                        .fragment
                        .template
                        .as_str()
                        .contains("agent_context.workdir")
                );
                let template = publish.fragment.template.as_str();
                assert!(template.contains("no global shell cwd"));
                assert!(template.contains("project root"));
                assert!(template.contains("direnv exec ."));
                assert!(template.contains("later tool turn"));
                assert!(template.contains("sibling calls have no workdir-first ordering"));
                assert!(template.contains("{{value.label}}_workdir"));
                assert!(
                    !template.contains("### Shell command allowlist"),
                    "disabled enforcement must leave the established fragment unchanged"
                );
                found_fragment = true;
            }
            _ => {}
        }
    }
    assert!(
        found_context_provider,
        "shell cwd context must gate first prompt dispatch"
    );
    assert!(found_fragment, "expected shell cwd prompt fragment publish");
    assert!(!saw_tool_fragment, "cwd must not be attached to any tool");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures lock-enabled replace admission preserves strict validation and its
/// compact error contract instead of leaking a rewritten absolute path.
#[test]
fn locked_replace_rejects_malformed_request_without_path_disclosure() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("secret.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);
    writer
        .write_event(&tool_started(
            "replace-malformed",
            REPLACE_TOOL_NAME,
            cbor_map(vec![
                ("path", CborValue::Text(path.display().to_string())),
                ("legacy", CborValue::Text("forbidden".to_owned())),
            ]),
            "agent-replace",
        ))
        .expect("replace");
    writer.flush().expect("flush replace");

    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::ToolError(error) if error.call_id.as_str() == "replace-malformed" => {
                assert_eq!(error.message, "request contains an unknown field");
                assert!(!error.message.contains(path.to_str().expect("UTF-8 path")));
                break;
            }
            Event::ToolResult(result) if result.call_id.as_str() == "replace-malformed" => {
                panic!("malformed replace unexpectedly succeeded: {result:?}");
            }
            _ => {}
        }
    }
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}
