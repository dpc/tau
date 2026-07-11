use std::io::Cursor;

use tau_proto::{
    AgentPromptSubmitted, CborValue, Configure, Event, HarnessInputMessage, HarnessInputReader,
    HarnessOutputMessage, HarnessOutputWriter, InterceptReply, InterceptRequest,
    PromptMessageClass, PromptOriginator, ToolStarted, UnixMicros,
};

use super::*;

fn invoke_restart() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        arguments: tau_proto::CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }))
}

fn extension_originated_restart() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: "extension-call".into(),
        tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        arguments: tau_proto::CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::Extension {
            name: tau_proto::ExtensionName::new("fixture"),
            query_id: "query-1".to_owned(),
        },
    }))
}

fn replayed_restart() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1_700_000_000_000_000),
        Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
            arguments: tau_proto::CborValue::Map(Vec::new()),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: PromptOriginator::User,
        }),
    )
}

fn restart_config(mode: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        instance_name: None,
        config: CborValue::Map(vec![(
            CborValue::Text("restart_mode".to_owned()),
            CborValue::Text(mode.to_owned()),
        )]),
        state_dir: None,
        secrets: std::collections::BTreeMap::new(),
    })
}

fn run_restart_frames(
    input_frames: &[HarnessOutputMessage],
    seed: u64,
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    for frame in input_frames {
        writer.write_message(frame).expect("write input frame");
    }
    writer.flush().expect("flush input");

    let mut output = Vec::new();
    let mut rng = StdRng::seed_from_u64(seed);
    run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

    let mut reader = HarnessInputReader::new(Cursor::new(output));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read") {
        frames.push(frame);
    }
    frames
}

fn emitted_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn fixture_extension_originator() -> PromptOriginator {
    PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("fixture"),
        query_id: "query-1".to_owned(),
    }
}

/// Verifies the historical random restart fixture can reply with a tool error.
#[test]
fn restart_tool_can_return_error() {
    let frames = run_restart_frames(&[invoke_restart()], 1);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    let Some(Event::ToolRegister(register)) = emitted_event(&frames[3]) else {
        panic!("expected tool register");
    };
    assert_eq!(
        register
            .tool_group
            .as_ref()
            .map(|group| group.name.as_str()),
        Some("test")
    );
    assert!(matches!(frames[4], HarnessInputMessage::Ready(_)));
    let Some(Event::ToolError(error)) = frames.get(5).and_then(emitted_event) else {
        panic!("expected tool error");
    };
    assert_eq!(error.message, "restarting failed");
    assert_eq!(frames.len(), 6);
}

/// Verifies the historical random restart fixture can exit without replying.
#[test]
fn restart_tool_can_exit_without_reply() {
    let frames = run_restart_frames(&[invoke_restart()], 2);
    assert_eq!(frames.len(), 5);
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    let Some(Event::ToolRegister(register)) = emitted_event(&frames[3]) else {
        panic!("expected tool register");
    };
    assert_eq!(
        register
            .tool_group
            .as_ref()
            .map(|group| group.name.as_str()),
        Some("test")
    );
    // The random-exit branch must exit without emitting any
    // reply frame for the invoke — guard against a future bug that
    // re-introduces a stray ToolResult/ToolError before exit.
    assert!(
        frames.iter().all(|frame| !matches!(
            emitted_event(frame),
            Some(Event::ToolError(_)) | Some(Event::ToolResult(_))
        )),
        "no tool reply frame should appear in the random-exit branch"
    );
}

/// Verifies deterministic `success` configuration returns a final tool result.
#[test]
fn restart_tool_config_success_returns_tool_result() {
    // Harness restart tests need a deterministic happy path, not the
    // historical random exit-or-error behavior.
    let frames = run_restart_frames(&[restart_config("success"), invoke_restart()], 1);

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(result),
            _ => None,
        })
        .expect("configured success should return a tool result");
    assert_eq!(result.call_id.as_str(), "call-1");
    assert_eq!(
        result.result,
        CborValue::Text("restart succeeded".to_owned())
    );
    assert_eq!(result.kind, tau_proto::ToolResultKind::Final);
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolError(_))))
    );
}

/// Verifies deterministic `error` configuration overrides random exit.
#[test]
fn restart_tool_config_error_overrides_random_exit() {
    // Seed 2 hits the random exit branch; config must force a reply so
    // harness tests can exercise tool-error handling deterministically.
    let frames = run_restart_frames(&[restart_config("error"), invoke_restart()], 2);

    let error = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolError(error)) => Some(error),
            _ => None,
        })
        .expect("configured error should return a tool error");
    assert_eq!(error.call_id.as_str(), "call-1");
    assert_eq!(error.message, "restarting failed");
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolResult(_))))
    );
}

/// Verifies deterministic `exit` configuration overrides random error.
#[test]
fn restart_tool_config_exit_overrides_random_error() {
    // Seed 1 hits the random error branch; config must force the
    // extension-disconnect shape with no tool reply frame.
    let frames = run_restart_frames(&[restart_config("exit"), invoke_restart()], 1);

    assert_eq!(frames.len(), 5);
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolError(_)) | Some(Event::ToolResult(_))
    )));
}

/// Verifies invalid restart configuration is reported to the harness.
#[test]
fn invalid_restart_mode_emits_config_error() {
    let frames = run_restart_frames(&[restart_config("bogus")], 1);

    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error),
            _ => None,
        })
        .expect("invalid config should emit ConfigError");
    assert!(
        error.message.contains("bogus") && error.message.contains("expected one of"),
        "error should describe invalid restart mode: {}",
        error.message
    );
}

/// Verifies deterministic success replies preserve non-user invocation origin.
#[test]
fn restart_tool_result_preserves_originator() {
    let frames = run_restart_frames(
        &[restart_config("success"), extension_originated_restart()],
        1,
    );

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(result),
            _ => None,
        })
        .expect("configured success should return a tool result");
    assert_eq!(result.call_id.as_str(), "extension-call");
    assert_eq!(result.originator, fixture_extension_originator());
}

/// Verifies deterministic error replies preserve non-user invocation origin.
#[test]
fn restart_tool_error_preserves_originator() {
    let frames = run_restart_frames(
        &[restart_config("error"), extension_originated_restart()],
        1,
    );

    let error = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolError(error)) => Some(error),
            _ => None,
        })
        .expect("configured error should return a tool error");
    assert_eq!(error.call_id.as_str(), "extension-call");
    assert_eq!(error.originator, fixture_extension_originator());
}

/// Verifies replayed tool-start events do not re-run side-effecting tool logic.
#[test]
fn replayed_restart_tool_is_ignored() {
    let frames = run_restart_frames(&[restart_config("success"), replayed_restart()], 1);

    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolError(_)) | Some(Event::ToolResult(_))
    )));
    assert_eq!(frames.len(), 5);
}

/// Verifies replayed exit-mode restart events do not terminate the extension.
#[test]
fn replayed_exit_restart_does_not_prevent_later_live_tool_result() {
    let frames = run_restart_frames(
        &[
            restart_config("exit"),
            replayed_restart(),
            restart_config("success"),
            invoke_restart(),
        ],
        1,
    );

    let results = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "only the later live invoke should reply");
    assert_eq!(results[0].call_id.as_str(), "call-1");
}

fn intercepted_prompt(prompt: AgentPromptSubmitted) -> HarnessOutputMessage {
    HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(Event::AgentPromptSubmitted(prompt)),
        transient: false,
    })
}

fn test_prompt(text: &str) -> AgentPromptSubmitted {
    AgentPromptSubmitted {
        inference_activation: false,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: text.to_owned(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }
}

fn run_intercept(prompt: AgentPromptSubmitted) -> (Vec<tau_proto::Emit>, Vec<InterceptReply>) {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    writer
        .write_message(&intercepted_prompt(prompt))
        .expect("write intercepted prompt");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

    let mut reader = HarnessInputReader::new(Cursor::new(output));
    let mut notice_emits = Vec::new();
    let mut replies = Vec::new();
    while let Some(frame) = reader.read_message().expect("read") {
        match frame {
            HarnessInputMessage::Emit(emit)
                if matches!(emit.event.as_ref(), Event::HarnessNotice(_)) =>
            {
                notice_emits.push(emit);
            }
            HarnessInputMessage::InterceptReply(reply) => replies.push(reply),
            _ => {}
        }
    }
    (notice_emits, replies)
}
fn replaced_prompt_text(reply: &InterceptReply) -> Option<String> {
    match &reply.action {
        tau_proto::InterceptAction::Pass(Some(boxed)) => match boxed.as_ref() {
            Event::AgentPromptSubmitted(p) => Some(p.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Verifies corrected prompts emit a user-facing transient harness notice.
#[test]
fn prompt_with_tao_is_corrected_with_notice() {
    let (emits, replies) = run_intercept(test_prompt("I love Tao"));

    assert_eq!(emits.len(), 1, "exactly one notice emit on correction");
    assert!(emits[0].transient, "correction notice should be transient");
    assert!(matches!(
        emits[0].event.as_ref(),
        Event::HarnessNotice(notice) if notice.message.contains("Tau") && notice.message.contains("corrected")
    ));

    assert_eq!(replies.len(), 1);
    let replaced =
        replaced_prompt_text(&replies[0]).expect("intercept reply carries replacement event");
    assert_eq!(replaced, "I love Tau");
}

/// Verifies the ASCII replacement preserves the case of the original letters.
#[test]
fn prompt_correction_preserves_letter_case() {
    for (input, expected) in [
        ("tao", "tau"),
        ("Tao", "Tau"),
        ("TAO", "TAU"),
        ("tAo", "tAu"),
        ("TaO", "TaU"),
        ("the TAO of Tao and tao", "the TAU of Tau and tau"),
    ] {
        let (_, replies) = run_intercept(test_prompt(input));
        let replaced = replaced_prompt_text(&replies[0]).unwrap_or_else(|| {
            panic!("expected replacement for input {input:?}");
        });
        assert_eq!(replaced, expected, "case preservation for {input:?}");
    }
}

/// Verifies prompt interception preserves non-text identity and routing fields.
#[test]
fn prompt_correction_preserves_prompt_identity_fields() {
    let mut prompt = test_prompt("internal Tao");
    prompt.message_class = PromptMessageClass::Internal;
    prompt.originator = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("fixture"),
        query_id: "query-1".to_owned(),
    };
    prompt.display_name = Some("fixture".to_owned());
    prompt.ctx_id = Some("ctx-1".into());
    let original_agent_id = prompt.agent_id.clone();

    let (_, replies) = run_intercept(prompt);

    let tau_proto::InterceptAction::Pass(Some(event)) = &replies[0].action else {
        panic!("expected replacement prompt");
    };
    let Event::AgentPromptSubmitted(replaced) = event.as_ref() else {
        panic!("expected AgentPromptSubmitted replacement");
    };
    assert_eq!(replaced.text, "internal Tau");
    assert_eq!(replaced.agent_id, original_agent_id);
    assert_eq!(replaced.message_class, PromptMessageClass::Internal);
    assert_eq!(
        replaced.originator,
        PromptOriginator::Extension {
            name: tau_proto::ExtensionName::new("fixture"),
            query_id: "query-1".to_owned(),
        }
    );
    assert_eq!(replaced.display_name.as_deref(), Some("fixture"));
    assert_eq!(replaced.ctx_id.as_deref(), Some("ctx-1"));
}

/// Verifies the fixture does not rewrite `tao` embedded within ASCII words.
#[test]
fn prompt_correction_skips_substrings_inside_words() {
    // `tao` inside `chaotic` is just three letters, not the word —
    // don't touch it.
    let (emits, replies) = run_intercept(test_prompt("a chaotic taoism enjoyer"));

    assert_eq!(emits.len(), 0, "no notice when no whole-word match");
    assert_eq!(replies.len(), 1);
    assert!(
        matches!(&replies[0].action, tau_proto::InterceptAction::Pass(None)),
        "no replacement when no whole-word match"
    );
}

/// Verifies prompts without a matching word are passed through unchanged.
#[test]
fn prompt_without_tao_passes_through_unchanged() {
    let (emits, replies) = run_intercept(test_prompt("hello world"));

    assert_eq!(emits.len(), 0);
    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].action,
        tau_proto::InterceptAction::Pass(None)
    ));
}
