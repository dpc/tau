//! Tests for process lifecycle behavior.

use super::*;

#[test]
fn shell_hidden_read_write_mode_is_unrestricted_by_default() {
    // With directory locking disabled, shell calls run read-write and publish
    // no access-mode chip.
    let td = TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf ok > probe".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(td.path().to_string_lossy().into_owned()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(
        fs::read_to_string(td.path().join("probe")).expect("probe"),
        "ok"
    );
}

/// Ensures model shell calls and user `!` / `!!` children receive the same
/// protected pager overlay after hostile ordinary configuration, without
/// replacing the configured TERM value.
#[test]
fn model_and_user_shells_share_protected_pager_environment() {
    let shell_config: crate::config::ShellConfig = serde_json::from_value(serde_json::json!({
        "extra_env": {
            "PAGER": "hostile-pager",
            "GIT_PAGER": "hostile-git-pager",
            "GH_PAGER": "hostile-gh-pager",
            "JJ_PAGER": "hostile-jj-pager",
            "SYSTEMD_PAGER": "hostile-systemd-pager",
            "TERM": "tau-test-term"
        }
    }))
    .expect("parse shell config");
    let command = "printf '%s|%s|%s|%s|%s|%s' \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \
         \"$JJ_PAGER\" \"$SYSTEMD_PAGER\" \"$TERM\"";
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command.to_owned()),
    )]);

    let CommandOutcome::Finished(model_output) = run_command_live(
        &args,
        &shell_config,
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run model shell environment probe") else {
        panic!("expected finished model shell outcome");
    };
    assert_eq!(
        cbor_map_text(&model_output.result, "output"),
        Some("out(no_nl) cat|cat|cat|cat|cat|tau-test-term")
    );

    let (tx, rx) = path_std_sync::mpsc::channel();
    let user_command = tau_proto::UiShellCommand {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-pager-env")
            .expect("test identifier must satisfy its grammar"),
        command: command.to_owned(),
        include_in_context: true,
        target_agent_id: None,
    };
    let output = Output::channel(tx);
    let (_cancel_tx, cancel_rx) = path_std_sync::mpsc::channel();
    path_crate_tools::shell::dispatch_user_shell_command(
        user_command,
        shell_config,
        &output,
        cancel_rx,
        std::env::current_dir().expect("current dir"),
    );
    let finished = rx
        .try_iter()
        .find_map(|message| match message {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ShellCommandFinishedReported(event) => Some(event),
                _ => None,
            },
            _ => None,
        })
        .expect("user shell finished event");
    assert_eq!(finished.output, "cat|cat|cat|cat|cat|tau-test-term");
    assert_eq!(finished.exit_code, Some(0));
}

/// Ensures the user-shell spawn boundary retains an admission-time non-UTF-8
/// cwd as an OS path instead of round-tripping it through lossy display text.
#[cfg(unix)]
#[test]
fn user_shell_preserves_non_utf8_operational_cwd() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let root = TempDir::new().expect("tempdir");
    let cwd = root
        .path()
        .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
    fs::create_dir(&cwd).expect("create non-UTF-8 cwd");
    fs::write(cwd.join("sentinel"), b"present").expect("write sentinel");

    let (tx, rx) = path_std_sync::mpsc::channel();
    let command = tau_proto::UiShellCommand {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-non-utf8-cwd")
            .expect("test identifier must satisfy its grammar"),
        command: "test -f ./sentinel".to_owned(),
        include_in_context: true,
        target_agent_id: None,
    };
    let output = Output::channel(tx);
    let (_cancel_tx, cancel_rx) = path_std_sync::mpsc::channel();

    path_crate_tools::shell::dispatch_user_shell_command(
        command,
        path_crate_config::ShellConfig::default(),
        &output,
        cancel_rx,
        cwd,
    );

    let finished = rx
        .try_iter()
        .find_map(|message| match message {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ShellCommandFinishedReported(event) => Some(event),
                _ => None,
            },
            _ => None,
        })
        .expect("user shell finished event");
    assert_eq!(finished.output, "");
    assert_eq!(finished.exit_code, Some(0));
    assert!(!finished.cancelled);
}

#[test]
fn shell_tool_replaces_invalid_utf8_stderr_and_marks_output_invalid() {
    // Regression coverage for agent-facing shell output collection: stderr
    // must be decoded lossily too, with a warning that does not erase the
    // original stderr text.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\376stderr' >&2".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("err(invalid-utf8,no_nl) �stderr")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[test]
fn shell_tool_replaces_invalid_utf8_both_streams_in_combined_output() {
    // Regression coverage for commands that write invalid bytes to both
    // streams: the agent should see both decoded streams and one concise
    // warning.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\377stdout'; printf '\\376stderr' >&2".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(invalid-utf8,no_nl) �stdout\nerr(invalid-utf8,no_nl) �stderr")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[cfg(target_os = "linux")]
#[test]
fn read_only_mount_setattr_flags_are_recursive() {
    // Enforced read-only mode clones the cwd tree recursively, so the final
    // mount_setattr call must also apply recursively; otherwise nested mounts
    // could remain writable under a supposedly read-only cwd subtree.
    let flags = crate::isolation::read_only_mount_setattr_flags();
    assert_ne!(flags & (libc::AT_EMPTY_PATH as libc::c_uint), 0);
    assert_ne!(flags & (libc::AT_RECURSIVE as libc::c_uint), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn shell_tool_enforced_read_only_mode_bind_mounts_cwd_read_only() {
    // Regression coverage for enforced inferred read-only shell mode: lock
    // elision is not enough. When `enforce_ro_bind` is true, the child must get
    // a read-only bind mount over its cwd so accidental writes fail before
    // they can alter the working tree.
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("input.txt"), "ok").expect("write fixture");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(dir.path().display().to_string()),
        ),
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("cat input.txt; touch created.txt".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let mut world = path_crate_tools_world::ShellWorld::real();
    let output = match path_crate_tools::shell::run_command_cancellable(
        "enforced_ro_test",
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::visible(
            path_crate_tools_shell::ShellAccessMode::ReadOnly,
        ),
        true,
        None,
        &mut world,
    ) {
        Ok(path_crate_tools_shell::CommandOutcome::Finished(output)) => *output,
        Ok(path_crate_tools_shell::CommandOutcome::Cancelled) => panic!("unexpected cancellation"),
        Err(error) if error.message.contains("Operation not permitted") => return,
        Err(error) => panic!("unexpected shell start error: {error:?}"),
    };

    assert_ne!(cbor_int_field(&output.result, "status"), Some(0));
    assert!(
        !dir.path().join("created.txt").exists(),
        "read-only shell command must not create files in cwd"
    );
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(
        combined.contains(" ok"),
        "read should still work: {combined}"
    );
    assert!(
        combined.contains("Read-only file system") || combined.contains("Permission denied"),
        "write should fail due to mount permissions: {combined}"
    );
}

#[test]
fn command_details_value_records_combined_output_stats() {
    let details = command_details_value(CommandDetails {
        status: Some(0),
        signal: None,
        timed_out: false,
        duration_seconds: None,
        termination_reason: "exit",
        total_lines: None,
        total_bytes: None,
        output: "out hi\nerr oops".to_owned(),
        truncated: false,
        valid_utf8: true,
        saved_output: None,
    });
    assert_eq!(cbor_map_text(&details, "output"), Some("out hi\nerr oops"));
    assert!(cbor_map_field(&details, "total_lines").is_none());
    assert!(cbor_map_field(&details, "total_bytes").is_none());
    assert!(cbor_map_field(&details, "valid_utf8").is_none());
    assert!(cbor_map_field(&details, "timed_out").is_none());
    assert!(cbor_map_field(&details, "termination_reason").is_none());
    assert!(cbor_map_field(&details, "truncated").is_none());
    assert!(cbor_map_field(&details, "duration_seconds").is_none());
}

#[test]
fn command_details_value_records_slow_command_exec_time() {
    let details = command_details_value(CommandDetails {
        status: Some(0),
        signal: None,
        timed_out: false,
        duration_seconds: Some(6),
        termination_reason: "exit",
        total_lines: None,
        total_bytes: None,
        output: String::new(),
        truncated: false,
        valid_utf8: true,
        saved_output: None,
    });

    assert_eq!(cbor_int_field(&details, "duration_seconds"), Some(6));
}

/// Ensures closed stdin presents persistent poll-visible readiness rather than
/// leaving event-driven consumers waiting for input forever.
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
#[test]
fn shell_tool_closed_stdin_provides_poll_visible_readiness() {
    let helper = std::env::current_exe()
        .expect("current test executable")
        .to_string_lossy()
        .replace('\'', "'\"'\"'");
    let command = format!(
        "'{helper}' --ignored --exact tests::process_lifecycle::shell_tool_poll_stdin_helper \
         >/dev/null 2>&1 && printf 'after poll\\n'"
    );
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run stdin hangup probe") else {
        panic!("expected finished shell outcome");
    };

    assert_eq!(cbor_int_field(&output.result, "status"), Some(0));
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out after poll")
    );
    assert!(cbor_bool_field(&output.result, "timed_out").is_none());
}

/// Ensures both model shell dialects enforce command-regex rules before spawn
/// and disclose each typed command/workdir pair in the agent-facing error.
#[test]
fn model_shell_surfaces_enforce_allowlist_before_spawn() {
    let workdir = TempDir::new().expect("workdir");
    let sentinel = workdir.path().join("must-not-exist");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_shell_regex_allowlist_config(
        &mut writer,
        vec![(
            workdir.path().to_str().expect("UTF-8 workdir"),
            r"printf allowed",
            Some("Use the permitted printf form."),
        )],
    );
    let Event::ExtPromptFragmentPublish(_) = reader
        .read_event()
        .expect("read configured prompt fragment")
        .expect("configured prompt fragment")
    else {
        panic!("expected configured prompt fragment publication");
    };

    for (call_id, tool_name, directory_field) in [
        ("deny-generic", SHELL_TOOL_NAME, "cwd"),
        ("deny-chatgpt", GPT_SHELL_TOOL_NAME, "workdir"),
    ] {
        writer
            .write_event(&Event::ToolStarted(ToolStarted {
                invocation_policy: tau_proto::ToolInvocationPolicy::default(),
                call_id: call_id.into(),
                tool_name: tau_proto::ToolName::new(tool_name),
                arguments: cbor_map(vec![
                    (
                        "command",
                        CborValue::Text(format!("touch {}", sentinel.display())),
                    ),
                    (
                        directory_field,
                        CborValue::Text(workdir.path().display().to_string()),
                    ),
                ]),
                agent_id: tau_proto::AgentId::parse("allowlist-agent").expect("agent id"),
                originator: tau_proto::PromptOriginator::User,
            }))
            .expect("invoke denied shell");
        writer.flush().expect("flush denied shell");

        let Event::ToolError(error) = reader.read_event().expect("read").expect("error") else {
            panic!("expected tool error");
        };
        assert!(error.message.contains("denied by configured allowlist"));
        assert!(error.message.contains(r#"command_regex: "printf allowed""#));
        assert!(
            error
                .message
                .contains(r#"description: "Use the permitted printf form.""#)
        );
        assert!(error.message.contains("workdir:"));
        assert!(
            !error.message.contains(&sentinel.display().to_string()),
            "generated model denial must not echo the denied command"
        );
        assert!(!sentinel.exists(), "denied command must not spawn");
    }
}

/// Ensures both `!` and `!!` execution semantics use the same command-regex
/// allowlist and report one terminal denial without spawning a child.
#[test]
fn user_shell_context_modes_enforce_the_same_allowlist() {
    let workdir = TempDir::new().expect("workdir");
    let agent_id = tau_proto::AgentId::parse("allowlist-user-agent").expect("agent id");
    let sentinel = workdir.path().join("must-not-exist");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_shell_regex_allowlist_config(
        &mut writer,
        vec![(
            workdir.path().to_str().expect("UTF-8 workdir"),
            r"printf allowed",
            Some("Use the permitted printf form."),
        )],
    );
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

    for (command_id, include_in_context) in [("deny-bang", true), ("deny-double-bang", false)] {
        writer
            .write_event(&Event::UiShellCommand(tau_proto::UiShellCommand {
                session_id: "session-1".parse().expect("session id"),
                command_id: test_shell_command_id(command_id),
                command: format!("touch {}", sentinel.display()),
                include_in_context,
                target_agent_id: Some(agent_id.clone()),
            }))
            .expect("send user shell");
        writer.flush().expect("flush user shell");
        let finished = wait_for_user_shell_finished(&mut reader, command_id);
        assert_eq!(finished.include_in_context, include_in_context);
        assert!(finished.output.contains("denied by configured allowlist"));
        assert!(
            finished
                .output
                .contains(r#"command_regex: "printf allowed""#)
        );
        assert!(
            finished
                .output
                .contains(r#"description: "Use the permitted printf form.""#)
        );
        assert!(
            !finished.output.contains(&sentinel.display().to_string()),
            "generated user-shell denial must not echo the denied command"
        );
        assert_eq!(finished.exit_code, None);
        assert!(!sentinel.exists(), "denied user command must not spawn");
    }
}

/// Ensures denial happens before RecordIfMissing can create an empty cassette
/// and before ReplayOnly can report missing or malformed VCR state.
#[test]
fn shell_allowlist_denial_never_opens_vcr_state() {
    let workdir = TempDir::new().expect("workdir");
    let config: path_crate_config::ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": workdir.path().display().to_string(),
            "command_regex": "printf allowed"
        }]
    }))
    .expect("config");
    let invoke = || {
        let mut invoke = match tool_started(
            "deny-before-vcr",
            SHELL_TOOL_NAME,
            cbor_map(vec![
                ("command", CborValue::Text("printf denied".to_owned())),
                ("cwd", CborValue::Text(workdir.path().display().to_string())),
            ]),
            "vcr-policy-agent",
        ) {
            Event::ToolStarted(invoke) => invoke,
            _ => unreachable!("helper always returns ToolStarted"),
        };
        invoke.tool_name = tau_proto::ToolName::new(SHELL_TOOL_NAME);
        invoke
    };

    let record_dir = TempDir::new().expect("record dir");
    let error = world_after_shell_authorization(
        &mut invoke(),
        &config,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            record_dir.path(),
        )),
        workdir.path().to_path_buf(),
    )
    .err()
    .expect("policy denial precedes recording");
    assert!(error.message.contains("denied by configured allowlist"));
    assert!(
        std::fs::read_dir(record_dir.path())
            .expect("read record dir")
            .next()
            .is_none(),
        "denial must not create a cassette"
    );

    let invalid_record_dir = TempDir::new().expect("invalid record dir");
    let mut invalid = match tool_started(
        "invalid-before-vcr",
        GPT_SHELL_TOOL_NAME,
        cbor_map(vec![
            ("command", CborValue::Text("printf allowed".to_owned())),
            (
                "workdir",
                CborValue::Text(workdir.path().display().to_string()),
            ),
            ("cwd", CborValue::Text(workdir.path().display().to_string())),
        ]),
        "vcr-policy-agent",
    ) {
        Event::ToolStarted(invoke) => invoke,
        _ => unreachable!("helper always returns ToolStarted"),
    };
    let error = world_after_shell_authorization(
        &mut invalid,
        &config,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            invalid_record_dir.path(),
        )),
        workdir.path().to_path_buf(),
    )
    .err()
    .expect("surface validation precedes recording");
    assert!(error.message.contains("`cwd` is not supported"));
    assert!(
        std::fs::read_dir(invalid_record_dir.path())
            .expect("read invalid record dir")
            .next()
            .is_none(),
        "invalid invocation must not create a cassette"
    );

    for malformed in [false, true] {
        let replay_dir = TempDir::new().expect("replay dir");
        if malformed {
            std::fs::write(
                replay_dir.path().join("deny-before-vcr.yaml"),
                "not: [valid",
            )
            .expect("write malformed cassette");
        }
        let error = world_after_shell_authorization(
            &mut invoke(),
            &config,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                replay_dir.path(),
            )),
            workdir.path().to_path_buf(),
        )
        .err()
        .expect("policy denial precedes replay state");
        assert!(error.message.contains("denied by configured allowlist"));
        assert!(!error.message.contains("vcr"));
    }
}
/// Ensures the default overlay bypasses a deterministic hostile pager, while
/// the explicit opt-out exposes that pager and lets the normal timeout bound
/// it.
#[cfg(unix)]
#[test]
fn protected_pager_overlay_prevents_stall_and_opt_out_forfeits_guarantee() {
    use std::os::unix::fs::PermissionsExt as _;

    let tempdir = TempDir::new().expect("tempdir");
    let pager = tempdir.path().join("hostile-pager");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/hostile-pager.sh"
        ),
        &pager,
    )
    .expect("copy hostile pager fixture");
    fs::set_permissions(&pager, fs::Permissions::from_mode(0o700))
        .expect("make hostile pager executable");
    let pager = pager.to_string_lossy();
    let command = "printf 'pager payload\\n' | \"$PAGER\"";
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command.to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);
    let protected: crate::config::ShellConfig = serde_json::from_value(serde_json::json!({
        "extra_env": { "PAGER": pager.as_ref() }
    }))
    .expect("parse protected config");

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &protected,
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run protected pager probe") else {
        panic!("expected finished protected shell outcome");
    };
    assert_eq!(cbor_int_field(&output.result, "status"), Some(0));
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out pager payload")
    );
    assert!(cbor_bool_field(&output.result, "timed_out").is_none());

    let unprotected: crate::config::ShellConfig = serde_json::from_value(serde_json::json!({
        "non_interactive_pager": false,
        "extra_env": { "PAGER": pager.as_ref() }
    }))
    .expect("parse opt-out config");
    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &unprotected,
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run opt-out pager probe") else {
        panic!("expected finished opt-out shell outcome");
    };
    assert_eq!(cbor_bool_field(&output.result, "timed_out"), Some(true));
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("timeout")
    );
}

#[test]
fn shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    let Event::ToolProgressReported(progress) = progress else {
        panic!("expected tool progress");
    };
    assert_eq!(progress.display.expect("display").mode, "");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, SHELL_TOOL_NAME);
    assert_eq!(result.display.as_ref().expect("display").mode, "");
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) hello".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn gpt_shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-gpt-shell".into(),
            tool_name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    let Event::ToolProgressReported(progress) = progress else {
        panic!("expected tool progress");
    };
    assert_eq!(progress.tool_name, GPT_SHELL_TOOL_NAME);
    assert_eq!(progress.display.expect("display").mode, "");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, GPT_SHELL_TOOL_NAME);
    assert_eq!(result.display.as_ref().expect("display").mode, "");
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) hello".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn command_isolation_preserves_explicit_environment() {
    let mut cmd = path_std_process::Command::new("sh");
    cmd.arg("-c")
        .arg("printf %s \"${TAU_EXPLICIT_ENV_TEST-unset}\"")
        .env("TAU_EXPLICIT_ENV_TEST", "ok")
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::piped());
    crate::isolation::apply_command_isolation(&mut cmd);
    let output = cmd.output().expect("run env probe");
    assert!(output.status.success(), "env probe failed: {output:?}");
    assert_eq!(String::from_utf8(output.stdout).expect("utf8 stdout"), "ok");
}

#[test]
fn command_isolation_clears_cargo_build_environment() {
    let cargo_env_vars = [
        "CARGO",
        "CARGO_BIN_NAME",
        "CARGO_CRATE_NAME",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_LINKS",
        "CARGO_MANIFEST_PATH",
        "CARGO_PKG_AUTHORS",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_HOMEPAGE",
        "CARGO_PKG_LICENSE",
        "CARGO_PKG_LICENSE_FILE",
        "CARGO_PKG_NAME",
        "CARGO_PKG_README",
        "CARGO_PKG_REPOSITORY",
        "CARGO_PKG_RUST_VERSION",
        "CARGO_PKG_VERSION",
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
        "CARGO_PKG_VERSION_PRE",
        "CARGO_PRIMARY_PACKAGE",
        "OUT_DIR",
    ];
    let script = cargo_env_vars
        .iter()
        .map(|env_var| format!("printf '%s=%s\\n' {env_var} \"${{{env_var}-unset}}\""))
        .collect::<Vec<_>>()
        .join("; ");
    let expected = cargo_env_vars
        .iter()
        .map(|env_var| format!("{env_var}=unset\n"))
        .collect::<String>();

    let mut cmd = path_std_process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::piped());
    for env_var in cargo_env_vars {
        cmd.env(env_var, "should-not-leak");
    }
    crate::isolation::apply_command_isolation(&mut cmd);
    let output = cmd.output().expect("run env probe");
    assert!(output.status.success(), "env probe failed: {output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        expected
    );
}

#[test]
fn shell_working_directory_cannot_be_set_after_runtime_events() {
    // A late None -> Some transition would mutate process-global cwd while
    // workers may already be resolving relative paths, so it must be rejected.
    let current = ExtConfig::default();
    let next = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/late")),
        ..Default::default()
    };

    let err = apply_working_directory(&current, &next, true).expect_err("late cwd set rejected");

    assert!(err.contains("cannot be set after runtime events"));
}

#[test]
fn shell_tool_multiline_display_uses_short_args_and_text_payload() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf hello\nprintf world".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(output.display.args, "printf hello");
    assert_eq!(
        output.display.payload,
        Some(tau_proto::ToolUsePayload::Text {
            text: "printf hello\nprintf world".to_owned(),
        })
    );
}

/// A one-line shell command shortened in the display header remains available
/// as a full Unicode text payload in both progress and terminal descriptors so
/// `show-tools=full` can render the omitted middle.
#[test]
fn shell_tool_long_display_args_include_full_text_payload() {
    let command = "αβγδεζηθικλμνξοπρστυφχψω一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command.to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);
    let expected_args = "αβγδεζηθικλμνξοπρστυ┄一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let expected_payload = Some(tau_proto::ToolUsePayload::Text {
        text: command.to_owned(),
    });
    let initial = path_crate_tools::shell::initial_display(
        &args,
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
    );
    assert_eq!(initial.args, expected_args);
    assert_eq!(initial.payload, expected_payload);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(output.display.args, expected_args);
    assert_eq!(output.display.payload, expected_payload);
}

/// A one-line shell command that fits the display header does not gain a
/// redundant payload body.
#[test]
fn shell_tool_short_display_args_omit_redundant_text_payload() {
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf hello".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.args, "printf hello");
    assert_eq!(output.display.payload, None);
}

#[test]
fn shell_tool_use_state_mode_can_show_inferred_access_mode() {
    // When directory locking is enabled, the CLI can render ext-shell's
    // inferred shell access mode separately from display args.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf hello".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::visible(
            path_crate_tools_shell::ShellAccessMode::ReadOnly,
        ),
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "ro");
    assert_eq!(output.display.args, "printf hello");
}

/// Ensures PTY-supported shell commands see TTY-backed output descriptors while
/// stdin stays closed and the result still distinguishes stdout from stderr.
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
#[test]
fn shell_tool_runs_with_tty_outputs_closed_stdin_and_separate_streams() {
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(
            "[ ! -t 0 ] && [ -t 1 ] && [ -t 2 ] \
             && printf 'tty stdout\\n' \
             && printf 'tty stderr\\n' >&2"
                .to_owned(),
        ),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run TTY probe") else {
        panic!("expected finished shell outcome");
    };

    assert_eq!(cbor_int_field(&output.result, "status"), Some(0));
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out tty stdout\nerr tty stderr")
    );
}

/// Ensures the documented bare-`cat` dependency fails as an ordinary missing
/// command when the configured child PATH contains no `cat`.
#[cfg(unix)]
#[test]
fn protected_pager_reports_command_not_found_when_cat_is_absent() {
    let shell = path_std_process::Command::new("sh")
        .arg("-c")
        .arg("command -v sh")
        .output()
        .expect("locate shell");
    assert!(shell.status.success());
    let shell = String::from_utf8(shell.stdout)
        .expect("shell path is utf8")
        .trim()
        .to_owned();
    let empty_path = TempDir::new().expect("empty PATH directory");
    let shell_config: crate::config::ShellConfig = serde_json::from_value(serde_json::json!({
        "command": shell,
        "extra_env": { "PATH": empty_path.path() }
    }))
    .expect("parse missing-cat config");
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf payload | \"$PAGER\"".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &shell_config,
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run missing-cat probe") else {
        panic!("expected finished missing-cat shell outcome");
    };
    assert_eq!(cbor_int_field(&output.result, "status"), Some(127));
    assert!(
        cbor_map_text(&output.result, "output")
            .is_some_and(|output| output.contains("cat") && output.contains("not found"))
    );
}

/// Repository-owned subprocess helper that asserts closed stdin is immediately
/// poll-ready without depending on an external interpreter.
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
#[test]
#[ignore = "invoked explicitly by shell_tool_closed_stdin_provides_poll_visible_readiness"]
fn shell_tool_poll_stdin_helper() {
    let mut poll_fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    // SAFETY: `poll_fd` points to one initialized `pollfd`, fd 0 is borrowed
    // rather than owned, and a zero timeout cannot block.
    #[allow(unsafe_code)]
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    assert_eq!(ready, 1);
}

#[test]
fn shell_tool_marks_invalid_utf8_stdout_line_and_marks_output_invalid() {
    // Regression coverage for agent-facing shell output collection: stdout
    // can contain arbitrary bytes, and read_to_string used to drop all output
    // after the first invalid UTF-8 sequence.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\377stdout'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(invalid-utf8,no_nl) �stdout")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[test]
fn shell_tool_marks_crlf_and_cr_line_endings() {
    // Keep shell output line markers aligned with `read`: raw carriage
    // returns should not leak into agent-visible output.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf 'a\r\nb\rc\n'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(crlf) a\nout(cr) b\nout c")
    );
}

#[test]
fn shell_tool_omits_truncation_marker_without_truncation() {
    // Compatibility metadata should stay sparse: total/truncated fields are
    // only present when a stream was actually truncated.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf 'ok\\n'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(cbor_map_text(&output.result, "output"), Some("out ok"));
    let field = "truncated";
    assert!(
        cbor_map_field(&output.result, field).is_none(),
        "{field} should be absent without truncation"
    );
}

/// Ensures model shell accepts output exactly at its 15 KiB byte boundary.
#[test]
fn shell_tool_accepts_exact_model_output_byte_cap() {
    const EXPECTED_MODEL_SHELL_OUTPUT_BYTES: usize = 15 * 1024;
    assert_eq!(
        crate::tools::shell::MAX_MODEL_SHELL_OUTPUT_BYTES,
        EXPECTED_MODEL_SHELL_OUTPUT_BYTES
    );
    let emoji = "🙂";
    let emoji_count = (EXPECTED_MODEL_SHELL_OUTPUT_BYTES - "out ".len()) / emoji.len();
    let expected = format!("out {}", emoji.repeat(emoji_count));
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(format!(
            "i=0; while [ \"$i\" -lt {emoji_count} ]; do printf '\\360\\237\\231\\202'; i=$((i + 1)); done; printf '\\n'"
        )),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert_eq!(combined, expected);
    assert_eq!(combined.len(), EXPECTED_MODEL_SHELL_OUTPUT_BYTES);
    assert!(cbor_map_field(&output.result, "truncated").is_none());
    assert!(cbor_map_field(&output.result, "full_output_path").is_none());
}

/// Ensures byte-cap truncation preserves UTF-8 boundaries and saves the
/// complete rendering.
#[test]
fn shell_tool_multibyte_output_over_model_cap_keeps_exact_artifact() {
    const EXPECTED_MODEL_SHELL_OUTPUT_BYTES: usize = 15 * 1024;
    assert_eq!(
        crate::tools::shell::MAX_MODEL_SHELL_OUTPUT_BYTES,
        EXPECTED_MODEL_SHELL_OUTPUT_BYTES
    );
    let emoji = "🙂";
    const SECOND_LINE_EMOJI_COUNT: usize = 3;
    const ONE_BYTE_OVER_CAP: usize = 1;
    let output_prefix = "out ";
    let second_line = format!("{output_prefix}{}", emoji.repeat(SECOND_LINE_EMOJI_COUNT));
    let first_line_emoji_count = (EXPECTED_MODEL_SHELL_OUTPUT_BYTES + ONE_BYTE_OVER_CAP
        - "\n".len()
        - second_line.len()
        - output_prefix.len())
        / emoji.len();
    let first_line = format!("{output_prefix}{}", emoji.repeat(first_line_emoji_count));
    let expected = format!("{first_line}\n{second_line}");
    assert_eq!(
        expected.len(),
        EXPECTED_MODEL_SHELL_OUTPUT_BYTES + ONE_BYTE_OVER_CAP
    );
    let command = format!(
        "i=0; while [ \"$i\" -lt {first_line_emoji_count} ]; do printf '\\360\\237\\231\\202'; i=$((i + 1)); done; printf '\\n\\360\\237\\231\\202\\360\\237\\231\\202\\360\\237\\231\\202\\n'"
    );
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert_eq!(combined, format!("{first_line}\nout(truncated)"));
    assert_eq!(combined.len(), EXPECTED_MODEL_SHELL_OUTPUT_BYTES - 1);
    assert!(!combined.contains('\u{FFFD}'));
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
    assert_eq!(
        cbor_int_field(&output.result, "total_bytes"),
        Some(expected.len() as i128)
    );
    let saved = std::fs::read_to_string(
        cbor_map_text(&output.result, "full_output_path").expect("full output path"),
    )
    .expect("read complete saved output");
    assert_eq!(saved, expected);
}

/// Ensures shell truncation publishes complete totals and a readable,
/// privately contained complete saved rendering.
#[test]
fn shell_tool_reports_truncation_marker_and_original_totals() {
    let line_count = MAX_OUTPUT_LINES + 1;
    let command = format!(
        "i=0; while [ \"$i\" -lt {line_count} ]; do printf 'x\\n'; printf 'e\\n' >&2; i=$((i + 1)); done"
    );
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(combined.starts_with("out x") || combined.starts_with("err e"));
    assert!(combined.contains("\n...\n"));
    assert!(combined.contains("\nout x") || combined.contains("\nerr e"));
    assert_eq!(
        cbor_int_field(&output.result, "total_lines"),
        Some((line_count * 2) as i128)
    );
    assert!(cbor_int_field(&output.result, "total_bytes").is_some());
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
    assert!(cbor_map_text(&output.result, "truncation_warning").is_some());
    let path = path_std_path::PathBuf::from(
        cbor_map_text(&output.result, "full_output_path").expect("full output path"),
    );
    let saved =
        std::fs::read_to_string(&path).expect("saved output remains readable by exact path");
    assert!(saved.len() > combined.len());
    assert!(saved.starts_with("out x") || saved.starts_with("err e"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory_mode = path
            .parent()
            .expect("parent")
            .metadata()
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = path.metadata().expect("file metadata").permissions().mode() & 0o777;
        assert_eq!(directory_mode, 0o300);
        assert_eq!(file_mode, 0o600);
    }
}

#[test]
fn shell_tool_marks_invalid_utf8_and_truncation_together() {
    // When multiple shell-side warnings apply, keep them outside the stream
    // marker and in a deterministic order before stderr content.
    let line_count = MAX_OUTPUT_LINES + 1;
    let command = format!(
        "printf '\\377'; i=0; while [ \"$i\" -lt {line_count} ]; do printf 'x\\n'; i=$((i + 1)); done"
    );
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
}

#[test]
fn shell_tool_runs_in_requested_cwd() {
    // Regression coverage for the schema-exposed cwd argument: the execution
    // path already supports it, and the shell must actually start there.
    let tempdir = TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("pwd".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let cwd = tempdir.path().canonicalize().expect("canonical cwd");
    let expected_stdout = format!("out {}", cwd.display());
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some(expected_stdout.as_str())
    );
    assert!(cbor_map_text(&output.result, "cwd").is_none());
}

#[test]
fn shell_tool_timeout_preserves_partial_output() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf 'before\\n'; sleep 2; printf 'after\\n'".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "timeout");
    assert_eq!(cbor_map_text(&output.result, "output"), Some("out before"));
    assert!(cbor_map_field(&output.result, "total_lines").is_none());
    assert_eq!(cbor_bool_field(&output.result, "timed_out"), Some(true));
    assert!(cbor_int_field(&output.result, "timeout_secs").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("timeout")
    );
}

/// Ensures a foreground model shell result does not wait for EOF from a
/// background descendant that retains an output endpoint.
#[cfg(unix)]
#[test]
fn shell_tool_returns_after_foreground_exit_even_if_background_holds_output_endpoint() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("(sleep 5; printf late) & printf early".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let started = path_std_time::Instant::now();
    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "background output holder delayed shell result for {elapsed:?}"
    );
    let output = cbor_map_text(&output.result, "output").expect("output");
    assert_eq!(output, "out(no_nl) early");
    assert!(!output.contains("late"));
}

/// Ensures user `!` completion does not wait for stream EOF or capture late
/// output from a detached descendant that inherits stdout.
#[cfg(unix)]
#[test]
fn user_shell_returns_after_foreground_exit_even_if_background_holds_output_endpoint() {
    if !path_std_process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let (tx, rx) = path_std_sync::mpsc::channel();
    let cmd = tau_proto::UiShellCommand {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-bg")
            .expect("test identifier must satisfy its grammar"),
        command: "setsid sh -c 'sleep 5; printf late' & printf early".to_owned(),
        include_in_context: true,
        target_agent_id: None,
    };

    let started = path_std_time::Instant::now();
    let output = Output::channel(tx);
    let (_cancel_tx, cancel_rx) = path_std_sync::mpsc::channel();
    path_crate_tools::shell::dispatch_user_shell_command(
        cmd,
        path_crate_config::ShellConfig::default(),
        &output,
        cancel_rx,
        std::env::current_dir().expect("current dir"),
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "background output holder delayed user shell result for {elapsed:?}"
    );

    let mut finished = None;
    for message in rx.try_iter() {
        if let HarnessInputMessage::Emit(emit) = message
            && let Event::ShellCommandFinishedReported(event) = *emit.event
        {
            finished = Some(event);
        }
    }
    let finished = finished.expect("finished event");
    assert_eq!(finished.output, "early");
    assert!(!finished.output.contains("late"));
    assert_eq!(finished.exit_code, Some(0));
    assert!(!finished.cancelled);
}

/// Ensures timeout returns after killing the foreground process group even when
/// an escaped descendant retains an inherited stdout endpoint.
#[cfg(unix)]
#[test]
fn shell_tool_timeout_returns_without_waiting_for_escaped_output_holder() {
    if !path_std_process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(
                "setsid sh -c 'sleep 5; printf late' & printf early; sleep 5".to_owned(),
            ),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let started = path_std_time::Instant::now();
    let CommandOutcome::Finished(result) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let result = *result;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "escaped output holder delayed timeout result for {elapsed:?}"
    );
    assert_eq!(result.display.status, ToolUseStatus::Error);
    assert_eq!(result.display.status_text, "timeout");
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert_eq!(output, "out(no_nl) early");
    assert!(!output.contains("late"));
    assert_eq!(cbor_bool_field(&result.result, "timed_out"), Some(true));
    assert_eq!(
        cbor_map_text(&result.result, "termination_reason"),
        Some("timeout")
    );
}

#[test]
fn shell_tool_bounded_huge_output_reports_original_totals() {
    // The shell reader keeps only a bounded tail in memory while counting the
    // original stream, so huge stdout still reports total bytes and truncation.
    let byte_count = MAX_OUTPUT_BYTES * 4 + 123;
    let command = format!("yes x | head -c {byte_count}");
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(combined.contains("..."));
    assert!(combined.len() < byte_count);
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
    assert!(cbor_int_field(&output.result, "total_bytes").is_some());
}

#[test]
fn shell_tool_timeout_zero_is_immediate_timeout() {
    // A zero timeout is valid and means the child should be killed as soon as
    // timeout accounting observes that it has not already exited.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("sleep 1".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "timeout");
    assert_eq!(cbor_bool_field(&output.result, "timed_out"), Some(true));
    assert!(cbor_int_field(&output.result, "timeout_secs").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("timeout")
    );
}

#[test]
fn shell_tool_rejects_negative_timeout() {
    // Negative durations cannot be represented by the runner; reject them
    // explicitly instead of silently falling back to the default.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer((-1).into()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("timeout");
    assert_eq!(error.message, "argument `timeout` must be non-negative");
}
#[test]
fn shell_tool_rejects_wrong_type_timeout() {
    // The old lenient integer helper ignored wrong-type values, causing the
    // default timeout to be used without telling the agent its request was bad.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Text("1".to_owned()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("timeout");
    assert_eq!(error.message, "argument `timeout` must be an integer");
}

#[test]
fn shell_tool_rejects_wrong_type_cwd() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("cwd");
    assert_eq!(error.message, "argument `cwd` must be a string");
}

#[cfg(unix)]
#[test]
fn shell_tool_reports_signal_termination_details() {
    // Regression coverage for signal deaths: shells killed by a signal do not
    // have an exit code, but Unix exposes the terminating signal separately.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("kill -TERM $$".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &path_crate_config::ShellConfig::default(),
        path_crate_tools_shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("signal result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "signal 15");
    assert_eq!(cbor_int_field(&output.result, "signal"), Some(15));
    assert!(cbor_bool_field(&output.result, "timed_out").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("signal")
    );
    assert!(cbor_map_field(&output.result, "status").is_none());
}

#[test]
fn shell_tool_reports_failures_with_details() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("exit 7".to_owned()),
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
    assert_eq!(result.tool_name, SHELL_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "status"), Some(7));
    assert_eq!(
        cbor_map_text(&result.result, "termination_reason"),
        Some("exit")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Regression guard: a call-local shell cwd must never mutate durable workdir
/// metadata.
#[test]
fn explicit_shell_cwd_is_canonicalized_without_emitting_metadata() {
    let temp = TempDir::new().expect("tempdir");
    let original = TempDir::new().expect("original cwd");
    let agent_id = tau_proto::AgentId::parse("agent-cwd-explicit").expect("agent id");
    let cwd_state = CwdState::new();
    cwd_state.set(
        agent_id.clone(),
        original.path().canonicalize().expect("original"),
    );
    let Event::ToolStarted(invoke) = tool_started(
        "call-cwd",
        SHELL_TOOL_NAME,
        cbor_text_map(vec![
            ("command", "pwd"),
            ("cwd", &temp.path().display().to_string()),
        ]),
        agent_id.as_str(),
    ) else {
        unreachable!();
    };

    let base = cwd_state.get_or_default(&agent_id).expect("remembered cwd");
    let rewritten = rewrite_invoke_for_cwd(invoke, &base);
    let canonical = temp.path().canonicalize().expect("canonical cwd");
    assert_eq!(
        cwd_state.get_or_default(&agent_id).expect("remembered cwd"),
        original.path().canonicalize().expect("original")
    );
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "cwd").expect("cwd arg"),
        Some(canonical.display().to_string())
    );
}

/// Regression guard: generic `shell.cwd` and GPT `shell_command.workdir` remain
/// call-local across full dispatch, including relative GPT resolution.
#[test]
fn shell_surface_directory_overrides_remain_call_local_across_full_dispatch() {
    let remembered = TempDir::new().expect("remembered");
    let override_dir = TempDir::new().expect("override");
    let relative_override = remembered.path().join("relative-override");
    fs::create_dir(&relative_override).expect("relative override");
    fs::write(remembered.path().join("remembered.txt"), "from-a\n").expect("remembered file");
    let agent_id = tau_proto::AgentId::parse("agent-call-local-cwd").expect("agent id");
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

    for tool_name in [SHELL_TOOL_NAME, GPT_SHELL_TOOL_NAME] {
        let surface =
            path_crate_tools::ShellSurface::for_tool_name(tool_name).expect("known shell surface");
        let mut cases = vec![
            (
                "override",
                Some(override_dir.path().display().to_string()),
                override_dir.path(),
            ),
            ("remembered-after", None, remembered.path()),
        ];
        if surface == path_crate_tools::ShellSurface::ChatGpt {
            cases.push((
                "relative-override",
                Some("relative-override".to_owned()),
                relative_override.as_path(),
            ));
        }
        for (suffix, directory_argument, expected) in cases {
            let call_id = format!("call-{tool_name}-{suffix}");
            let mut args = vec![("command", "pwd")];
            if let Some(directory_argument) = directory_argument.as_deref() {
                args.push((surface.directory_argument(), directory_argument));
            }
            writer
                .write_event(&tool_started(
                    &call_id,
                    tool_name,
                    cbor_text_map(args),
                    agent_id.as_str(),
                ))
                .expect("shell call");
            writer.flush().expect("flush shell call");
            loop {
                match reader.read_event().expect("read").expect("event") {
                    Event::AgentMetadataSetRequest(metadata) => {
                        panic!("call-local shell directory emitted metadata: {metadata:?}")
                    }
                    Event::AgentUserMessageInjected(notice) => {
                        panic!("call-local shell directory emitted notice: {notice:?}")
                    }
                    Event::ToolResult(result) if result.call_id.as_str() == call_id => {
                        let rendered = format!("{:?}", result.result);
                        assert!(
                            rendered.contains(expected.to_str().expect("UTF-8 path")),
                            "shell result did not use expected call-local directory: {rendered}"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    writer
        .write_event(&tool_started(
            "call-read-remembered-after",
            READ_TOOL_NAME,
            cbor_text_map(vec![("path", "remembered.txt")]),
            agent_id.as_str(),
        ))
        .expect("read remembered file");
    writer.flush().expect("flush read");
    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSetRequest(metadata) => {
                panic!("call-local shell directory emitted metadata: {metadata:?}")
            }
            Event::ToolResult(result)
                if result.call_id.as_str() == "call-read-remembered-after" =>
            {
                assert!(format!("{:?}", result.result).contains("from-a"));
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn replayed_shell_tool_delivery_does_not_run_tool() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("replay-must-not-write.txt");
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);

    let Event::ToolStarted(invoke) = tool_started(
        "replayed-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&path, vec![context_half_open_edit(1, 1, "created\n", "")]),
        "agent-a",
    ) else {
        unreachable!();
    };
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::ToolStarted(invoke),
        ))
        .expect("replayed edit");
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");

    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension exit")
        .expect("extension ok");
    assert!(
        !path.exists(),
        "replayed tool delivery must not mutate files"
    );
}

#[test]
fn shell_tool_cancel_request_stops_running_command_quickly() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let call_id = tau_proto::ToolCallId::new("cancel-shell-call");
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: call_id.clone(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("sleep 30".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke shell");
    writer.flush().expect("flush invoke");

    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(2));
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress)) if progress.call_id == call_id => break,
            Some(_) => continue,
            None => panic!("extension closed before shell started"),
        }
    }

    writer
        .write_event(&Event::ToolCancelRequest(ToolCancelRequest {
            target_call_id: call_id.clone(),
        }))
        .expect("cancel shell");
    writer.flush().expect("flush cancel");

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "shell cancellation timed out");
        match reader.read_event().expect("read") {
            Some(Event::ToolCancelled(cancelled)) if cancelled.call_id == call_id => break,
            Some(_) => continue,
            None => panic!("extension closed before cancellation"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}
