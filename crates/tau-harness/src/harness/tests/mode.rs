use super::*;

fn wait_for_socket(sock: &Path) {
    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3), "socket timeout");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ensures embedded mode returns provider output and persists the resulting
/// history/debug events.
#[test]
fn embedded_mode_returns_provider_response_and_persists_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "hello")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "response should not be empty: {r:?}");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let branch = persisted_agent_branch(&sp, "s1");
    assert!(
        2 <= branch.len(),
        "should have user msg + agent response, got {}",
        branch.len()
    );

    // Debug-log mirror: every turn that goes through the harness
    // should produce both an inbound `from_connection` line capturing
    // the raw agent frame, and a `published` line capturing the
    // enriched copy the harness committed. This is what
    // cache/cost-analysis tooling reads.
    let jsonl = std::fs::read_to_string(sessions_dir.join("s1").join("events.jsonl"))
        .expect("events.jsonl should exist for session s1");
    let parsed: Vec<serde_json::Value> = jsonl
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl"))
        .collect();
    let from_connection_finished = parsed
        .iter()
        .filter(|e| {
            e["type"] == "from_connection" && e["event_name"] == "provider.response_finished"
        })
        .count();
    let published_finished = parsed
        .iter()
        .filter(|e| e["type"] == "published" && e["event_name"] == "provider.response_finished")
        .count();
    assert!(
        1 <= from_connection_finished,
        "expected ≥1 inbound provider.response_finished line, got {from_connection_finished}",
    );
    assert!(
        1 <= published_finished,
        "expected ≥1 published provider.response_finished line, got {published_finished}",
    );
}

/// Ensures daemon mode accepts multiple later socket clients and persists both
/// cycles.
#[test]
fn daemon_mode_accepts_later_clients() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(2).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let r1 = send_daemon_message(&sock, "s1", "hello").expect("first");
    let r2 = send_daemon_message(&sock, "s1", "again").expect("second");
    assert_eq!(r1, "hello", "first cycle should echo our submission");
    assert_eq!(r2, "again", "second cycle should echo our submission");

    server.join().expect("join").expect("daemon clean exit");
    let branches = persisted_agent_branches(&sp, "s1");
    // The sandbox may not have any AGENTS.md to inject, so assert the
    // two user-visible cycles rather than an environment-dependent total.
    let mut submitted_user_texts: Vec<&str> = branches
        .iter()
        .flat_map(|branch| branch.iter())
        .filter_map(|entry| match entry {
            AgentEntry::UserInput { items } => items.iter().find_map(|item| match item {
                ContextItem::Message(message) if message.role == ContextRole::User => {
                    message.content.first().map(|part| match part {
                        ContentPart::Text { text } => text.as_str(),
                    })
                }
                _ => None,
            }),
            _ => None,
        })
        .filter(|text| *text == "hello" || *text == "again")
        .collect();
    submitted_user_texts.sort_unstable();
    assert_eq!(
        submitted_user_texts,
        vec!["again", "hello"],
        "expected both submitted prompts to persist, got {branches:?}"
    );
    assert_eq!(
        branches
            .iter()
            .flat_map(|branch| branch.iter())
            .filter(|entry| matches!(entry, AgentEntry::ToolResults { .. }))
            .count(),
        2,
        "expected both tool result rounds to persist, got {branches:?}"
    );
}

/// Ensures daemon debug system-prompt rendering uses the requested role over
/// the socket path.
#[test]
fn daemon_mode_renders_system_prompt_for_requested_role() {
    // `tau dev print-system-prompt` asks the daemon for the rendered system
    // prompt. Exercise the socket helper rather than a direct Harness call so
    // the debug command's request/response path is covered.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let prompt =
        get_daemon_rendered_system_prompt(&sock, "senior-engineer").expect("render prompt");
    assert!(prompt.contains("## Your mission"));
    assert!(prompt.contains("## Agent identity"));
    assert!(prompt.contains("Your agent id is `dev-preview-agent`."));

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon debug tool rendering uses the requested role over the socket
/// path.
#[test]
fn daemon_mode_renders_tool_definitions_for_requested_role() {
    // `tau dev print-tools` asks the daemon for the same tool definitions the
    // harness would include in provider prompts. Cover the socket endpoint so
    // role filtering stays shared with actual agent turns.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let tools = get_daemon_rendered_tool_definitions(&sock, "senior-engineer")
        .expect("render tool definitions");
    assert!(!tools.is_empty());
    let read_tool = tools
        .iter()
        .find(|tool| tool.name.as_str() == "read")
        .expect("read tool should be available");
    assert!(
        read_tool
            .description
            .as_deref()
            .is_some_and(|d| d.contains("Reads a file"))
    );
    assert!(read_tool.parameters.is_some());

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon tool rendering reports unknown roles instead of using
/// fallback data.
#[test]
fn daemon_mode_reports_unknown_role_for_rendered_tool_definitions_request() {
    // Tool diagnostics should fail in-band for role typos, matching prompt
    // diagnostics and avoiding a misleading dump for the selected fallback role.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let error =
        get_daemon_rendered_tool_definitions(&sock, "missing-role").expect_err("unknown role");
    assert!(
        matches!(error, HarnessError::Participant(message) if message.contains("unknown role"))
    );

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures daemon prompt rendering reports unknown roles instead of using
/// fallback data.
#[test]
fn daemon_mode_reports_unknown_role_for_rendered_system_prompt_request() {
    // The debug prompt endpoint must fail in-band with a participant error for
    // typos, instead of silently falling back to the selected role and printing
    // misleading prompt content.
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let error = get_daemon_rendered_system_prompt(&sock, "missing-role").expect_err("unknown role");
    assert!(
        matches!(error, HarnessError::Participant(message) if message.contains("unknown role"))
    );

    server.join().expect("join").expect("daemon clean exit");
}

/// Ensures embedded mode can execute the read tool against a real file fixture.
#[test]
fn embedded_mode_can_read_files() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let fp = td.path().join("note.txt");
    std::fs::write(&fp, "hello from disk").expect("write fixture");
    let r = run_embedded_message_with_echo(&sp, "s1", &format!("read {}", fp.display()))
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "read response should not be empty");
    assert!(r.contains("hello from disk"));
}

/// Ensures embedded mode can execute shell commands through the echo harness.
#[test]
fn embedded_mode_can_run_shell_commands() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "shell printf hi")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "shell response should not be empty");
}

/// Ensures traced embedded shell runs still return final output when transient
/// progress is missed.
#[test]
fn traced_embedded_returns_shell_output_when_progress_is_missed() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
    // Socket clients can miss short-lived progress when the command
    // completes before the subscription writer drains, but the final
    // response must still arrive and lifecycle tracing is covered above.
    assert!(!o.response.is_empty(), "shell response should not be empty");
}

/// Ensures daemon-mode shell interactions report lifecycle events and clean up
/// their owned socket path after the daemon exits.
#[test]
fn traced_daemon_reports_lifecycle_and_cleans_up_socket_for_shell_run() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    wait_for_socket(&sock);

    let o = send_daemon_message_with_trace(&sock, "s1", "shell printf hi").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell ready")
    );
    // Socket clients may miss short-lived progress if the shell command
    // completes before the writer drains the transient event.
    assert!(!o.response.is_empty(), "shell response should not be empty");
    server.join().expect("join").expect("clean exit");
    assert!(!sock.exists(), "daemon socket should be cleaned up");
}

/// Ensures traced embedded runs report provider lifecycle messages.
#[test]
fn traced_embedded_reports_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "hello").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider starting")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension provider exited")
    );
}

/// Ensures daemon helpers surface an in-band socket disconnect reason as a
/// participant error.
#[test]
fn daemon_disconnect_reason_is_reported() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let listener = bind_listener(&sock).expect("bind");

    let server = thread::spawn(move || {
        let mut accepted = listener.accept().expect("accept");
        let _ = accepted.recv(); // hello
        let _ = accepted.recv(); // subscribe
        let _ = accepted.recv(); // message
        accepted
            .send(&HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some("test disconnect".to_owned()),
            }))
            .expect("write");
    });

    let err =
        send_daemon_message_with_trace(&sock, "s1", "hello").expect_err("should get disconnect");
    assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
    server.join().expect("join");
}

/// Ensures harness startup eagerly initializes the configured session before
/// use.
#[test]
fn harness_startup_eagerly_initializes_eager_session() {
    // Guards against the recurring "this looks like redundant work"
    // urge to lazy-ify session init. `echo_harness` calls
    // `Harness::new_with_provider`, which must eagerly initialize the
    // session before returning — see the design-choice comment in
    // the constructor for why.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");

    assert!(
        h.initialized_sessions.contains("s1"),
        "eager init should mark the bound session as initialized at startup; \
         `initialized_sessions` was {:?}",
        h.initialized_sessions
    );
    assert!(
        matches!(h.turn_state, TurnState::Idle),
        "turn state should be Idle after eager init completes"
    );
}

/// Ensures resumed startup publishes a resume-flavored SessionStarted event.
#[test]
fn resumed_startup_publishes_resume_session_started() {
    // Restored daemons get only the eager startup `SessionStarted` to tell
    // extensions that existing per-session state should be resumed instead of
    // treated as a brand-new harness session.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("start");

    let mut next_seq = crate::event_log::EventLogSeq::new(0);
    let mut session_started_reason = None;
    while let Some(entry) = h.event_log.get_next_from(next_seq) {
        next_seq = entry.seq.next();
        if let Event::SessionStarted(started) = entry.event
            && started.session_id.as_str() == "s1"
        {
            session_started_reason = Some(started.reason);
            break;
        }
    }

    assert_eq!(
        session_started_reason,
        Some(tau_proto::SessionStartReason::Resume)
    );
    h.shutdown().expect("shutdown");
}
