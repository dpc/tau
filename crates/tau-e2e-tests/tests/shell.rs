use tau_e2e_tests::VcrFixture;

/// Runs one real provider + shell turn through the headless harness.
///
/// The test is opt-in because recording needs live provider auth. When enabled,
/// `TAU_VCR` decides whether this records cassettes or validates replay. The
/// test intentionally does not compare stdout; VCR cassette creation/replay is
/// the behavior under test.
#[test]
fn shell_vcr_turn() -> Result<(), Box<dyn std::error::Error>> {
    let Some(fixture) = VcrFixture::from_env("shell_vcr_turn")? else {
        return Ok(());
    };
    let outcome = fixture.run_turn(
        "Use the shell tool to run exactly `printf tau-vcr-e2e` and then finish the turn.",
    )?;
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
