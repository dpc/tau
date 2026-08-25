use tau_e2e_tests::VcrFixture;
use tau_proto::CborValue;

/// Runs one real provider + shell turn through the headless harness.
///
/// The test is opt-in because recording needs live provider auth. When enabled,
/// `TAU_VCR` decides whether this records cassettes or validates replay. The
/// test keeps the provider prose unconstrained while proving its typed shell
/// call, correlated result, and UI progress seams.
#[test]
fn shell_vcr_turn() -> Result<(), Box<dyn std::error::Error>> {
    let Some(fixture) = VcrFixture::from_env("shell_vcr_turn")? else {
        return Ok(());
    };
    let outcome = fixture.run_turn(
        "Use the shell tool to run exactly `printf tau-vcr-e2e` and then finish the turn.",
    )?;
    let [call] = outcome.tool_calls.as_slice() else {
        panic!(
            "expected exactly one tool call, got {:?}",
            outcome.tool_calls
        );
    };
    assert_eq!(call.name.as_str(), "shell");
    assert_eq!(
        cbor_map_text(&call.arguments, "command"),
        Some("printf tau-vcr-e2e")
    );
    let [result] = outcome.tool_results.as_slice() else {
        panic!(
            "expected exactly one terminal tool result, got {:?}",
            outcome.tool_results
        );
    };
    assert_eq!(result.call_id, call.call_id);
    assert_eq!(result.tool_name, call.name);
    assert!(
        cbor_map_text(&result.result, "output")
            .is_some_and(|output| output.contains("tau-vcr-e2e")),
        "expected the correlated shell result to contain `tau-vcr-e2e`, got {:?}",
        result.result
    );
    assert!(
        outcome
            .progress_messages
            .iter()
            .any(|message| message == "shell" || message.starts_with("shell:")),
        "expected shell tool progress, got {:?}",
        outcome.progress_messages
    );
    Ok(())
}

/// Returns one text field from a protocol CBOR map.
fn cbor_map_text<'a>(value: &'a CborValue, field: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match (key, value) {
        (CborValue::Text(key), CborValue::Text(value)) if key == field => Some(value.as_str()),
        _ => None,
    })
}
