use std::os::unix::net as path_std_os_unix_net;
use std::time as path_std_time;

use tau_proto::{
    Event, HarnessInputMessage, HarnessOutputMessage, NodeId, PromptOriginator, UiRoleUpdateAction,
    UiTreeNavigationTarget,
};

use super::{event_for_test_line, message_for_line, read_tree_result, run_send};
use crate::ui_prompt::DEFAULT_AGENT_ROLE;

const SESSION_ID: &str = "test-session";

fn event(text: &str) -> Option<Event> {
    event_for_test_line(SESSION_ID, text)
}

fn message(text: &str) -> Option<HarnessInputMessage> {
    message_for_line(SESSION_ID, text)
}

/// The external headless-send boundary must reject an invalid controlled
/// session identifier instead of reaching infallible UI event construction.
#[test]
fn headless_send_rejects_invalid_session_id_without_panicking() {
    let error = run_send("bad.id", "hello").expect_err("invalid session id must fail");
    assert!(error.to_string().contains("invalid session id `bad.id`"));
}

fn prompt_text(text: &str) -> String {
    match event(text).expect("prompt event") {
        Event::UiCreateAgent(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.role, DEFAULT_AGENT_ROLE);
            assert_eq!(req.model_override, None);
            assert_eq!(req.originator, PromptOriginator::User);
            let ctx_id = req.ctx_id.as_deref().expect("prompt correlation id");
            assert!(!ctx_id.is_empty());
            assert_ne!(ctx_id, req.request_id);
            req.initial_prompt.expect("initial prompt")
        }
        other => panic!("expected UiCreateAgent, got {other:?}"),
    }
}

/// Headless send intentionally treats interactive-only exit commands as
/// no-ops.
#[test]
fn quit_and_detach_are_no_ops() {
    assert_eq!(event(":quit"), None);
    assert_eq!(event(":detach"), None);
}

/// Headless send must accept exact interactive picker commands as explicit
/// no-ops while rejecting argument-bearing variants locally.
#[test]
fn agent_picker_commands_are_exact_headless_no_ops() {
    for command in [":pick-agent", ":pick-agent-all"] {
        run_send("definitely-not-running", command).expect("exact picker command must be a no-op");
        let malformed = format!("{command} unexpected");
        let error = run_send("definitely-not-running", &malformed)
            .expect_err("picker arguments must fail before daemon lookup");
        assert!(error.to_string().contains("unknown or unsupported command"));
    }
}

/// `:session-stats` is interactive-only, so headless send accepts its exact
/// form without daemon contact while rejecting accidental arguments.
#[test]
fn session_stats_is_an_exact_headless_no_op() {
    run_send("definitely-not-running", ":session-stats")
        .expect("exact session stats command must be a no-op");
    let error = run_send("definitely-not-running", ":session-stats unexpected")
        .expect_err("session stats arguments must fail before daemon lookup");
    assert!(error.to_string().contains("unknown or unsupported command"));
}

/// `:cancel` maps to the broadcast cancel form; the harness may later
/// retarget it.
#[test]
fn cancel_requests_prompt_cancellation() {
    match event(":cancel").expect("cancel event") {
        Event::UiCancelPrompt(cancel) => {
            assert_eq!(cancel.session_id, SESSION_ID);
            assert_eq!(cancel.agent_prompt_id, None);
        }
        other => panic!("expected UiCancelPrompt, got {other:?}"),
    }
}

/// Headless send must provide the same exact logical-retry control as the
/// interactive static command rather than submitting prompt text.
#[test]
fn retry_requests_exact_delayed_prompt_release() {
    match event(":retry").expect("retry event") {
        Event::UiRetryPrompt(retry) => {
            assert_eq!(retry.session_id, SESSION_ID);
            assert_eq!(retry.target_agent_id, None);
            assert_eq!(retry.agent_prompt_id, None);
        }
        other => panic!("expected UiRetryPrompt, got {other:?}"),
    }
    assert!(
        !matches!(event(":retry later"), Some(Event::UiRetryPrompt(_))),
        "arguments must not invoke retry"
    );
    assert!(matches!(event(":retry "), Some(Event::UiRetryPrompt(_))));
    for malformed in [":retry\tlater", ":retry\nlater"] {
        assert_eq!(
            event(malformed),
            None,
            "malformed retry syntax must neither retry nor become a prompt"
        );
    }
}

/// Tree commands are daemon-side operations, while malformed navigation
/// stays a prompt.
#[test]
fn tree_commands_request_or_navigate_tree() {
    match message(":tree").expect("tree message") {
        HarnessInputMessage::UiTreeRequest(req) => assert_eq!(req.session_id, SESSION_ID),
        other => panic!("expected UiTreeRequest message, got {other:?}"),
    }

    match event(":tree 42").expect("navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::PromptAnchor(42));
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    match event(":tree 0").expect("root navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::Root);
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    match event(":tree node 42").expect("raw node navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::Node(NodeId::new(42)));
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    assert_eq!(event(":tree nope"), None);
}

/// The one-shot headless tree client consumes the same single multiline notice
/// sent to interactive UIs.
#[test]
fn headless_tree_result_reads_one_multiline_notice() {
    let (reader_stream, mut harness_stream) =
        path_std_os_unix_net::UnixStream::pair().expect("reader stream pair");
    reader_stream
        .set_read_timeout(Some(path_std_time::Duration::from_secs(2)))
        .expect("read timeout");
    let (writer_stream, _discard_stream) =
        path_std_os_unix_net::UnixStream::pair().expect("writer stream pair");
    let (mut reader, _writer) =
        crate::ui_client::connect_ui_streams(reader_stream, writer_stream, "tree-test", None)
            .expect("connect UI streams");
    let mut harness_writer = tau_proto::HarnessOutputWriter::new(&mut harness_stream);
    harness_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::now(),
            Event::HarnessNotice(tau_proto::HarnessNotice {
                kind: tau_proto::notice_kind::HARNESS_NOTICE.to_owned(),
                message: "root\nfirst prompt\nsecond prompt".to_owned(),
                level: tau_proto::NoticeLevel::Info,
                always_show: false,
            }),
        ))
        .expect("write tree result");
    harness_writer.flush().expect("flush tree result");

    assert_eq!(
        read_tree_result(&mut reader).expect("read tree result"),
        "root\nfirst prompt\nsecond prompt"
    );
}

/// `:compact` must reach the harness instead of being sent as prompt text.
#[test]
fn compact_requests_compaction() {
    match event(":compact").expect("compact event") {
        Event::UiCompactRequest(req) => assert_eq!(req.session_id, SESSION_ID),
        other => panic!("expected UiCompactRequest, got {other:?}"),
    }
}

/// Local configuration commands are ignored by `tau send`; they only make
/// sense in chat UI.
#[test]
fn local_configuration_commands_are_ignored() {
    let command = ":fast";
    assert_eq!(event(command), None);
    assert!(
        run_send("definitely-not-running", command).is_ok(),
        "{command} must be a valid headless no-op"
    );
}

/// Role selection and model selection commands are forwarded to their distinct
/// control events. `:model` targets an agent model id, not a role name.
#[test]
fn role_and_model_selection_commands_are_distinct() {
    assert_eq!(event(":role"), None);

    match event(":role reviewer").expect("role select") {
        Event::UiRoleSelect(select) => assert_eq!(select.role, "reviewer"),
        other => panic!("expected UiRoleSelect, got {other:?}"),
    }

    match event(":model test/model").expect("agent model select") {
        Event::UiAgentModelSelect(select) => {
            assert_eq!(select.session_id, SESSION_ID);
            assert_eq!(select.target_agent_id, None);
            assert_eq!(select.model.to_string(), "test/model");
        }
        other => panic!("expected UiAgentModelSelect, got {other:?}"),
    }

    assert_eq!(event(":model reviewer"), None);
    assert_eq!(event(":model "), None);
}

/// `:role <role> delete` is the headless spelling for deleting a runtime
/// role override.
#[test]
fn role_delete_command_updates_roles() {
    match event(":role scratch delete").expect("role update") {
        Event::UiRoleUpdate(update) => {
            assert_eq!(update.role, "scratch");
            assert_eq!(update.action, UiRoleUpdateAction::Delete);
        }
        other => panic!("expected UiRoleUpdate, got {other:?}"),
    }
}

/// Shell commands produce dynamic ids but preserve command text and
/// context-inclusion mode.
#[test]
fn shell_commands_record_context_mode() {
    match event("!! echo hi").expect("ui-only shell command") {
        Event::UiShellCommand(command) => {
            assert_eq!(command.session_id, SESSION_ID);
            assert!(command.command_id.as_str().starts_with("ui-sh-"));
            assert_eq!(command.command, "echo hi");
            assert!(!command.include_in_context);
        }
        other => panic!("expected UiShellCommand, got {other:?}"),
    }

    match event("! echo hi").expect("context shell command") {
        Event::UiShellCommand(command) => {
            assert_eq!(command.session_id, SESSION_ID);
            assert!(command.command_id.as_str().starts_with("ui-sh-"));
            assert_eq!(command.command, "echo hi");
            assert!(command.include_in_context);
        }
        other => panic!("expected UiShellCommand, got {other:?}"),
    }

    assert_eq!(event("!!"), None);
    assert_eq!(event("!"), None);
}

/// Unrecognized text is submitted unchanged as a normal user prompt.
#[test]
fn normal_text_submits_user_prompt() {
    assert_eq!(prompt_text("explain this diff"), "explain this diff");
}

/// Ensures headless send does not retain aliases for the removed slash grammar.
#[test]
fn old_slash_commands_submit_as_prompt_text() {
    for line in ["/quit", "/tree", "/model test/model", "/skill demo"] {
        assert_eq!(prompt_text(line), line);
    }
}

/// Ensures headless send removes the doubled-colon escape before constructing
/// the canonical prompt event.
#[test]
fn literal_colon_escape_submits_canonical_prompt_text() {
    assert_eq!(prompt_text("::literal"), ":literal");
    assert_eq!(prompt_text("  ::literal"), ":literal");
    let Event::UiCreateAgent(request) = event("::skill demo").expect("literal prompt event") else {
        panic!("expected create-agent prompt");
    };
    assert_eq!(request.initial_prompt.as_deref(), Some(":skill demo"));
    assert!(request.literal);
}

/// Ensures unknown colon commands do not silently become model prompts in the
/// headless parser.
#[test]
fn unknown_colon_commands_are_not_prompt_events() {
    assert_eq!(event(":unknown argument"), None);
    assert_eq!(event(":tree invalid"), None);
    let error = run_send("definitely-not-running", ":unknown argument")
        .expect_err("unknown command must fail before daemon lookup");
    assert!(error.to_string().contains("unknown or unsupported command"));
}

/// Ensures malformed interactive-only commands fail locally rather than being
/// silently accepted as headless no-ops.
#[test]
fn malformed_headless_noop_commands_fail_locally() {
    for line in [
        ":quit junk",
        ":new one two",
        ":ephemeral garbage",
        ":theme one two",
        ":set only-name",
        ":session nope",
        ":debug-show-event-stats",
        ":agent bogus",
    ] {
        let error = run_send("definitely-not-running", line)
            .expect_err("malformed command must fail before daemon lookup");
        assert!(
            error.to_string().contains("unknown or unsupported command"),
            "{line}: {error}"
        );
    }
}
