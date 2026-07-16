use tau_e2e_tests::{DeterministicFixture, ScenarioV1};
use tau_proto::{CborValue, ToolResultKind};

const FAKE_PROVIDER: &str = env!("CARGO_BIN_EXE_tau-e2e-fake-provider");
const DUMMY_TOOL: &str = env!("CARGO_BIN_EXE_tau-e2e-test-dummy");

/// Proves the real supervised provider route preserves two append deltas and a
/// complete durable final assistant response without any live provider.
#[test]
fn deterministic_text_stream_and_final_response() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = "return the deterministic greeting";
    let fixture = DeterministicFixture::new(
        "deterministic_text_stream_and_final_response",
        &ScenarioV1::text_v1(prompt, "hello deterministic tau"),
        FAKE_PROVIDER,
        None,
    )?;
    let outcome = fixture.run_turn(prompt)?;
    assert_eq!(outcome.response, "hello deterministic tau");
    assert!(outcome.tool_calls.is_empty());
    assert!(outcome.tool_results.is_empty());
    let events = fixture.durable_events()?;
    assert_exact_extensions(&events, &["e2e-fake-provider"]);
    assert_text_provider_sequence(&events);
    Ok(())
}

/// Proves one provider-authored function call crosses the harness's real tool
/// validation/dispatch path into deterministic `tau-ext-test-dummy`, and that
/// the exact call/result identity returns in the provider continuation.
#[test]
fn deterministic_dummy_tool_round() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = "run the deterministic dummy tool";
    let fixture = DeterministicFixture::new(
        "deterministic_dummy_tool_round",
        &ScenarioV1::dummy_tool_round_v1(prompt),
        FAKE_PROVIDER,
        Some(DUMMY_TOOL.into()),
    )?;
    let outcome = fixture.run_turn(prompt)?;
    assert_eq!(outcome.response, "tool completed");
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].call_id.as_str(), "fake-call-1");
    assert_eq!(outcome.tool_calls[0].name.as_str(), "restart_test_dummy");
    assert_eq!(outcome.tool_calls[0].arguments, CborValue::Map(Vec::new()));
    assert_eq!(outcome.tool_results.len(), 1);
    assert_eq!(outcome.tool_results[0].call_id.as_str(), "fake-call-1");
    assert_eq!(outcome.tool_results[0].kind, ToolResultKind::Final);
    assert_eq!(
        outcome.tool_results[0].result,
        CborValue::Text("restart succeeded".to_owned())
    );
    assert!(outcome.tool_results[0].provider_content.is_empty());
    let events = fixture.durable_events()?;
    assert_exact_extensions(&events, &["e2e-fake-provider", "e2e-test-dummy"]);
    assert_tool_provider_sequence(&events);
    Ok(())
}

/// Proves strict startup configuration rejects an unsupported scenario version
/// before Ready and causes required-extension harness startup to fail.
#[test]
fn deterministic_bad_config_fails_startup() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = ScenarioV1::text_v1("unused", "unused");
    scenario.version = 2;
    let fixture = DeterministicFixture::new(
        "deterministic_bad_config_fails_startup",
        &scenario,
        FAKE_PROVIDER,
        None,
    )?;
    let error = fixture
        .run_turn("unused")
        .expect_err("required provider must reject bad config before Ready");
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("scenario version must be 1"),
        "unexpected startup diagnostic: {diagnostic}"
    );
    assert!(fixture.root().join("artifacts/scenario.json").is_file());
    assert!(
        fixture
            .root()
            .join("artifacts/harness-config.json")
            .is_file()
    );
    let events = fixture.durable_events()?;
    assert!(events.iter().any(
        |event| matches!(event, tau_proto::Event::ExtensionStarting(starting)
            if starting.extension_name.as_str() == "e2e-fake-provider")
    ));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, tau_proto::Event::ExtensionReady(_))),
        "invalid config must fail before Ready"
    );
    fixture.acknowledge_expected_failure();
    Ok(())
}

/// Proves a first-turn prompt mismatch fails the real interaction, retains a
/// bounded semantic diagnostic, and cannot be mistaken for scenario
/// consumption.
#[test]
fn deterministic_prompt_mismatch_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new(
        "deterministic_prompt_mismatch_fails_closed",
        &ScenarioV1::text_v1("expected prompt", "unreachable"),
        FAKE_PROVIDER,
        None,
    )?;
    let error = fixture
        .run_turn("wrong prompt")
        .expect_err("mismatched prompt must disconnect without a successful response");
    assert!(
        error.to_string().contains("provider disconnected")
            || error
                .to_string()
                .contains("scenario first mismatch at turn 0"),
        "unexpected mismatch error: {error}"
    );
    let trace = fixture.trace()?;
    assert!(trace.contains("scenario first mismatch at turn 0: last user text mismatch"));
    assert!(trace.lines().all(|line| line.len() <= 1024));
    assert!(fixture.assert_consumed().is_err());
    fixture.acknowledge_expected_failure();
    Ok(())
}

fn assert_exact_extensions(events: &[tau_proto::Event], expected: &[&str]) {
    let mut actual = events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ExtensionStarting(starting) => Some(starting.extension_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, expected, "unexpected active extension set");
}

fn assert_one_fake_model(events: &[tau_proto::Event]) {
    let snapshots = events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ProviderModelsUpdated(snapshot) => Some(snapshot),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].models.len(), 1);
    assert_eq!(snapshots[0].models[0].id.provider.as_str(), "fake");
    assert_eq!(snapshots[0].models[0].id.model.as_str(), "test");
}

fn assert_text_provider_sequence(events: &[tau_proto::Event]) {
    assert_one_fake_model(events);
    let lifecycle = provider_lifecycle(events);
    let [
        ProviderLifecycle::Submitted(submitted),
        ProviderLifecycle::Updated(first),
        ProviderLifecycle::Updated(second),
        ProviderLifecycle::Finished(finished),
    ] = lifecycle.as_slice()
    else {
        panic!("unexpected text provider lifecycle: {lifecycle:?}");
    };
    let id = &submitted.agent_prompt_id;
    assert_eq!(&first.agent_prompt_id, id);
    assert_eq!(&second.agent_prompt_id, id);
    assert_eq!(&finished.agent_prompt_id, id);
    assert_update(first, "hello determ");
    assert_update(second, "inistic tau");
    assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::EndTurn);
    assert_assistant(&finished.output_items, "hello deterministic tau");
}

fn assert_tool_provider_sequence(events: &[tau_proto::Event]) {
    assert_one_fake_model(events);
    let lifecycle = provider_lifecycle(events);
    let [
        ProviderLifecycle::Submitted(first_submitted),
        ProviderLifecycle::Finished(first_finished),
        ProviderLifecycle::Submitted(second_submitted),
        ProviderLifecycle::Finished(second_finished),
    ] = lifecycle.as_slice()
    else {
        panic!("unexpected tool provider lifecycle: {lifecycle:?}");
    };
    assert_eq!(
        first_submitted.agent_prompt_id,
        first_finished.agent_prompt_id
    );
    assert_eq!(
        second_submitted.agent_prompt_id,
        second_finished.agent_prompt_id
    );
    assert_ne!(
        first_submitted.agent_prompt_id,
        second_submitted.agent_prompt_id
    );
    assert_eq!(
        first_finished.stop_reason,
        tau_proto::ProviderStopReason::ToolCalls
    );
    assert_eq!(
        first_finished.output_items,
        vec![tau_proto::ContextItem::ToolCall(
            outcome_tool_call_projection()
        )]
    );
    assert_eq!(
        second_finished.stop_reason,
        tau_proto::ProviderStopReason::EndTurn
    );
    assert_assistant(&second_finished.output_items, "tool completed");
}

#[derive(Debug)]
enum ProviderLifecycle<'a> {
    Submitted(&'a tau_proto::ProviderPromptSubmitted),
    Updated(&'a tau_proto::ProviderResponseUpdated),
    Finished(&'a tau_proto::ProviderResponseFinished),
}

fn provider_lifecycle(events: &[tau_proto::Event]) -> Vec<ProviderLifecycle<'_>> {
    events
        .iter()
        .filter_map(|event| match event {
            tau_proto::Event::ProviderPromptSubmitted(value) => {
                Some(ProviderLifecycle::Submitted(value))
            }
            tau_proto::Event::ProviderResponseUpdated(value) => {
                Some(ProviderLifecycle::Updated(value))
            }
            tau_proto::Event::ProviderResponseFinished(value) => {
                Some(ProviderLifecycle::Finished(value))
            }
            _ => None,
        })
        .collect()
}

fn outcome_tool_call_projection() -> tau_proto::ToolCallItem {
    tau_proto::ToolCallItem {
        call_id: "fake-call-1".into(),
        name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: Some("{}".to_owned()),
        responses_envelope: None,
    }
}

fn assert_update(update: &tau_proto::ProviderResponseUpdated, expected: &str) {
    match update.deltas.as_slice() {
        [
            tau_proto::ProviderResponseTextDelta::Message {
                output_index,
                text,
                phase,
            },
        ] => {
            assert_eq!(*output_index, 0);
            assert_eq!(text, expected);
            assert_eq!(*phase, None);
        }
        other => panic!("unexpected update deltas: {other:?}"),
    }
}

fn assert_assistant(items: &[tau_proto::ContextItem], expected: &str) {
    match items {
        [tau_proto::ContextItem::Message(message)] => {
            assert_eq!(message.role, tau_proto::ContextRole::Assistant);
            assert_eq!(message.phase, None);
            assert_eq!(message.responses_raw_json, None);
            assert_eq!(
                message.content,
                vec![tau_proto::ContentPart::Text {
                    text: expected.to_owned()
                }]
            );
        }
        other => panic!("unexpected assistant output: {other:?}"),
    }
}
