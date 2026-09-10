use super::*;

/// Local fragmented traffic must produce one complete-message observation, not
/// two frame timings, and pair its reader, owner and decode boundaries.
#[test]
fn fragmented_message_has_paired_owner_timing() {
    let server = TestWsServer::spawn(ServerScript::FragmentedText {
        first: r#"{"type":"response.completed","#.to_owned(),
        second: r#""response":{"id":"timing-local"}}"#.to_owned(),
    });
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let mut conn = WsConn::connect(
        &config,
        "thread-timing",
        &crate::test_network_policy(),
        &mut NeverAbort,
    )
    .expect("local WebSocket");
    let fixture = PromptFixture::new();
    let mut trace = private_trace::AttemptTrace::selected_for_capture(
        private_trace::Backend::Codex,
        private_trace::Transport::Websocket,
        true,
    );
    conn.run_response(
        &config,
        "ap-timing",
        &fixture.payload(),
        None,
        None,
        ResponseMode::Ordinary,
        &mut NeverAbort,
        &mut |_| {},
        &mut |_| {},
        &mut trace,
    )
    .expect("completed response");
    assert!(conn.message_read_timing.observe().is_none());
    let timing = trace
        .expect("trace")
        .finish_with_timing(private_trace::Outcome::Completed);
    let (read, dequeue) = timing.text_message_read.expect("complete message");
    let (associated_read, decode) = timing.associated_message_read.expect("associated message");
    assert_eq!(read, associated_read);
    assert!(decode >= dequeue);
    assert!(
        timing
            .dispatch_to_first_decoded_payload_us
            .expect("decoded")
            >= read
    );
    assert_eq!(timing.decode_count, 1);
    drop(conn);
    server.join();
}

/// Old queued samples are not current reads; malformed payloads still count as
/// owner input but never successful decoding, and errors deactivate
/// observation.
#[test]
fn queued_old_message_and_malformed_payload_preserve_missing_timing() {
    let (mut conn, inbound, _outbound) = test_ws_conn();
    let previous = conn.message_read_timing.activate().expect("old owner");
    let read = conn.message_read_timing.observe();
    drop(previous);
    inbound
        .send_blocking(InboundEvent::Event {
            text: "{not-json".into(),
            read,
        })
        .expect("queued old message");
    let fixture = PromptFixture::new();
    let mut trace = private_trace::AttemptTrace::selected_for_capture(
        private_trace::Backend::Codex,
        private_trace::Transport::Websocket,
        true,
    );
    let result = conn.run_response(
        &test_responses_config(),
        "ap-timing-error",
        &fixture.payload(),
        None,
        None,
        ResponseMode::Ordinary,
        &mut NeverAbort,
        &mut |_| {},
        &mut |_| {},
        &mut trace,
    );
    assert!(result.is_err());
    assert!(conn.message_read_timing.observe().is_none());
    let timing = trace
        .expect("trace")
        .finish_with_timing(private_trace::Outcome::Failed);
    assert!(timing.dispatch_to_first_input_us.is_some());
    assert_eq!(timing.text_message_read, None);
    assert_eq!(timing.associated_message_read, None);
    assert_eq!(timing.dispatch_to_first_decoded_payload_us, None);
    assert_eq!(timing.decode_count, 1);
}

/// Cancellation before dispatch must close the observation scope without
/// manufacturing a dispatch, reader sample or decoded event.
#[test]
fn canceled_owner_disables_read_observations() {
    let (mut conn, _inbound, _outbound) = test_ws_conn();
    let fixture = PromptFixture::new();
    let mut trace = private_trace::AttemptTrace::selected_for_capture(
        private_trace::Backend::Codex,
        private_trace::Transport::Websocket,
        true,
    );
    let result = conn.run_response(
        &test_responses_config(),
        "ap-timing-cancel",
        &fixture.payload(),
        None,
        None,
        ResponseMode::Ordinary,
        &mut AbortAfterChecks { remaining_false: 0 },
        &mut |_| {},
        &mut |_| {},
        &mut trace,
    );
    assert!(matches!(result, Err(LlmError::Canceled)));
    assert!(conn.message_read_timing.observe().is_none());
    let timing = trace
        .expect("trace")
        .finish_with_timing(private_trace::Outcome::Canceled);
    assert_eq!(timing.final_dispatch_us, None);
    assert_eq!(timing.text_message_read, None);
}

/// A non-response decoded payload must not supply the associated-message pair.
/// The later response keeps its own read timestamp instead of mixing messages.
#[test]
fn unrelated_payload_does_not_supply_associated_read_pair() {
    let mut trace = private_trace::AttemptTrace::selected_for_capture(
        private_trace::Backend::Codex,
        private_trace::Transport::Websocket,
        true,
    );
    trace.as_mut().expect("trace").record_dispatch();
    let unrelated = Instant::now();
    trace.as_mut().expect("trace").text_message_read(unrelated);
    observe_associated_timing_milestone(
        &mut trace,
        &serde_json::json!({"type": "codex.rate_limits"}),
        Some(unrelated),
    );
    // Synthetic read instants isolate message selection from wall-clock speed
    // and microsecond truncation; elapsed association delay is not tested here.
    let response = unrelated + Duration::from_millis(1);
    observe_associated_timing_milestone(
        &mut trace,
        &serde_json::json!({"type": "response.created"}),
        Some(response),
    );
    let timing = trace
        .expect("trace")
        .finish_with_timing(private_trace::Outcome::Completed);
    assert_eq!(
        timing.associated_message_read.expect("associated").0,
        timing.text_message_read.expect("raw").0 + 1_000,
    );
}
