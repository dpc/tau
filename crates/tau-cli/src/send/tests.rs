use tau_proto::{Event, NodeId, PromptOriginator, UiRoleUpdateAction, UiTreeNavigationTarget};

use super::event_for_line;
use crate::ui_prompt::DEFAULT_AGENT_ROLE;

const SESSION_ID: &str = "test-session";

fn event(text: &str) -> Option<Event> {
    event_for_line(SESSION_ID, text)
}

fn prompt_text(text: &str) -> String {
    match event(text).expect("prompt event") {
        Event::UiCreateAgent(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.role, DEFAULT_AGENT_ROLE);
            assert_eq!(req.model_override, None);
            assert_eq!(req.originator, PromptOriginator::User);
            assert_eq!(req.ctx_id, None);
            req.initial_prompt.expect("initial prompt")
        }
        other => panic!("expected UiCreateAgent, got {other:?}"),
    }
}

/// Headless send intentionally treats interactive-only exit commands as
/// no-ops.
#[test]
fn quit_and_detach_are_no_ops() {
    assert_eq!(event("/quit"), None);
    assert_eq!(event("/detach"), None);
}

/// `/cancel` maps to the broadcast cancel form; the harness may later
/// retarget it.
#[test]
fn cancel_requests_prompt_cancellation() {
    match event("/cancel").expect("cancel event") {
        Event::UiCancelPrompt(cancel) => {
            assert_eq!(cancel.session_id, SESSION_ID);
            assert_eq!(cancel.agent_prompt_id, None);
        }
        other => panic!("expected UiCancelPrompt, got {other:?}"),
    }
}

/// Tree commands are daemon-side operations, while malformed navigation
/// stays a prompt.
#[test]
fn tree_commands_request_or_navigate_tree() {
    match event("/tree").expect("tree event") {
        Event::UiTreeRequest(req) => assert_eq!(req.session_id, SESSION_ID),
        other => panic!("expected UiTreeRequest, got {other:?}"),
    }

    match event("/tree 42").expect("navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::PromptAnchor(42));
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    match event("/tree 0").expect("root navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::Root);
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    match event("/tree node 42").expect("raw node navigate event") {
        Event::UiNavigateTree(req) => {
            assert_eq!(req.session_id, SESSION_ID);
            assert_eq!(req.target, UiTreeNavigationTarget::Node(NodeId::new(42)));
        }
        other => panic!("expected UiNavigateTree, got {other:?}"),
    }

    assert_eq!(prompt_text("/tree nope"), "/tree nope");
}

/// `/compact` must reach the harness instead of being sent as prompt text.
#[test]
fn compact_requests_compaction() {
    match event("/compact").expect("compact event") {
        Event::UiCompactRequest(req) => assert_eq!(req.session_id, SESSION_ID),
        other => panic!("expected UiCompactRequest, got {other:?}"),
    }
}

/// Local configuration commands are ignored by `tau send`; they only make
/// sense in chat UI.
#[test]
fn local_configuration_commands_are_ignored() {
    for command in ["/fast", "/fast on"] {
        assert_eq!(event(command), None, "{command}");
    }
}

/// Role selection and model selection commands are forwarded to their distinct
/// control events. `/model` targets an agent model id, not a role name.
#[test]
fn role_and_model_selection_commands_are_distinct() {
    assert_eq!(event("/role"), None);

    match event("/role reviewer").expect("role select") {
        Event::UiRoleSelect(select) => assert_eq!(select.role, "reviewer"),
        other => panic!("expected UiRoleSelect, got {other:?}"),
    }

    match event("/model test/model").expect("agent model select") {
        Event::UiAgentModelSelect(select) => {
            assert_eq!(select.session_id, SESSION_ID);
            assert_eq!(select.target_agent_id, None);
            assert_eq!(select.model.to_string(), "test/model");
        }
        other => panic!("expected UiAgentModelSelect, got {other:?}"),
    }

    assert_eq!(event("/model reviewer"), None);
    assert_eq!(event("/model "), None);
}

/// `/role <role> delete` is the headless spelling for deleting a runtime
/// role override.
#[test]
fn role_delete_command_updates_roles() {
    match event("/role scratch delete").expect("role update") {
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
