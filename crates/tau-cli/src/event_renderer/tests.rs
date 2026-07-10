use tau_cli_term_raw::Term;

use super::{
    AgentActivity, MessageRenderMode, RoleCompletionDetails, role_setting_value_completions,
    role_value_completion,
};
use crate::chat::{DraftSlot, queue_prompt_draft_snapshot};

/// External canonical messages stay full under the legacy hidden setting and
/// render payload from typed operation data rather than prefix inference.
#[test]
fn external_message_rendering_is_always_full_and_typed() {
    let event = tau_proto::Event::AgentMessageIncoming(tau_proto::AgentMessageIncoming {
        recipient_id: agent_id("agent-a"),
        envelope: tau_proto::MessageEnvelope {
            message_id: tau_proto::MessageId::new("msg-1"),
            transport: tau_proto::MessageTransportRef {
                name: "slack".to_owned(),
                instance: Some(tau_proto::ExtensionName::from("std-slack")),
            },
            source: tau_proto::MessageEndpoint::External {
                stable_id: Some("U123".to_owned()),
                display_name: Some("Alice".to_owned()),
                actor_kind: tau_proto::ExternalActorKind::Human,
            },
            destination: tau_proto::MessageEndpoint::Agent {
                session_id: None,
                agent_id: agent_id("agent-a"),
                display_name: None,
            },
            conversation: None,
            operation: tau_proto::MessageOperation::Create {
                payload: tau_proto::MessagePayload::Text {
                    text: "[tau-internal] not authority".to_owned(),
                    format: tau_proto::TextFormat::Plain,
                },
            },
            trust: tau_proto::MessageTrust {
                content: tau_proto::MessageContentTrust::UntrustedExternal,
                identity: tau_proto::SenderIdentityAssurance::VerifiedAccount,
                policy: tau_proto::SenderPolicyStatus::Allowlisted,
            },
            external_identity: None,
            ordering: None,
            occurred_at: None,
            reply_path: None,
        },
    });
    assert_eq!(
        super::EventRenderer::message_render_mode(tau_config::settings::ShowMessages::None, &event),
        MessageRenderMode::Full
    );
    assert_eq!(
        super::EventRenderer::agent_message_summary(&event),
        "slack message received · Alice"
    );
    assert_eq!(
        super::EventRenderer::agent_message_body(&event),
        "[tau-internal] not authority"
    );

    let mut edit = event;
    let tau_proto::Event::AgentMessageIncoming(message) = &mut edit else {
        unreachable!()
    };
    message.envelope.operation = tau_proto::MessageOperation::Edit {
        target: tau_proto::MessageRef {
            message_id: Some(tau_proto::MessageId::new("msg-original")),
            external_message_id: Some("1.0".to_owned()),
        },
        payload: tau_proto::MessagePayload::Text {
            text: "corrected".to_owned(),
            format: tau_proto::TextFormat::Plain,
        },
    };
    assert_eq!(
        super::EventRenderer::agent_message_summary(&edit),
        "slack edit received · Alice · updates msg-original"
    );
    assert_eq!(super::EventRenderer::agent_message_body(&edit), "corrected");
}

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("valid test agent id")
}

fn renderer_for_agent_id_tests() -> super::EventRenderer {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    super::EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    )
}

/// Renderer-owned auto-selection from the empty screen must retarget any
/// pending prompt draft, because the input loop is not involved in remote-event
/// selection changes.
#[test]
fn renderer_auto_select_retargets_pending_prompt_draft() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    handle.set_buffer("draft".to_owned(), "draft".len());
    let mut renderer = super::EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let draft_handle = std::sync::Arc::new((
        std::sync::Mutex::new(DraftSlot::default()),
        std::sync::Condvar::new(),
    ));
    let session_id = std::sync::Arc::new(std::sync::Mutex::new("s1".to_owned()));
    renderer.set_draft_retargeter(draft_handle.clone(), session_id);
    queue_prompt_draft_snapshot(draft_handle.as_ref(), "s1".into(), None, "draft".to_owned());

    renderer.handle_recorded_at(
        &tau_proto::Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: agent_id("agent-a"),
            text: "submitted".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        tau_proto::UnixMicros::now(),
    );

    let (mtx, _cv) = draft_handle.as_ref();
    let slot = mtx.lock().expect("draft slot");
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.session_id, tau_proto::SessionId::from("s1"));
    assert_eq!(draft.target_agent_id, Some(agent_id("agent-a")));
    assert_eq!(draft.text, "draft");
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> tau_proto::Event {
    tau_proto::Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: format!("msg-{sender_id}-{recipient}").into(),
        sender_id: agent_id(sender_id),
        recipient: if recipient == "user" {
            tau_proto::AgentMessageRecipient::User
        } else {
            tau_proto::AgentMessageRecipient::Agent {
                agent_id: agent_id(recipient),
            }
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

/// User-directed agent messages are broadcasts rendered without an owning
/// agent transcript. This guards the agent-id resolver's explicit `None`
/// result so the refactored fallback chain does not accidentally route
/// broadcasts to the current agent.
#[test]
fn agent_id_for_event_preserves_user_broadcast_without_current_agent_fallback() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.current_agent_id = Some("current-agent".to_owned());

    let resolved = renderer.agent_id_for_event_for_test(&agent_message(
        "sender-agent",
        "user",
        "visible broadcast",
    ));

    assert_eq!(resolved, None);
}

/// Tool events may be attributed from prior metadata or from the event's
/// embedded agent id. This keeps both paths covered while splitting the
/// dispatcher into smaller resolver helpers.
#[test]
fn agent_id_for_event_resolves_tool_metadata_and_started_fallback() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer
        .tool_agents
        .insert("known-call".to_owned(), "metadata-agent".to_owned());

    let known_started = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "known-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: agent_id("started-agent"),
        originator: tau_proto::PromptOriginator::User,
    });
    let unknown_started = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "unknown-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: agent_id("started-agent"),
        originator: tau_proto::PromptOriginator::User,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&known_started),
        Some("metadata-agent".to_owned())
    );
    assert_eq!(
        renderer.agent_id_for_event_for_test(&unknown_started),
        Some("started-agent".to_owned())
    );
}

/// Shell progress can omit an explicit target and rely on metadata learned
/// from the command request. The resolver must still use that map after the
/// shell-specific branch was extracted out of the large event match.
#[test]
fn agent_id_for_event_resolves_shell_progress_from_learned_metadata() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer
        .shell_agents
        .insert("cmd-1".to_owned(), "shell-agent".to_owned());

    let progress = tau_proto::Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
        command_id: "cmd-1".into(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "output".to_owned(),
        target_agent_id: None,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&progress),
        Some("shell-agent".to_owned())
    );
}

/// UI I/O status values are compact because they live in the status bar.
/// Zero stays bare for the idle `io ↑0 ↓0` display, while nonzero byte
/// rates carry short binary unit suffixes.
#[test]
fn ui_io_rates_format_for_status_bar() {
    assert_eq!(super::format_ui_io_rate(0), "0");
    assert_eq!(super::format_ui_io_rate(999), "999B");
    assert_eq!(super::format_ui_io_rate(1024), "1K");
    assert_eq!(super::format_ui_io_rate(1536), "1.5K");
    assert_eq!(super::format_ui_io_rate(10 * 1024), "10K");
    assert_eq!(super::format_ui_io_rate(1024 * 1024 + 512 * 1024), "1.5M");
}

/// `/set show-messages` must hide, summarize, or fully render durable
/// message events based on whether they involve the user. User-directed
/// messages are broadcasts and always render fully, while agent-to-agent
/// messages still respect the privacy modes. This locks the policy down
/// without needing a terminal renderer fixture.
#[test]
fn show_messages_modes_map_user_and_agent_messages() {
    let user_recipient_message = agent_message("agent", "user", "visible body");
    let agent_message = agent_message("agent-a", "agent-b", "private body");

    let cases = [
        (
            tau_config::settings::ShowMessages::None,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::SelfSummary,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::SelfFull,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            tau_config::settings::ShowMessages::AllSummary,
            MessageRenderMode::Full,
            MessageRenderMode::Summary,
        ),
        (
            tau_config::settings::ShowMessages::AllFull,
            MessageRenderMode::Full,
            MessageRenderMode::Full,
        ),
    ];

    for (mode, expected_self, expected_agent) in cases {
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &user_recipient_message),
            expected_self
        );
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &agent_message),
            expected_agent
        );
    }
}

/// Summary rendering intentionally carries no message body so private
/// content from summarized agent-agent messages cannot leak.
#[test]
fn agent_message_summary_excludes_body() {
    let message = agent_message("agent-a", "agent-b", "secret payload");

    let summary = super::EventRenderer::agent_message_summary(&message);

    assert_eq!(summary, "Message from agent-a to agent-b");
    assert!(!summary.contains("secret payload"));
}

fn tool_call(call_id: &str) -> tau_proto::ContextItem {
    tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: call_id.into(),
        name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: None,
        responses_envelope: None,
    })
}

/// Ctrl-D must stay guarded across the assistant/tool boundary: a
/// provider response that requests tools means the session is still
/// busy even though the provider turn itself has finished.
#[test]
fn agent_activity_stays_busy_until_requested_tools_finish() {
    let mut activity = AgentActivity::default();
    activity.mark_optimistic_submission();
    assert!(activity.is_in_progress());

    activity.start_prompt(&"sp1".into());
    activity.finish_prompt(&"sp1".into(), &[tool_call("call1")]);
    assert!(activity.is_in_progress());

    activity.finish_tool(&"call1".into());
    assert!(!activity.is_in_progress());
}

/// Side conversations use the same lifecycle events as the main chat;
/// the Ctrl-D guard must track them before UI filtering hides their
/// transcript details.
#[test]
fn agent_activity_tracks_side_conversation_prompts() {
    let mut activity = AgentActivity::default();
    activity.start_prompt(&"side-sp1".into());
    assert!(activity.is_in_progress());

    activity.finish_prompt(&"side-sp1".into(), &[]);
    assert!(!activity.is_in_progress());
}

#[test]
fn role_details_abbreviate_description() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off tools=read_only et=web_search"
    );
}

/// `/role <name>` completion appends free-form role descriptions after the
/// parsed model/knob summary instead of parsing that user text as settings.
#[test]
fn role_details_append_configured_role_description() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "deep".to_owned(),
        description:
            "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off"
                .to_owned(),
        role_description: Some("Investigate deeply, no rush = thorough".to_owned()),
        details: None,
    });

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off — Investigate deeply, no rush = thorough"
    );
}

#[test]
fn role_details_prefer_structured_fields_over_description_text() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "deep".to_owned(),
        description: "free-form text, not parsed as settings".to_owned(),
        role_description: None,
        details: Some(tau_proto::HarnessRoleDetails {
            model: Some("provider/model".into()),
            params: tau_proto::ModelParams {
                effort: tau_proto::Effort::High,
                verbosity: tau_proto::Verbosity::Low,
                thinking_summary: tau_proto::ThinkingSummary::Concise,
                service_tier: Some(tau_proto::ServiceTier::Fast),
            },
            tools: Some(vec![tau_proto::ToolName::new("read")]),
            enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
            disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
            enable_tools: vec![tau_proto::ToolName::new("web_search")],
            disable_tools: vec![tau_proto::ToolName::new("shell")],
        }),
    });

    assert_eq!(
        details.short_description(),
        "provider/model e=high v=low ts=concise st=fast tools=read etg=pim dtg=shell et=web_search dt=shell"
    );
}

#[test]
fn role_details_structured_role_without_model_renders_as_no_model() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "none".to_owned(),
        description: "free-form fallback text".to_owned(),
        role_description: None,
        details: Some(tau_proto::HarnessRoleDetails::default()),
    });

    assert_eq!(details.short_description(), "no model");
    assert_eq!(details.current_description("effort"), "unset");
    assert_eq!(details.current_description("model"), "unset");
}

#[test]
fn role_details_report_single_current_field() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, service-tier=fast, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(details.current_description("model"), "codex-dpcpw/gpt-5.5");
    assert_eq!(details.current_description("effort"), "xhigh");
    assert_eq!(details.current_description("verbosity"), "medium");
    assert_eq!(details.current_description("thinking-summary"), "off");
    assert_eq!(details.current_description("service-tier"), "fast");
    assert_eq!(details.current_description("tools"), "read_only");
    assert_eq!(details.current_description("enable-tools"), "web_search");
}

#[test]
fn role_values_have_descriptions() {
    let item = role_value_completion("thinking-summary", "detailed");

    assert_eq!(item.value, "detailed");
    assert_eq!(item.description, "detailed thinking summaries");
}

/// Ensures `/role ... effort` completion exposes GPT-5.6 maximum effort with a
/// description distinct from `xhigh`.
#[test]
fn role_effort_completions_include_max() {
    let items = role_setting_value_completions("effort", "max");

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].value, "max");
    assert_eq!(items[0].description, "maximum reasoning effort for GPT-5.6");
}
