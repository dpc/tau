use std::io::Cursor;
use std::time as path_std_time;

use tau_proto::{AgentPromptCreated, MessageItem, PromptOriginator, ProviderStopReason};

use super::*;

const HOSTILE_TEXT: &str = "A\x1b[31mB\x1b[0mC\x1b]52;c;WA==\x07D\u{009B}31mE";
const SAFE_TEXT: &str = "ABCDE";

#[test]
fn prompt_stdin_role_uses_startup_role_or_default() {
    assert_eq!(prompt_stdin_role(Some("specialist")), "specialist");
    assert_eq!(prompt_stdin_role(None), DEFAULT_AGENT_ROLE);
}

/// `--prompt-stdin` must mark every initial prompt literal so colon-prefixed
/// input reaches the created agent instead of entering command dispatch.
#[test]
fn prompt_stdin_submission_marks_colon_input_literal() {
    let request = create_user_agent_prompt(
        &tau_proto::SessionId::parse("s1").expect("session id"),
        "engineer",
        ":skill",
        CreateUserAgentPromptOptions {
            command_handling: PromptCommandHandling::LiteralEscape,
            ..CreateUserAgentPromptOptions::default()
        },
    );

    assert_eq!(request.initial_prompt.as_deref(), Some(":skill"));
    assert!(request.literal);
}

fn create_result(
    request_id: &str,
    outcome: tau_proto::UiCreateAgentOutcome,
) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::UiCreateAgentResult(tau_proto::UiCreateAgentResult {
        request_id: request_id.to_owned(),
        session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
        outcome,
    }))
}

fn prompt_created(agent_id: &str, prompt_id: &str, ctx_id: &str) -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: prompt_id.parse().expect("prompt id"),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
        system_prompt: String::new(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: Some(ctx_id.to_owned()),
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

/// A matching create rejection must terminate admission immediately while an
/// unrelated result remains unable to satisfy the correlation.
#[test]
fn prompt_stdin_admission_reports_matching_rejection() {
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(Some(create_result(
        "unrelated",
        tau_proto::UiCreateAgentOutcome::Created {
            agent_id: tau_proto::AgentId::parse("other-agent").expect("agent id"),
            initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
        },
    ))))
    .expect("unrelated result");
    tx.send(Ok(Some(create_result(
        "wanted",
        tau_proto::UiCreateAgentOutcome::Rejected {
            reason: tau_proto::UiCreateAgentRejection::RoleUnavailable,
            message: "unknown role `missing`".to_owned(),
            agent_id: None,
        },
    ))))
    .expect("matching result");

    let error = wait_for_create_agent_admission_until(
        &rx,
        "wanted",
        "wanted-prompt",
        path_std_time::Instant::now() + Duration::from_secs(1),
    )
    .expect_err("rejection must fail");
    assert_eq!(
        error.to_string(),
        "create-agent request failed (role_unavailable): unknown role `missing`"
    );
}

/// The admission deadline is independent of provider execution and can expire
/// deterministically without sleeping when no result arrives.
#[test]
fn prompt_stdin_admission_timeout_is_bounded() {
    let (_tx, rx) = mpsc::channel();
    let error = wait_for_create_agent_admission_until(
        &rx,
        "wanted",
        "wanted-prompt",
        path_std_time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("past deadline"),
    )
    .expect_err("elapsed admission deadline");
    assert_eq!(
        error.to_string(),
        "timed out after 10s waiting for create-agent admission"
    );
}

/// Acceptance ends only the admission phase; this helper does not impose a
/// deadline on the later provider-response receiver.
#[test]
fn prompt_stdin_admission_accepts_created_result() {
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(Some(create_result(
        "wanted",
        tau_proto::UiCreateAgentOutcome::Created {
            agent_id: tau_proto::AgentId::parse("created-agent").expect("agent id"),
            initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
        },
    ))))
    .expect("created result");
    wait_for_create_agent_admission_until(
        &rx,
        "wanted",
        "wanted-prompt",
        path_std_time::Instant::now() + Duration::from_secs(1),
    )
    .expect("created admission");
}

/// A foreign agent that reuses the prompt correlation before admission cannot
/// bind the created agent's provider-chain floor.
#[test]
fn prompt_stdin_admission_rejects_foreign_same_ctx_binding() {
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(Some(HarnessOutputMessage::deliver(
        Event::AgentPromptCreated(prompt_created("other", "ap-other-9", "prompt-1")),
    ))))
    .expect("foreign prompt");
    tx.send(Ok(Some(create_result(
        "request-1",
        tau_proto::UiCreateAgentOutcome::Created {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
        },
    ))))
    .expect("created result");

    let admission = wait_for_create_agent_admission_until(
        &rx,
        "request-1",
        "prompt-1",
        path_std_time::Instant::now() + Duration::from_secs(1),
    )
    .expect("created admission");
    assert_eq!(admission.initial_prompt_index, None);
}

/// After admission, prompt-stdin binds the first matching created-agent prompt
/// exactly once and ignores same-ctx prompts from foreign agents.
#[test]
fn prompt_stdin_binds_owned_same_ctx_prompt_once() {
    let mut output = OneShotOutput {
        agent_id: Some(tau_proto::AgentId::parse("main").expect("agent id")),
        ctx_id: Some("prompt-1".to_owned()),
        ..OneShotOutput::default()
    };
    for prompt in [
        prompt_created("other", "ap-other-8", "prompt-1"),
        prompt_created("main", "ap-main-2", "prompt-1"),
        prompt_created("main", "ap-main-7", "prompt-1"),
    ] {
        handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::AgentPromptCreated(prompt)),
            &mut output,
        )
        .expect("created prompt");
    }
    assert_eq!(output.initial_prompt_index, Some(2));
}

/// A user-originated terminal response from another agent cannot complete an
/// admitted prompt-stdin invocation on a busy shared daemon.
#[test]
fn prompt_stdin_ignores_unrelated_agent_after_admission() {
    let mut output = OneShotOutput {
        agent_id: Some(tau_proto::AgentId::parse("main").expect("agent id")),
        ..OneShotOutput::default()
    };
    let mut unrelated =
        assistant_finished("ap-other-1", "foreign answer", ProviderStopReason::EndTurn);
    unrelated.agent_id = tau_proto::AgentId::parse("other").expect("agent id");

    assert!(
        !handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::ProviderResponseFinished(unrelated)),
            &mut output,
        )
        .expect("unrelated response")
    );
    assert!(output.final_response.is_none());
    assert!(
        handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::ProviderResponseFinished(assistant_finished(
                "ap-main-1",
                "owned answer",
                ProviderStopReason::EndTurn,
            ))),
            &mut output,
        )
        .expect("owned response")
    );
    assert_eq!(output.final_response.as_deref(), Some("owned answer"));
}

/// A pre-materialization failure must match all three create, agent, and prompt
/// identities before it can terminate the one-shot invocation.
#[test]
fn prompt_stdin_accepts_only_fully_correlated_prompt_failure() {
    let mut output = OneShotOutput {
        request_id: Some("request-1".to_owned()),
        agent_id: Some(tau_proto::AgentId::parse("main").expect("agent id")),
        ctx_id: Some("prompt-1".to_owned()),
        ..OneShotOutput::default()
    };
    let failed = tau_proto::AgentPromptFailed {
        request_id: "request-1".to_owned(),
        agent_id: tau_proto::AgentId::parse("other").expect("agent id"),
        ctx_id: "prompt-1".to_owned(),
        stage: tau_proto::AgentPromptFailureStage::Preprocessing,
        message: "failed to load skill".to_owned(),
    };
    assert!(
        !handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::AgentPromptFailed(failed.clone())),
            &mut output,
        )
        .expect("foreign failure must be ignored")
    );

    let error = handle_prompt_stdin_message(
        HarnessOutputMessage::deliver(Event::AgentPromptFailed(tau_proto::AgentPromptFailed {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            ..failed
        })),
        &mut output,
    )
    .expect_err("matching failure must terminate");
    assert_eq!(
        error.to_string(),
        "initial prompt failed (preprocessing): failed to load skill"
    );
}

/// An owned unsuccessful provider terminal must fail the invocation instead of
/// printing partial output and exiting successfully.
#[test]
fn prompt_stdin_reports_owned_provider_failure() {
    let mut output = OneShotOutput {
        agent_id: Some(tau_proto::AgentId::parse("main").expect("agent id")),
        ..OneShotOutput::default()
    };
    let mut finished = assistant_finished("ap-main-1", "partial", ProviderStopReason::Error);
    finished.error = Some("provider unavailable".to_owned());

    let error = handle_prompt_stdin_message(
        HarnessOutputMessage::deliver(Event::ProviderResponseFinished(finished)),
        &mut output,
    )
    .expect_err("owned provider error must fail");
    assert_eq!(
        error.to_string(),
        "initial prompt failed (execution): provider unavailable"
    );
    assert!(output.final_response.is_none());
}

fn user_update(spid: &str, text: &str, thinking: Option<&str>) -> ProviderResponseUpdated {
    let mut deltas = Vec::new();
    if let Some(thinking) = thinking.filter(|thinking| !thinking.is_empty()) {
        deltas.push(tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 0,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: thinking.to_owned(),
        });
    }
    if !text.is_empty() {
        deltas.push(tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: text.to_owned(),
            phase: None,
        });
    }
    ProviderResponseUpdated {
        agent_prompt_id: spid
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas,
        compaction: None,
        status: None,
        response_stats: None,
        originator: PromptOriginator::User,
    }
}

fn user_status_clear_update(spid: &str) -> ProviderResponseUpdated {
    ProviderResponseUpdated {
        agent_prompt_id: spid
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
            retry: None,
        }),
        response_stats: None,
        originator: PromptOriginator::User,
    }
}

fn assistant_finished(
    spid: &str,
    text: &str,
    stop_reason: ProviderStopReason,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason,
        originator: PromptOriginator::User,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// The one-shot client ignores streaming updates for display but keeps the
/// appended streaming deltas so finished turns can print reasoning blocks
/// and the final answer only once the agent is done.
#[test]
fn one_shot_output_waits_through_tool_calls_and_keeps_final_snapshots() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-tool", "", Some("plan v1")));
    output.capture_update(&user_update("sp-tool", "", Some(" final")));

    assert!(
        !output.capture_finished(&ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "sp-tool"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            stop_reason: ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: PromptOriginator::User,
            output_items: Vec::new(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
    );

    output.capture_update(&user_update(
        "sp-final",
        "streamed answer",
        Some("answer plan"),
    ));
    assert!(output.capture_finished(&assistant_finished(
        "sp-final",
        "final answer",
        ProviderStopReason::EndTurn,
    )));

    assert_eq!(output.thinking_blocks, vec!["plan v1 final", "answer plan"]);
    assert_eq!(output.final_response.as_deref(), Some("final answer"));
}

/// A planned output-length source is an intermediate boundary, while the
/// successor's incomplete terminal ends the one-shot wait without inventing a
/// successful assistant answer.
#[test]
fn one_shot_output_waits_for_length_successor_and_preserves_incomplete_reasoning() {
    let mut output = OneShotOutput::default();
    let mut source = assistant_finished("sp-length-source", "", ProviderStopReason::Length);
    source.output_items = vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "source reasoning".to_owned(),
    })];
    source.output_length_disposition = tau_proto::OutputLengthDisposition::ContinuationPlanned {
        outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&source.agent_prompt_id),
        successor_agent_prompt_id: "sp-length-successor".parse().expect("successor prompt id"),
        ordinal: 1,
        limit: 1,
    };
    assert!(!output.capture_finished(&source));
    assert_eq!(output.thinking_blocks, vec!["source reasoning"]);
    assert_eq!(output.final_response, None);

    let mut successor = assistant_finished("sp-length-successor", "", ProviderStopReason::Length);
    successor.output_items = vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "incomplete successor reasoning".to_owned(),
    })];
    successor.output_length_disposition =
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&source.agent_prompt_id),
            source_agent_prompt_id: source.agent_prompt_id,
            ordinal: 1,
            outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
            outer_turn_finish_owed: true,
        };
    assert!(output.capture_finished(&successor));
    assert_eq!(
        output.thinking_blocks,
        vec!["source reasoning", "incomplete successor reasoning"]
    );
    assert_eq!(output.final_response, None);
}

/// The real one-shot event path must ignore the planned source terminal and
/// complete only from the reserved successor.
#[test]
fn prompt_stdin_planned_length_waits_then_successor_succeeds() {
    let mut output = OneShotOutput::default();
    let mut source = assistant_finished("ap-main-1", "", ProviderStopReason::Length);
    source.output_items = vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "source reasoning".to_owned(),
    })];
    source.output_length_disposition = tau_proto::OutputLengthDisposition::ContinuationPlanned {
        outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&source.agent_prompt_id),
        successor_agent_prompt_id: "ap-main-2".parse().expect("successor prompt id"),
        ordinal: 1,
        limit: 1,
    };
    assert!(
        !handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::ProviderResponseFinished(source)),
            &mut output,
        )
        .expect("planned source remains pending")
    );
    assert!(
        handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::ProviderResponseFinished(assistant_finished(
                "ap-main-2",
                "finished answer",
                ProviderStopReason::EndTurn,
            ))),
            &mut output,
        )
        .expect("successor completes")
    );
    assert_eq!(output.final_response.as_deref(), Some("finished answer"));
}

/// Every unplanned Length shape must terminate one-shot mode unsuccessfully
/// with its shape-specific safe diagnostic.
#[test]
fn prompt_stdin_terminal_length_variants_fail_with_exact_diagnostic() {
    let mut cases = Vec::new();
    let mut reasoning = assistant_finished("ap-main-1", "", ProviderStopReason::Length);
    reasoning.output_items = vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "hidden partial reasoning".to_owned(),
    })];
    cases.push((
        reasoning,
        "Model reached its output-token limit before completing the turn. No assistant answer or executable tool call was produced.",
    ));
    cases.push((
        assistant_finished("ap-main-2", "partial prose", ProviderStopReason::Length),
        "Model reached its output-token limit before completing the turn. The displayed response may be incomplete.",
    ));
    let mut tool_call = assistant_finished("ap-main-3", "", ProviderStopReason::Length);
    tool_call.output_items = vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: "truncated-call".into(),
        name: tau_proto::ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    cases.push((
        tool_call,
        "Model reached its output-token limit while producing a tool call. The incomplete call was not executed.",
    ));

    for (finished, diagnostic) in cases {
        let mut output = OneShotOutput::default();
        let error = handle_prompt_stdin_message(
            HarnessOutputMessage::deliver(Event::ProviderResponseFinished(finished)),
            &mut output,
        )
        .expect_err("terminal length must fail");
        assert_eq!(
            error.to_string(),
            format!("initial prompt failed (execution): {diagnostic}")
        );
        assert!(output.final_response.is_none());
        assert!(output.thinking_blocks.is_empty());
    }
}

/// Some provider paths may have accumulated streaming text but no final
/// assistant message item; fall back to accumulated deltas rather than
/// printing nothing.
#[test]
fn one_shot_output_falls_back_to_latest_streaming_text() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-final", "partial", None));
    output.capture_update(&user_update("sp-final", "complete", None));

    assert!(
        output.capture_finished(&ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "sp-final"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: PromptOriginator::User,
            output_items: Vec::new(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
    );

    assert_eq!(output.final_response.as_deref(), Some("partialcomplete"));
}

/// Provider retry/status resets must clear stale failed-attempt fallback text
/// so one-shot mode does not print reasoning or answer text from replaced work.
#[test]
fn one_shot_output_status_clear_resets_streaming_fallback() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-final", "bad", Some("bad plan")));
    output.capture_update(&user_status_clear_update("sp-final"));
    output.capture_update(&user_update("sp-final", "good", Some("good plan")));

    assert!(
        output.capture_finished(&ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "sp-final"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: PromptOriginator::User,
            output_items: Vec::new(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
    );

    assert_eq!(output.thinking_blocks, vec!["good plan"]);
    assert_eq!(output.final_response.as_deref(), Some("good"));
}

/// Prompt-stdin keeps stdout machine-friendly by routing provider reasoning
/// separately to stderr, so redirecting stderr leaves only the final answer.
#[test]
fn one_shot_output_writes_thinking_to_stderr_and_answer_to_stdout() {
    let output = OneShotOutput {
        thinking_blocks: vec!["first plan".to_owned(), "final plan".to_owned()],
        final_response: Some("answer".to_owned()),
        ..OneShotOutput::default()
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    output
        .write_to(
            &mut stdout,
            OutputPolicy::NonTerminal,
            &mut stderr,
            OutputPolicy::NonTerminal,
        )
        .expect("write one-shot output");

    assert_eq!(stdout, b"answer\n");
    assert_eq!(stderr, b"first plan\n\nfinal plan\n");
}

/// Terminal-item answer and reasoning bodies use their own descriptor policies,
/// covering ESC CSI, OSC through BEL, and C1 CSI without changing framing.
#[test]
fn one_shot_terminal_items_sanitize_each_terminal_destination_independently() {
    let mut finished = assistant_finished("sp-final", HOSTILE_TEXT, ProviderStopReason::EndTurn);
    finished.output_items.insert(
        0,
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Summary,
            text: HOSTILE_TEXT.to_owned(),
        }),
    );
    let mut output = OneShotOutput::default();
    assert!(output.capture_finished(&finished));

    for (stdout_policy, stderr_policy, expected_stdout, expected_stderr) in [
        (
            OutputPolicy::Terminal,
            OutputPolicy::NonTerminal,
            format!("{SAFE_TEXT}\n").into_bytes(),
            format!("{HOSTILE_TEXT}\n").into_bytes(),
        ),
        (
            OutputPolicy::NonTerminal,
            OutputPolicy::Terminal,
            format!("{HOSTILE_TEXT}\n").into_bytes(),
            format!("{SAFE_TEXT}\n").into_bytes(),
        ),
    ] {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        output
            .write_to(&mut stdout, stdout_policy, &mut stderr, stderr_policy)
            .expect("write terminal items");
        assert_eq!(stdout, expected_stdout);
        assert_eq!(stderr, expected_stderr);
    }
}

/// Streaming fallback retains the same destination-sensitive policy as final
/// items, including two-block reasoning separators and each trailing LF.
#[test]
fn one_shot_streaming_fallback_uses_destination_policy_and_existing_framing() {
    let mut output = OneShotOutput::default();
    output.capture_update(&user_update("sp-tool", "", Some(HOSTILE_TEXT)));
    assert!(!output.capture_finished(&ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::ToolCalls,
        ..assistant_finished("sp-tool", "", ProviderStopReason::ToolCalls)
    }));
    output.capture_update(&user_update("sp-final", HOSTILE_TEXT, Some(HOSTILE_TEXT)));
    assert!(output.capture_finished(&ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_items: Vec::new(),
        ..assistant_finished("sp-final", "", ProviderStopReason::EndTurn)
    }));

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    output
        .write_to(
            &mut stdout,
            OutputPolicy::NonTerminal,
            &mut stderr,
            OutputPolicy::Terminal,
        )
        .expect("write streaming fallback");
    assert_eq!(stdout, format!("{HOSTILE_TEXT}\n").as_bytes());
    assert_eq!(stderr, format!("{SAFE_TEXT}\n\n{SAFE_TEXT}\n").as_bytes());
}

/// Prompt-failure, provider-failure, and admission-rejection bodies sanitize
/// only for terminal stderr while their fixed prefixes remain unchanged.
#[test]
fn prompt_stdin_error_policy_sanitizes_only_dynamic_terminal_bodies() {
    let expected_prefixes = [
        "create-agent request failed (role_unavailable): ",
        "initial prompt failed (preprocessing): ",
        "initial prompt failed (execution): ",
    ];

    for (policy, expected_body) in [
        (OutputPolicy::Terminal, SAFE_TEXT),
        (OutputPolicy::NonTerminal, HOSTILE_TEXT),
    ] {
        for (error, prefix) in hostile_prompt_stdin_errors()
            .into_iter()
            .zip(expected_prefixes)
        {
            let rendered = sanitize_prompt_stdin_error(error, policy);
            assert_eq!(rendered.to_string(), format!("{prefix}{expected_body}"));
        }
    }
}

/// Construct every prompt-stdin error variant with a dynamic body.
fn hostile_prompt_stdin_errors() -> [CliError; 3] {
    [
        CliError::PromptStdin(PromptStdinError::Rejected {
            reason: tau_proto::UiCreateAgentRejection::RoleUnavailable,
            message: HOSTILE_TEXT.to_owned(),
        }),
        CliError::PromptStdin(PromptStdinError::PromptFailed {
            stage: tau_proto::AgentPromptFailureStage::Preprocessing,
            message: HOSTILE_TEXT.to_owned(),
        }),
        CliError::PromptStdin(PromptStdinError::ExecutionFailed {
            message: HOSTILE_TEXT.to_owned(),
        }),
    ]
}

/// Role values share stderr's injected policy, preserving plain multiline
/// Unicode while removing the full hostile control-sequence matrix on a TTY.
#[test]
fn prompt_stdin_role_uses_stderr_destination_policy() {
    let plain = "réviewer\nsecond line";
    assert_eq!(
        OutputPolicy::Terminal.dynamic_text(plain),
        OutputPolicy::NonTerminal.dynamic_text(plain),
    );
    assert_eq!(OutputPolicy::Terminal.dynamic_text(HOSTILE_TEXT), SAFE_TEXT,);
    assert_eq!(
        OutputPolicy::NonTerminal.dynamic_text(HOSTILE_TEXT),
        HOSTILE_TEXT,
    );

    for (policy, expected_role) in [
        (OutputPolicy::Terminal, SAFE_TEXT),
        (OutputPolicy::NonTerminal, HOSTILE_TEXT),
    ] {
        let mut stderr = Vec::new();
        print_prompt_stdin_headers(&mut stderr, "session-1", Some(HOSTILE_TEXT), policy);
        assert_eq!(
            stderr,
            format!("session_id: session-1\nrole: {expected_role}\n").as_bytes(),
        );
    }
}

/// Header rendering must surface a closed or exhausted stderr sink instead of
/// running provider work after prompt-stdin failed to present its headers.
#[test]
#[should_panic(expected = "failed to print prompt-stdin headers")]
fn prompt_stdin_headers_preserve_write_failures() {
    let mut storage = [];
    print_prompt_stdin_headers(
        &mut Cursor::new(&mut storage[..]),
        "session-1",
        Some("engineer"),
        OutputPolicy::NonTerminal,
    );
    panic!("provider continuation ran after header failure");
}
