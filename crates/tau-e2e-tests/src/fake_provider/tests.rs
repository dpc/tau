use super::*;

/// Ensures strict Configure decoding rejects undeclared control fields.
#[test]
fn config_rejects_unknown_fields() {
    let value = serde_json::json!({
        "scenario": ScenarioV1::text_v1("prompt", "response"),
        "command": "escape"
    });
    assert!(serde_json::from_value::<FakeConfig>(value).is_err());
}

/// Ensures phase one accepts exactly its text and single-tool-round grammars.
#[test]
fn validation_accepts_named_scenarios_only() {
    for scenario in [
        ScenarioV1::text_v1("prompt", "response"),
        ScenarioV1::dummy_tool_round_v1("prompt"),
    ] {
        assert!(FakeConfig { scenario }.validate().is_ok());
    }
    let mut invalid = ScenarioV1::text_v1("prompt", "response");
    invalid.turns.push(ScenarioTurnV1::Text {
        user_text: "extra".to_owned(),
        deltas: vec!["extra".to_owned()],
        response: "extra".to_owned(),
    });
    assert!(FakeConfig { scenario: invalid }.validate().is_err());
}

/// Ensures delta amplification and inconsistent final text fail at Configure.
#[test]
fn validation_bounds_and_matches_deltas() {
    let mut too_many = ScenarioV1::text_v1("prompt", "response");
    let ScenarioTurnV1::Text {
        user_text: _,
        deltas,
        response: _,
    } = &mut too_many.turns[0]
    else {
        unreachable!();
    };
    *deltas = vec![String::new(); MAX_DELTAS + 1];
    assert!(FakeConfig { scenario: too_many }.validate().is_err());

    let mut mismatch = ScenarioV1::text_v1("prompt", "response");
    let ScenarioTurnV1::Text {
        user_text: _,
        deltas,
        response: _,
    } = &mut mismatch.turns[0]
    else {
        unreachable!();
    };
    *deltas = vec!["different".to_owned()];
    assert!(FakeConfig { scenario: mismatch }.validate().is_err());
}

/// Ensures serialized scenario bytes and tool-call identity bounds fail closed.
#[test]
fn validation_bounds_scenario_bytes_and_call_ids() {
    let oversized = ScenarioV1::text_v1("prompt", "x".repeat(MAX_SCENARIO_BYTES));
    assert!(
        FakeConfig {
            scenario: oversized
        }
        .validate()
        .is_err()
    );

    let mut near = ScenarioV1::text_v1("prompt", "x".repeat(MAX_SCENARIO_BYTES));
    while serde_json::to_vec(&near)
        .expect("typed scenario serializes")
        .len()
        > MAX_SCENARIO_BYTES
    {
        let ScenarioTurnV1::Text {
            user_text: _,
            deltas,
            response,
        } = &mut near.turns[0]
        else {
            unreachable!();
        };
        response.pop();
        *deltas = vec![response.clone()];
    }
    assert!(FakeConfig { scenario: near }.validate().is_ok());

    for (call_id, result_id) in [
        ("".into(), "".into()),
        ("x".repeat(257).into(), "x".repeat(257).into()),
        ("call".into(), "different".into()),
    ] {
        let mut scenario = ScenarioV1::dummy_tool_round_v1("prompt");
        let ScenarioTurnV1::ToolCall {
            user_text: _,
            tool_name: _,
            call_id: actual_call_id,
        } = &mut scenario.turns[0]
        else {
            unreachable!();
        };
        *actual_call_id = call_id;
        let ScenarioTurnV1::ToolResult {
            call_id: actual_result_id,
            response: _,
        } = &mut scenario.turns[1]
        else {
            unreachable!();
        };
        *actual_result_id = result_id;
        assert!(FakeConfig { scenario }.validate().is_err());
    }
}

/// Ensures diagnostics remain byte-bounded without cutting UTF-8 code points.
#[test]
fn trace_bound_is_utf8_safe() {
    let message = format!("{}é", "x".repeat(1023));
    let bounded = bounded_trace_message(&message);
    assert!(bounded.len() <= 1024);
    assert_eq!(bounded, "x".repeat(1023));
}
