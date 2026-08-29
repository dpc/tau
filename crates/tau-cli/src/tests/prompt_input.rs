//! Tests for prompt input behavior.

use super::super::DispatchCommand;
use super::super::chat::send_event;
use super::*;

/// The dump-initial-prompt dispatch is an internal validation path, not a
/// startup consumer, so dedicated aliases must be rejected before generic
/// override folding can erase their source identity.
#[test]
fn dump_initial_prompt_dispatch_rejects_alias_inputs() {
    let command = DispatchCommand::Other(CliCommand::Dev {
        command: DevCommand::DumpInitialPrompt {
            out: "prompt.txt".into(),
            message: "hello".to_owned(),
        },
    });
    assert!(super::super::rejects_model_reference_alias_inputs(&command));
    let error = super::super::reject_model_reference_alias_inputs(
        super::super::ModelReferenceAliasInputPresence {
            model_flag: true,
            ..Default::default()
        },
        "dev dump-initial-prompt",
    )
    .expect_err("dump path must reject aliases");
    assert!(error.to_string().contains("--model-alias"), "{error}");
}

/// Ensures `:prompt <id>` resolves a configured template to editable prompt
/// text rather than submitting it immediately.
#[test]
fn custom_prompt_command_returns_configured_prompt_text() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let replacement = custom_prompt_replacement(":prompt review", &prompts)
        .expect("prompt command")
        .expect("known prompt");

    assert_eq!(replacement, "Review this patch carefully");
}

/// A terminal queue rejection must remove the stale queued marker and render
/// the actionable provider configuration failure.
#[test]
fn rejected_prompt_replaces_queued_marker_with_actionable_failure() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "prompt without providers".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptRejected(AgentPromptRejected {
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        message: "No provider models are available. Run `tau provider list`.".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "prompt without providers (queued)"));
    assert!(vt.screen_contains(100, "No provider models are available"));
    assert!(vt.screen_contains(100, "tau provider list"));
}

/// FIFO prompt terminals remove the corresponding oldest queued marker, so a
/// create-agent failure followed by an ordinary rejection cannot cross-remove
/// adjacent prompts.
#[test]
fn prompt_failure_and_rejection_remove_queued_markers_in_fifo_order() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-a".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    for text in ["initial A", "ordinary B"] {
        renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
            text: text.into(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
        }));
    }

    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-a".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-a".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed".into(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "initial A (queued)"));
    assert!(vt.screen_contains(100, "ordinary B (queued)"));

    renderer.handle(&Event::AgentPromptRejected(AgentPromptRejected {
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        message: "No provider models are available.".into(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "ordinary B (queued)"));
}

#[test]
fn dev_print_prompt_accepts_agents_md_toggle() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "print-prompt",
        "--enable-agents-md",
        "false",
    ]);
    assert!(matches!(
        cli.command,
        Some(super::super::cli::Command::Dev {
            command: super::super::cli::DevCommand::PrintPrompt {
                enable_agents_md: false,
            },
        })
    ));
}

/// Ensures unknown `:prompt` ids produce a clear local error and list
/// configured ids so users can recover without accidentally submitting the
/// command text.
#[test]
fn custom_prompt_command_reports_unknown_id() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let error = custom_prompt_replacement(":prompt missing", &prompts)
        .expect("prompt command")
        .expect_err("unknown prompt should fail");

    assert!(error.contains("unknown custom prompt `missing`"));
    assert!(error.contains("available: review"));
}

/// Ensures ordinary prose does not take a custom-prompt payload snapshot before
/// the command token gate, while whitespace-leading `:prompt` still does.
#[test]
fn ordinary_prose_skips_custom_prompt_payload_work() {
    let snapshot_count = Cell::new(0);
    let snapshot = || {
        snapshot_count.set(snapshot_count.get() + 1);
        vec![tau_proto::HarnessCustomPrompt {
            id: "review".to_owned(),
            text: "Review this patch carefully".to_owned(),
        }]
    };

    assert!(
        custom_prompt_replacement_from_snapshot("review this patch carefully", snapshot).is_none()
    );
    assert_eq!(snapshot_count.get(), 0);
    assert!(custom_prompt_replacement_from_snapshot("::prompt review", snapshot).is_none());
    assert_eq!(snapshot_count.get(), 0);

    let replacement =
        custom_prompt_replacement_from_snapshot(" \t:prompt review", snapshot).expect("command");
    assert_eq!(
        replacement.expect("configured prompt"),
        "Review this patch carefully"
    );
    assert_eq!(snapshot_count.get(), 1);
}

/// Ensures `:prompt` remains a local command for command echo/history
/// routing and does not fall through as a normal user prompt.
#[test]
fn prompt_command_is_known_static_command() {
    assert!(is_known_static_command(":prompt review"));
}

/// Protects the final command ownership fallback recorded by
/// `SPEC-tau-cli-command-mode`: a likely mistyped leading command
/// must not become a normal prompt, while non-leading slashes remain prompt
/// text.
#[test]
fn leading_command_tokens_are_identified_before_prompt_submission() {
    assert_eq!(leading_command_token(":typo"), Some(":typo"));
    assert_eq!(leading_command_token("  :typo arg"), Some(":typo"));
    assert_eq!(
        leading_command_token(":skill:jujutsu args"),
        Some(":skill:jujutsu")
    );
    assert_eq!(leading_command_token("hello /typo"), None);
    assert_eq!(leading_command_token("please inspect /tmp/file"), None);
    assert_eq!(leading_command_token("./relative/path"), None);
}

/// Gmail OAuth finish input must use one fixed presentation across direct and
/// literal-escaped command spellings so code/state never enters echo or
/// history. The adjacent startup-profile assertion keeps the original refusal
/// regression covered after sharing this command-classification test location.
#[test]
fn gmail_oauth_finish_redirect_url_is_redacted_from_echo_and_prompt_history() {
    let line = ":email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    let redacted = ":email auth google finish <redacted>";
    assert_eq!(redacted_command_echo_line(line), redacted);
    assert_eq!(redacted_prompt_history_line(line, line), redacted);
    assert!(!redacted_command_echo_line(line).contains("auth-code-secret"));
    let missing_account = ":email auth google finish http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    assert_eq!(redacted_command_echo_line(missing_account), redacted);
    assert!(
        !redacted_prompt_history_line(missing_account, missing_account)
            .contains("auth-code-secret")
    );
    assert_eq!(
        redacted_command_echo_line(":email auth google start work"),
        ":email auth google start work"
    );
    let escaped = tau_cli_term::canonical_literal_colon_prompt(
        "::email auth google finish work http://localhost/?code=auth-code-secret",
    )
    .expect("literal escape canonicalizes");
    assert_eq!(
        redacted_prompt_history_line(&escaped, escaped.trim()),
        redacted
    );

    let profile_error =
        super::super::reject_dev_tmux_startup_overrides(Some("focused"), None, &[], &[], &[])
            .expect_err("profile refused");
    assert!(profile_error.to_string().contains("configuration profile"));
}

/// Ordinary prose needs no presentation or history transformation, so both
/// views must borrow the submitted bytes until an independent owner retains
/// them.
#[test]
fn ordinary_prompt_presentation_and_history_views_borrow_submitted_bytes() {
    let line = "large ordinary prompt ".repeat(4_096);
    let text = line.trim();

    let presentation = redacted_command_echo_line(text);
    let history = redacted_prompt_history_line(&line, text);

    assert!(matches!(presentation, std::borrow::Cow::Borrowed(_)));
    assert!(matches!(history, std::borrow::Cow::Borrowed(_)));
    assert_eq!(presentation.as_ptr(), text.as_ptr());
    assert_eq!(history.as_ptr(), line.as_ptr());
}

/// Content-enabled draft publication must preserve ordinary text while
/// replacing every recognizable Gmail OAuth finish buffer before serialization.
#[test]
fn contentful_prompt_drafts_redact_gmail_oauth_finish_buffers() {
    const CODE: &str = "CODE_SENTINEL_46";
    const STATE: &str = "STATE_SENTINEL_46";
    const REDACTED: &str = ":email auth google finish <redacted>";
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let sensitive =
        format!(":email auth google finish work http://127.0.0.1:54321/?code={CODE}&state={STATE}");

    queue_prompt_draft_snapshot(&handle, test_session_id("s1"), None, sensitive);
    let encoded = {
        let (mtx, _cv) = &handle;
        let slot = super::super::locked(mtx);
        let (_, draft) = slot.pending.as_ref().expect("pending sensitive draft");
        serde_json::to_vec(draft).expect("serialize sensitive draft")
    };
    assert!(
        !encoded
            .windows(CODE.len())
            .any(|window| window == CODE.as_bytes())
    );
    assert!(
        !encoded
            .windows(STATE.len())
            .any(|window| window == STATE.as_bytes())
    );
    assert!(String::from_utf8(encoded).expect("JSON").contains(REDACTED));

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        format!(
            "::email auth google finish work http://127.0.0.1:54321/?code={CODE}&state={STATE}"
        ),
    );
    let escaped_encoded = {
        let (mtx, _cv) = &handle;
        let slot = super::super::locked(mtx);
        let (_, draft) = slot.pending.as_ref().expect("pending escaped draft");
        serde_json::to_vec(draft).expect("serialize escaped draft")
    };
    assert!(
        !escaped_encoded
            .windows(CODE.len())
            .any(|window| window == CODE.as_bytes())
    );
    assert!(
        !escaped_encoded
            .windows(STATE.len())
            .any(|window| window == STATE.as_bytes())
    );
    assert!(
        String::from_utf8(escaped_encoded)
            .expect("JSON")
            .contains(REDACTED)
    );

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        "  ordinary draft  ".to_owned(),
    );
    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (_, draft) = slot.pending.as_ref().expect("pending ordinary draft");
    assert_eq!(draft.text.as_deref(), Some("  ordinary draft  "));
}

/// Content-free prompt drafts must stay content-free even when the active
/// editor contains a recognizable Gmail OAuth finish buffer.
#[test]
fn content_free_prompt_drafts_do_not_add_gmail_redaction_text() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        ":email auth google finish account http://localhost/?code=secret".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (_, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(draft.text, None);
}

#[test]
fn first_agent_prompt_created_selects_new_agent_and_new_session_clears_it() {
    // Regression: the first prompt created for the default conversation carries
    // the new agent id; seeing it from the empty state selects that agent. A
    // later `:session new` returns to the empty start-new-agent state.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_abc12345"),
        ..agent_prompt_created("sp1", "s1")
    }));
    sync(&handle);
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("engineer_abc12345")
    );
    assert!(vt.screen_contains(80, "@engineer_abc12345"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );
}

#[test]
fn extension_prompt_with_target_does_not_select_from_empty_state() {
    // Regression: extension side prompts now carry target_agent_id for routing,
    // but `:agent none`/startup must stay on the no-agent screen until the user
    // explicitly selects a transcript.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..agent_prompt_created("worker-sp", "s1")
    }));

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response("worker-sp", vec![assistant_message_item("worker answer")])
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker answer"));
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("worker-1")
    );
}

#[test]
fn replayed_durable_first_user_prompt_selects_live_agent() {
    // Regression: cold replay skips transient AgentPromptCreated events. The
    // durable agent-owned prompt fact must still render the user message and
    // select a live agent so the next Enter press sends a targeted follow-up
    // instead of being rejected as "not live".
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "hello".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("engineer_abc12345")
    );
    assert!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .is_live("engineer_abc12345")
    );
    assert!(vt.screen_contains(80, "hello"));
}

/// Ensures each submitted-prompt projection preserves an explicit custom
/// `user.prompt` foreground instead of restoring the default bright white.
#[test]
fn custom_submitted_prompt_foreground_overrides_default_bright_white() {
    let theme = tau_themes::Theme::parse(r#"{ styles: { "user.prompt": { fg: "grey" } } }"#)
        .expect("custom prompt theme parses");
    let vt = render_submitted_prompt_projections(theme);

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_ansi_foreground(&vt, 100, text, 7);
    }
}

/// An extension-originated steered prompt without a queued user projection
/// renders as a message rather than a user prompt.
#[test]
fn extension_prompt_steered_uses_message_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::Extension {
            name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
        },
        agent_id: agent_id("engineer_abc12345"),
        text: "extension-steered prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    for (text, submission_source) in [
        (
            "legacy internal payload",
            tau_proto::PromptSubmissionSource::Legacy,
        ),
        (
            "human internal payload",
            tau_proto::PromptSubmissionSource::HumanUi,
        ),
    ] {
        renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source,
            display_name: None,
            ctx_id: None,
        }));
    }
    sync(&handle);

    assert!(vt.screen_contains(100, "■ External `fixture` message:"));
    assert!(vt.screen_contains(100, "extension-steered prompt"));
    assert!(!vt.screen_contains(100, "⬤ extension-steered prompt"));
}

/// Internal prompt facts use authenticated source rather than payload class:
/// extensions are always attributed messages, while typed harness prompts
/// reproject in place through the default-off diagnostic toggle.
#[test]
fn source_aware_internal_prompt_projection_and_toggle_are_exactly_once() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let extension = tau_proto::PromptSubmissionSource::Extension {
        name: tau_proto::ExtensionName::parse("std-swarm").expect("valid extension name"),
    };
    let harness = tau_proto::PromptSubmissionSource::HarnessInternal;

    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "extension submitted payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: extension.clone(),
        display_name: None,
        ctx_id: Some("swarm-command-1".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: extension,
        agent_id: agent_id("engineer_abc12345"),
        text: "extension steered payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("swarm-command-2".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness submitted payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: harness.clone(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: harness,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness steered payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "External `std-swarm` message:"));
    assert!(vt.screen_contains(100, "extension submitted payload"));
    assert!(vt.screen_contains(100, "extension steered payload"));
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    assert!(!vt.screen_contains(100, "legacy internal payload"));
    assert!(!vt.screen_contains(100, "human internal payload"));

    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    let enabled = visible_lines(&vt, 100).join("\n");
    assert_eq!(enabled.matches("harness submitted payload").count(), 1);
    assert_eq!(enabled.matches("harness steered payload").count(), 1);
    assert_eq!(enabled.matches("extension submitted payload").count(), 1);
    assert_eq!(enabled.matches("extension steered payload").count(), 1);
    assert!(!enabled.contains("legacy internal payload"));
    assert!(!enabled.contains("human internal payload"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "harness submitted payload"));
    assert!(vt.screen_contains(100, "harness steered payload"));

    renderer.apply_setting("show-internal-prompts", "off");
    sync(&handle);
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    assert!(vt.screen_contains(100, "extension submitted payload"));
    assert!(vt.screen_contains(100, "extension steered payload"));
}

/// A new session must discard hidden prompt slots before block identifiers are
/// reused, so enabling diagnostics cannot disclose prior-session prompt text.
#[test]
fn internal_prompt_toggle_does_not_reproject_previous_session_history() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let internal = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            display_name: None,
            ctx_id: None,
        })
    };

    renderer.handle(&internal("session one hidden prompt"));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&internal("session two hidden prompt"));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    assert!(!vt.screen_contains(100, "session one hidden prompt"));
    assert!(vt.screen_contains(100, "session two hidden prompt"));
}

/// Timer and context-alert presentation own their canonical prompt facts before
/// the diagnostic toggle, so enabling it cannot append generic notice blocks.
#[test]
fn internal_prompt_toggle_preserves_timer_and_context_alert_special_presentation() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `special` fired: exact once".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: Some("timer:special:1".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "context alert exact once".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        ctx_id: None,
    }));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    let lines = visible_lines(&vt, 100).join("\n");
    assert_eq!(
        lines
            .matches("Timer `special` woke this agent: exact once")
            .count(),
        1
    );
    assert_eq!(lines.matches("context alert exact once").count(), 1);
    assert!(!lines.contains("[tau-internal]: Timer `special` fired: exact once"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "Timer `special` woke this agent"));
    assert!(!vt.screen_contains(100, "context alert exact once"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "Timer `special` woke this agent"));
    assert!(vt.screen_contains(100, "context alert exact once"));
}

/// Replayed Submitted and Steered facts retain their per-agent source-aware
/// slots across snapshot switches, so repeated toggles restore each once.
#[test]
fn replayed_source_aware_prompt_slots_survive_agent_snapshot_switches() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.switch_agent("replayed-agent".to_owned());
    let submitted = Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("replayed-agent"),
        text: "replayed submitted internal".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    let steered = Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("replayed-agent"),
        text: "replayed steered internal".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    });
    renderer.handle(&submitted);
    renderer.handle(&steered);
    renderer.switch_agent("other-agent".to_owned());
    renderer.switch_agent("replayed-agent".to_owned());
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    let enabled = visible_lines(&vt, 100).join("\n");
    assert_eq!(enabled.matches("replayed submitted internal").count(), 1);
    assert_eq!(enabled.matches("replayed steered internal").count(), 1);
    assert!(
        enabled.find("replayed submitted internal") < enabled.find("replayed steered internal")
    );

    renderer.apply_setting("show-internal-prompts", "off");
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    let retoggled = visible_lines(&vt, 100).join("\n");
    assert_eq!(retoggled.matches("replayed submitted internal").count(), 1);
    assert_eq!(retoggled.matches("replayed steered internal").count(), 1);
}

/// Typed harness provenance remains hidden by default even for a user-class
/// legacy-shaped fact, then the diagnostic toggle reveals it as a notice.
#[test]
fn unqueued_harness_prompt_steered_uses_toggle_controlled_notice() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness-steered message".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "harness-steered message"));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    assert!(vt.screen_contains(100, "□ harness-steered message"));
    assert!(!vt.screen_contains(100, "⬤ harness-steered message"));
}

/// Queued prompts remain pending user input and must therefore use the same
/// hollow marker as the currently composed prompt, not the submitted marker.
#[test]
fn queued_prompt_uses_composing_marker() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued marker check".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "◯ queued marker check (queued)"));
    assert!(!vt.screen_contains(80, "⬤ queued marker check (queued)"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "queued marker check".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "⬤ queued marker check"));
    assert!(!vt.screen_contains(80, "◯ queued marker check"));
}

/// A failure routed to a hidden agent must not consume the visible agent's
/// queued marker.
#[test]
fn hidden_agent_prompt_failure_preserves_visible_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "visible queued".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "hidden-create".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("hidden"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hidden queued".into(),
        agent_id: agent_id("hidden"),
        message_class: tau_proto::PromptMessageClass::User,
    }));

    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "hidden-create".into(),
        agent_id: agent_id("hidden"),
        ctx_id: "hidden-ctx".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "hidden failed".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "visible queued (queued)"));
    assert!(!vt.screen_contains(100, "hidden queued (queued)"));
}

/// A non-requesting attached UI receives only the broadcast queued/failure
/// lifecycle and must still remove the failed initial marker.
#[test]
fn broadcast_only_initial_prompt_failure_removes_queue_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "initial from another UI".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "other-ui-create".into(),
        agent_id: agent_id("main"),
        ctx_id: "other-ui-ctx".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "initial from another UI (queued)"));
    assert!(vt.screen_contains(100, "initial failed"));
}

/// A late failure for an initial prompt already promoted into submitted history
/// must not consume a newer ordinary queued marker.
#[test]
fn submitted_initial_prompt_failure_preserves_newer_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-a".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "initial A".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "initial A".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: Some("ctx-a".into()),
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-a".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-a".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed late".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// An internal initial prompt owns no visible queue block, so its terminal
/// cannot consume a later visible user marker.
#[test]
fn internal_initial_prompt_failure_preserves_visible_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-internal".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "internal A".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::Internal,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-internal".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-internal".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "internal failed".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// A multiline queued prompt occupies two rows while steering still promotes
/// the complete original text rather than the bounded presentation windows.
#[test]
fn queued_prompt_elides_at_layout_without_changing_authoritative_text() {
    let (_term, handle, vt) = setup(32, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let text =
        "First line with discarded tail\nmiddle line retained\nforgotten start end of last line.";

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: text.into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    let queued = vt.screen_text(32).join("\n");
    assert!(queued.contains("◯ First line with discarded"));
    assert!(queued.contains("┄"));
    assert!(queued.contains("last line. (queued)"));
    assert!(!queued.contains("middle line retained"));

    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        text: text.into(),
        trusted_internal_spans: Vec::new(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(32, "middle line retained"));
}

/// Ensures the external prompt editor trailer is seeded from the visible
/// agent's response history, not from the most recent hidden agent response
/// processed by the renderer. It also preserves prompt-local editor fields that
/// are shared with the active input draft rather than with hidden transcripts.
#[test]
fn hidden_agent_response_does_not_replace_visible_editor_context() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "worker one response",
        ),
    ));
    let (visible_response_pointer, visible_copy_bytes) = {
        let editor_context = renderer.editor_context();
        let editor_context = editor_context.lock().expect("editor context");
        (
            editor_context
                .last_response
                .as_ref()
                .expect("visible response")
                .as_ptr(),
            renderer.editor_response_copy_bytes_for_test(),
        )
    };
    {
        let editor_context = renderer.editor_context();
        let mut editor_context = editor_context.lock().expect("editor context");
        editor_context.previous_prompt = Some("visible previous prompt".to_owned());
        editor_context.edited_trailer_recovery = Some("visible recovery".to_owned());
    }

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-0",
            "worker-2",
            20_000,
            0,
            0,
            "worker two response",
        ),
    ));

    let visible_context = renderer.editor_context();
    let visible_context = visible_context.lock().expect("editor context");
    assert_eq!(
        visible_context.last_response.as_deref(),
        Some("worker one response")
    );
    assert_eq!(visible_context.current_response, None);
    assert_eq!(
        visible_context.previous_prompt.as_deref(),
        Some("visible previous prompt")
    );
    assert_eq!(
        visible_context.edited_trailer_recovery.as_deref(),
        Some("visible recovery")
    );
    assert_eq!(
        visible_context
            .last_response
            .as_ref()
            .expect("visible response")
            .as_ptr(),
        visible_response_pointer,
        "a hidden fold must retain the visible editor response allocation"
    );
    drop(visible_context);
    assert_eq!(
        renderer.editor_response_copy_bytes_for_test(),
        visible_copy_bytes,
        "an unchanged visible editor context must not be republished"
    );

    {
        let editor_context = renderer.editor_context();
        editor_context.lock().expect("editor context").last_response =
            Some("externally changed response".to_owned());
    }
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-3-sp-0",
            "worker-3",
            20_000,
            0,
            0,
            "worker three response",
        ),
    ));
    let editor_context = renderer.editor_context();
    let editor_context = editor_context.lock().expect("editor context");
    assert_eq!(
        editor_context.last_response.as_deref(),
        Some("worker one response"),
        "a changed observer-visible response must be restored after hidden folding"
    );
    drop(editor_context);
    let visible_republish_bytes = visible_copy_bytes + "worker one response".len() as u64;
    assert_eq!(
        renderer.editor_response_copy_bytes_for_test(),
        visible_republish_bytes,
        "the hidden fold must republish only when its visible editor response changed"
    );

    renderer.switch_agent("worker-2".to_owned());

    let worker_two_context = renderer.editor_context();
    let worker_two_context = worker_two_context.lock().expect("editor context");
    assert_eq!(
        worker_two_context.last_response.as_deref(),
        Some("worker two response")
    );
    assert_eq!(
        worker_two_context.previous_prompt.as_deref(),
        Some("visible previous prompt")
    );
    assert_eq!(
        worker_two_context.edited_trailer_recovery.as_deref(),
        Some("visible recovery")
    );
    drop(worker_two_context);
    assert_eq!(
        renderer.editor_response_copy_bytes_for_test(),
        visible_republish_bytes + "worker two response".len() as u64,
        "selecting a changed transcript must publish its response context"
    );
}

/// Ensures the no-agent editor prompt context is not seeded with the last
/// selected agent's response and remains isolated from later hidden responses
/// owned by that old agent.
#[test]
fn clearing_selected_agent_clears_response_editor_context() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "worker one response",
        ),
    ));

    renderer.clear_selected_agent();
    let no_agent_context = renderer.editor_context();
    let no_agent_context = no_agent_context.lock().expect("editor context").clone();
    assert_eq!(no_agent_context.current_response, None);
    assert_eq!(no_agent_context.last_response, None);

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-1",
            "worker-1",
            20_000,
            0,
            0,
            "later hidden worker response",
        ),
    ));

    let no_agent_context = renderer.editor_context();
    let no_agent_context = no_agent_context.lock().expect("editor context").clone();
    assert_eq!(no_agent_context.current_response, None);
    assert_eq!(no_agent_context.last_response, None);
}

/// Skill statistics describe only the advertised description injected into the
/// initial context, including UTF-8 bytes and multiple description lines.
#[test]
fn agent_context_initialization_skill_stats_measure_prompt_description() {
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: vec![
            tau_proto::DiscoveryEffectiveSkill {
                name: "focused".into(),
                description: "one\né".to_owned(),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            },
            tau_proto::DiscoveryEffectiveSkill {
                name: "empty".into(),
                description: String::new(),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            },
        ],
        agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
            file_path: "/empty/AGENTS.md".into(),
            lines: 0,
            bytes: 0,
        }],
    };

    let text =
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, 0)
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();

    assert_eq!(
        text,
        "▤ initialized agent-1\nskills:\n  focused 2L, 6B\n  empty 0L, 0B\nAGENTS.md:\n  /empty/AGENTS.md 0L, 0B"
    );
}

/// Extension lifecycle completion must update the same snapshot that received
/// the starting block. Otherwise switching the viewed agent between
/// `extension.starting` and `extension.ready` leaves a stale starting line in
/// the old transcript and prints the ready line in an unrelated one.
#[test]
fn extension_lifecycle_completion_routes_to_starting_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent-a".to_owned());
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 7.into(),
        extension_name: tau_proto::ExtensionName::parse("std-test")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-test starting"));

    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 7.into(),
        extension_name: tau_proto::ExtensionName::parse("std-test")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-test ready"));
    assert!(!vt.screen_contains(80, "extension std-test starting"));

    renderer.switch_agent("agent-a".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-test ready"));
    assert!(!vt.screen_contains(80, "extension std-test starting"));
}

/// Removing a visible starting block must request a redraw even when the
/// matching completion is filtered by the current notice level. Without this,
/// the stale starting line stays on screen until some unrelated redraw happens.
#[test]
fn extension_lifecycle_removal_redraws_when_completion_is_filtered() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 9.into(),
        extension_name: tau_proto::ExtensionName::parse("std-filtered")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(789),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-filtered starting"));

    renderer.apply_setting("notice-level", "warning");
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 9.into(),
        extension_name: tau_proto::ExtensionName::parse("std-filtered")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(789),
    }));

    assert!(eventually_screen_lacks(
        &vt,
        80,
        "extension std-filtered starting"
    ));
    assert!(!vt.screen_contains(80, "extension std-filtered ready"));
}

/// An accepted inference-activating user prompt must expose main-turn activity
/// before any prompt-id or provider event exists, so local provider warm-up
/// cannot leave the UI looking idle.
#[test]
fn accepted_prompt_submission_starts_main_turn_before_provider_activity() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "warm up locally".to_owned(),
        agent_id: agent_id("local-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    assert!(!renderer.main_agent_turn_active_for_test());
    sync(&handle);
    assert!(!vt.screen_contains(80, "◇ …"));

    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("local-agent"),
        text: "warm up locally".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));

    assert!(renderer.main_agent_turn_active_for_test());
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "◇ …"),
        "screen: {:?}",
        vt.screen_text(80)
    );

    renderer.handle_disconnect(Some("provider socket closed".to_owned()));
    sync(&handle);
    assert!(!vt.screen_contains(80, "◇ …"));
    assert!(vt.screen_contains(80, "provider socket closed"));
    assert!(!renderer.main_agent_turn_active_for_test());
}

#[test]
fn replay_learns_side_agent_from_durable_agent_prompt_submission() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("worker-1"),
            text: "side task".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: originator.clone(),
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response(
            "worker-sp",
            vec![assistant_message_item("worker replay answer")],
        )
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker replay answer"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker replay answer"));
    assert!(!vt.screen_contains(80, "&q-worker"));
}

#[test]
fn queued_prompt_from_old_agent_does_not_steal_no_agent_selection() {
    // Regression: after `:agent new`, an already-running agent can still emit
    // queued/dequeued prompt events. Those background events must not reselect
    // the old agent while the user is typing the prompt meant to create a fresh
    // agent.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old agent prompt"));

    renderer.clear_selected_agent();
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued old-agent prompt".into(),
        agent_id: tau_proto::AgentId::parse("old-agent").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "stale old-agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("old-agent").expect("agent id"),
        ..agent_prompt_created("old-sp", "s1")
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "queued old-agent prompt"));
    assert!(!vt.screen_contains(80, "stale old-agent prompt"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("new-agent").expect("agent id"),
        ..agent_prompt_created("new-sp", "s1")
    }));
    sync(&handle);
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some("new-agent".to_owned())
    );
}

#[test]
fn queued_prompt_selects_agent_from_empty_state() {
    // Regression: replay can start with an already-queued user prompt. The UI
    // should treat that prompt as selecting the live agent, otherwise the next
    // Enter from the empty screen would create a new agent instead of targeting
    // the active one.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued live-agent prompt".into(),
        agent_id: tau_proto::AgentId::parse("live-agent").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "queued live-agent prompt"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some("live-agent".to_owned())
    );
}

/// A submission epoch bump must suppress a snapshot already taken by the
/// debounce worker so cleared or submitted prompt text cannot arrive late.
#[test]
fn stale_draft_snapshot_is_dropped_after_submit_epoch_bump() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("s1"),
                target_agent_id: None,
                text: Some("old".into()),
            },
        ));
    }

    let (epoch, _draft) = {
        let (mtx, _cv) = &handle;
        super::super::locked(mtx)
            .pending
            .take()
            .expect("pending draft")
    };
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::super::locked(mtx);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.pending = None;
    }

    assert!(!should_send_draft_snapshot(&handle, epoch));
}

/// Action submission uses the same epoch invalidation as ordinary prompt
/// submission so a pending command draft cannot publish after it runs.
#[test]
fn action_submission_invalidates_pending_draft_like_prompt_submission() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("s1"),
                target_agent_id: None,
                text: Some(":email list".into()),
            },
        ));
    }

    invalidate_pending_draft(&handle);

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    assert_eq!(slot.epoch, 1);
    assert!(slot.pending.is_none());
}

/// An explicitly content-enabled queued draft preserves the selected agent and
/// full buffer so subscribers can associate the opt-in text with its
/// transcript.
#[test]
fn queued_draft_snapshot_records_selected_agent_target() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_id = tau_proto::AgentId::parse("agent-a").expect("agent id");

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_id.clone()),
        "draft for agent".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, test_session_id("s1"));
    assert_eq!(draft.target_agent_id, Some(agent_id));
    assert_eq!(draft.text.as_deref(), Some("draft for agent"));
}

/// A default queued draft retains liveness and target metadata while omitting
/// the buffer so normal editing cannot expose prompt content to subscribers.
#[test]
fn queued_draft_snapshot_records_no_agent_target() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        "new agent draft".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, test_session_id("s1"));
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text, None);
}

/// Content-enabled retargeting invalidates the stale snapshot and preserves the
/// full buffer under the replacement viewed agent target.
#[test]
fn retarget_draft_snapshot_replaces_agent_a_with_agent_b() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    let agent_b = tau_proto::AgentId::parse("agent-b").expect("agent id");
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_a),
        "draft".to_owned(),
    );

    retarget_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_b.clone()),
        "draft".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, Some(agent_b));
    assert_eq!(draft.text.as_deref(), Some("draft"));
}

/// Content-enabled retargeting back to the new-agent prompt keeps its
/// replacement snapshot explicitly unscoped and contentful.
#[test]
fn retarget_draft_snapshot_replaces_agent_with_no_agent() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_a),
        "draft".to_owned(),
    );

    retarget_prompt_draft_snapshot(&handle, test_session_id("s1"), None, "draft".to_owned());

    let (mtx, _cv) = &handle;
    let slot = super::super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text.as_deref(), Some("draft"));
}

/// A newly created draft epoch is eligible for the debounce worker until a
/// submission, retarget, or shutdown invalidates that exact snapshot.
#[test]
fn current_draft_snapshot_is_sent_when_epoch_matches() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );

    assert!(should_send_draft_snapshot(&handle, 0));
}

/// Shutdown makes even a current pending draft ineligible so the worker cannot
/// write an event after the UI has begun disconnecting.
#[test]
fn draft_snapshot_is_dropped_after_shutdown() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        super::super::locked(mtx).done = true;
    }

    assert!(!should_send_draft_snapshot(&handle, 0));
}

/// Prompt submission must win when it invalidates a draft after the worker's
/// initial epoch check but before writer acquisition; otherwise the old prompt
/// text can be published after the submission that cleared it.
#[test]
fn prompt_submission_suppresses_draft_validated_before_writer_acquisition() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let handle = Arc::new((
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    ));
    let draft = tau_proto::UiPromptDraft {
        session_id: test_session_id("s1"),
        target_agent_id: None,
        text: Some("stale".to_owned()),
    };
    let worker_writer = writer.clone();
    let worker_handle = handle.clone();
    let (validated_tx, validated_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        send_draft_snapshot_with_before_writer(
            &worker_writer,
            worker_handle.as_ref(),
            0,
            draft,
            || {
                validated_tx
                    .send(())
                    .expect("announce initial draft validation");
                continue_rx
                    .recv()
                    .expect("wait for invalidation and submission");
            },
        )
        .expect("draft send decision")
    });
    validated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker must pause after initial epoch validation");

    invalidate_pending_draft(handle.as_ref());
    let submitted = Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "submitted".to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    send_event(&writer, &submitted).expect("send invalidating prompt");
    continue_tx.send(()).expect("release draft worker");

    assert!(!worker.join().expect("join draft worker"));
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert!(matches!(
        reader.read_message().expect("read prompt submission"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if emit.event.as_ref() == &submitted
    ));
}

/// The draft worker must send the first snapshot without waiting, then retain
/// only the latest edit until the next coalescing boundary instead of emitting
/// every queued buffer.
#[test]
fn draft_debounce_sends_immediately_then_coalesces_latest_snapshot() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let handle = Arc::new((
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    ));
    let worker_handle = handle.clone();
    let (boundary_tx, boundary_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        let mut first_boundary = true;
        debounce_loop_with_wait(worker_handle, writer, move |_| {
            if !first_boundary {
                return false;
            }
            first_boundary = false;
            boundary_tx.send(()).expect("first send reached boundary");
            continue_rx.recv().expect("advance coalescing boundary");
            true
        });
    });
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));

    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "first".to_owned(),
    );
    boundary_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first draft must send before the coalescing boundary");
    assert!(matches!(
        reader.read_message().expect("read immediate draft"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if matches!(
                emit.event.as_ref(),
                Event::UiPromptDraft(draft) if draft.text.as_deref() == Some("first")
            )
    ));

    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "intermediate".to_owned(),
    );
    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "latest".to_owned(),
    );
    continue_tx.send(()).expect("release coalescing boundary");
    assert!(matches!(
        reader.read_message().expect("read coalesced draft"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if matches!(
                emit.event.as_ref(),
                Event::UiPromptDraft(draft) if draft.text.as_deref() == Some("latest")
            )
    ));
    worker.join().expect("join draft worker");
}

#[test]
fn agent_in_progress_ignores_completed_replayed_prompt_history() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // Late subscribers can replay historical UI submit and provider-finished
    // events without replaying the old AgentPromptCreated. That sequence is
    // already complete, so it must not leave Ctrl-D permanently guarded.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "old-sp",
        vec![assistant_message_item("old answer")],
    )));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn prompt_termination_clears_live_response_and_activity() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-stale", "s1",
    )));
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    // Regression: if the harness discards a stale provider response, it now
    // publishes this terminal lifecycle fact instead of leaving the UI's live
    // response block and Ctrl-D guard stuck forever.
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_prompt_id: test_agent_prompt_id("sp-stale"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        reason: AgentPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!vt.screen_contains(80, "…"));
}

/// Ensures the no-agent fallback still accepts stats for a visible prompt it
/// already owns, while rejecting unrelated provider response stats.
///
/// A late provider update can create live response state before the UI has
/// selected or displayed an agent. The stats guard must preserve that supported
/// adoptable transcript path without letting other agents' stats leak into it.
#[test]
fn no_agent_visible_prompt_accepts_only_matching_response_stats() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("ap-agent_a-0"),
        agent_id: agent_id("agent_a"),
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_a"),
            4 * 1024,
            0,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "matching stats should update the visible no-agent prompt: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_b"),
            12 * 1024,
            4 * 1024,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "unrelated stats should leave the visible no-agent prompt unchanged: {:?}",
        vt.screen_text(80)
    );
    assert!(
        !vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "unrelated provider response stats must not render in the visible no-agent transcript: {:?}",
        vt.screen_text(80)
    );
}

/// Role availability should feed `:new` argument completion as well as
/// `:role`, because `:new <role>` is the fast path for opening a fresh
/// no-agent input target that will create the next agent with that role.
#[test]
fn new_command_completes_available_roles() {
    let (_term, handle, _vt) = setup(80, 24);
    let completion_data = tau_cli_term::CompletionData::new();
    let mut renderer = EventRenderer::new(handle, completion_data.clone(), cli_test_theme());

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![
            HarnessRoleInfo {
                name: "engineer".to_owned(),
                description: "write production code".to_owned(),
                role_description: None,
                details: None,
            },
            HarnessRoleInfo {
                name: "reviewer".to_owned(),
                description: "review code changes".to_owned(),
                role_description: None,
                details: None,
            },
        ],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));

    let candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(":new", "new agent")],
        &completion_data,
        ":new rev",
        ":new rev".len(),
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].label, "reviewer");
    assert_eq!(candidates[0].replacement, ":new reviewer");
}

#[test]
fn single_prompt_response_cycle() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // User submits prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "> hello"));

    // Harness creates agent prompt.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    // Agent streams response.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hi there!",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hi there!"));

    // Agent finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hi there! How can I help?")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hi there! How can I help?"),
        "final response should be visible, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_then_late_ui_submit_advances_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: replay/late-subscribe paths can observe a queued event before
    // the matching UI submit. The submit must promote the queued marker to one
    // normal transcript item rather than leaving stale "(queued)" text behind.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "late echo".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "late echo".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "late echo (queued)"));
    assert!(vt.screen_contains(80, "> late echo"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("late echo"))
            .count(),
        1,
        "created queued prompt should be promoted once, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_steered_promotes_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: steering folds a queued prompt into the in-flight turn
    // immediately, without a later `AgentPromptCreated`. The queued
    // marker should therefore be promoted in place to one normal user
    // prompt instead of lingering or duplicating.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "folded queued prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should show before steering, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        text: "folded queued prompt".into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should be gone after steering, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> folded queued prompt"),
        "steered prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("folded queued prompt"))
            .count(),
        1,
        "steered queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );
}

/// Extension provenance wins over an ambiguous queue-text match, preserving the
/// queued user projection and rendering the extension's canonical fact once.
#[test]
fn extension_steering_does_not_promote_matching_queued_user_prompt() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued extension collision".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::Extension {
            name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
        },
        text: "queued extension collision".into(),
        trusted_internal_spans: Vec::new(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "queued extension collision (queued)"));
    assert!(vt.screen_contains(80, "■ External `fixture` message:"));
    assert!(!vt.screen_contains(80, "⬤ queued extension collision"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("queued extension collision"))
            .count(),
        2,
        "queue and authenticated extension projections must remain distinct: {:?}",
        vt.screen_text(80)
    );
}

/// An extension message cannot consume a queued user prompt merely because its
/// payload matches a later queued row.
#[test]
fn nonfront_queued_match_remains_a_message_without_consuming_the_front_prompt() {
    for submission_source in [tau_proto::PromptSubmissionSource::Extension {
        name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
    }] {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = marker_test_renderer(handle.clone());
        for text in ["first queued user prompt", "second queued user prompt"] {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: text.to_owned(),
                agent_id: agent_id("main"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
        renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: false,
            submission_source,
            text: "second queued user prompt".to_owned(),
            trusted_internal_spans: Vec::new(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            ctx_id: None,
        }));
        sync(&handle);

        assert!(vt.screen_contains(80, "◯ first queued user prompt (queued)"));
        assert!(!vt.screen_contains(80, "⬤ first queued user prompt"));
        assert!(vt.screen_contains(80, "■ External `fixture` message:"));
        assert!(!vt.screen_contains(80, "⬤ second queued user prompt"));
    }
}

/// Human UI provenance promotes its front-exact queued projection before the
/// subsequent start event can duplicate it.
#[test]
fn submitted_human_prompt_promotes_matching_front_queue_before_start() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let text = "accepted queued prompt";
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: text.to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "accepted-queued",
        "s1",
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "⬤ accepted queued prompt"));
    assert!(!vt.screen_contains(80, "■ accepted queued prompt"));
    assert!(!vt.screen_contains(80, "accepted queued prompt (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains(text))
            .count(),
        1,
        "submitted queued prompt must render once: {:?}",
        vt.screen_text(80)
    );
}

/// Harness-typed active and passive background completion notices stay out of
/// the terminal even when the operator enables other internal prompt
/// diagnostics.
#[test]
fn typed_background_completion_prompts_are_always_hidden() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "Tool call `idle` completed. Its result is queued; use `wait` to consume it.".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.apply_setting("show-internal-prompts", "off");
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: "Tool call `steered` completed. Its result is queued; use `wait` to consume it."
            .into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        ctx_id: None,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "Tool call `idle`"));
    assert!(!vt.screen_contains(80, "Tool call `steered`"));
    assert!(
        vt.screen_text(80)
            .iter()
            .all(|row| !row.contains("Tool call"))
    );

    let (_cold_term, cold_handle, cold_vt) = setup(80, 24);
    let mut cold = EventRenderer::new(
        cold_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    cold.apply_setting("show-internal-prompts", "on");
    cold.handle_recorded_at(
        &Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("main"),
            text: "Tool call `replayed` completed. Its result is queued; use `wait` to consume it."
                .into(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            display_name: None,
            ctx_id: None,
        }),
        tau_proto::UnixMicros::new(1),
    );
    sync(&cold_handle);
    assert!(!cold_vt.screen_contains(80, "Tool call `replayed`"));
}

/// UI suppression relies on authenticated typed provenance rather than prose:
/// an ordinary harness-internal prompt with identical text remains available
/// when internal prompt diagnostics are enabled.
#[test]
fn untyped_internal_prompt_matching_completion_prose_remains_visible() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-internal-prompts", "on");

    let mut prompt = AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: "Tool call `same-prose` completed. Its result is queued; use `wait` to consume it."
            .into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        ctx_id: None,
    };
    prompt.internal_kind = None;
    renderer.handle(&Event::AgentPromptSteered(prompt));
    sync(&handle);

    assert!(vt.screen_contains(80, "□ Tool call `same-prose` completed."));
}

#[test]
fn queued_prompt_does_not_replace_dispatched_same_text() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: once a local echo has been accepted as a normal prompt,
    // a later queued prompt with the same text is a separate message. Do
    // not remove the earlier transcript block while rendering the queued
    // marker.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "repeat".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "repeat".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "repeat (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("repeat"))
            .count(),
        2,
        "queued prompt should not remove an earlier dispatched prompt with the same text, got: {:?}",
        vt.screen_text(80)
    );
}
