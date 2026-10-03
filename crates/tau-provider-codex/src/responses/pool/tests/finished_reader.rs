use std::time::Instant;

use super::*;

/// Waits for actual task completion, not merely peer close or an abort request.
fn wait_for_finished_reader(conn: &WsConn) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !conn.reader_is_finished() {
        assert!(Instant::now() < deadline, "reader did not finish");
        thread::yield_now();
    }
}

/// A close/EOF observed while pooled must cause fresh dispatch, not spend
/// repair on an envelope enqueued to a reader already known to be finished.
#[test]
fn finished_pooled_reader_is_rejected_before_turn_dispatch() {
    for abrupt in [false, true] {
        let (addr, server) = spawn_fake_codex_server();
        let gate = Arc::new(ResponseGate::new());
        server.lock_state().pooled_close = Some((Arc::clone(&gate), abrupt));
        let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
        let mut pool = WsPool::new();
        let first = run_context_turn(&mut pool, &config, "pooled-close", "first", context(&[]));
        gate.wait_for_arrival();
        gate.release_one();
        let key = pool_key_for(&config, "test-agent", tau_proto::PromptOriginator::User);
        wait_for_finished_reader(pool.conns.peek(&key).expect("pooled socket"));
        let response_id = first.response_id.clone().expect("first response id");
        run_context_turn(
            &mut pool,
            &config,
            "pooled-close",
            "second",
            context_after_response(
                &response_id,
                first.into_output_items(),
                vec![user_msg("next")],
            ),
        );
        let state = server.lock_state();
        assert_eq!(state.upgrade_count, 2);
        assert_eq!(state.turns_per_connection, vec![1, 1]);
        assert_eq!(state.requests.len(), 2);
        assert!(state.requests[1].get("previous_response_id").is_none());
        assert_eq!(
            pool.stats().silent_reconnects,
            0,
            "rejection is a pool miss, not repair"
        );
    }
}

/// Prewarm shares checkout admission and must not consume its one repair on a
/// cached reader that has already finished.
#[test]
fn finished_pooled_reader_is_rejected_before_prewarm_dispatch() {
    let (addr, server) = spawn_fake_codex_server();
    let gate = Arc::new(ResponseGate::new());
    server.lock_state().pooled_close = Some((Arc::clone(&gate), false));
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    run_context_turn(&mut pool, &config, "pooled-prewarm", "first", context(&[]));
    gate.wait_for_arrival();
    gate.release_one();
    let key = pool_key_for(&config, "test-agent", tau_proto::PromptOriginator::User);
    wait_for_finished_reader(pool.conns.peek(&key).expect("pooled socket"));
    let session_id = tau_proto::SessionId::parse("pooled-prewarm").expect("session");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: false,
    };
    let shared = SharedWsPool::new(Arc::new(crate::test_network_policy()));
    shared.inner.lock().expect("shared pool").pool = pool;
    run_prewarm_through_shared_pool(
        &shared,
        &config,
        session_id.as_str(),
        &request,
        &mut NeverAbort,
    )
    .expect("fresh prewarm")
    .expect("prewarm admitted");
    let state = server.lock_state();
    assert_eq!(state.upgrade_count, 2);
    assert_eq!(state.turns_per_connection, vec![1, 1]);
    assert_eq!(state.requests[1]["generate"], false);
    assert!(state.requests[1].get("previous_response_id").is_none());
    assert_eq!(shared.stats().expect("pool stats").silent_reconnects, 0);
}

/// A still-running reader may close immediately after checkout. Admission must
/// neither drain its terminal queue nor pretend that a running task is healthy.
#[test]
fn reader_close_after_checkout_remains_owned_by_turn() {
    let (addr, server) = spawn_fake_codex_server();
    let gate = Arc::new(ResponseGate::new());
    server.lock_state().pooled_close = Some((Arc::clone(&gate), false));
    let config = make_config(&format!("http://{addr}/backend-api"), Some("acc"));
    let mut pool = WsPool::new();
    run_context_turn(&mut pool, &config, "close-race", "first", context(&[]));
    gate.wait_for_arrival();
    let key = pool_key_for(&config, "test-agent", tau_proto::PromptOriginator::User);
    let mut conn = pool
        .checkout(&key, &config.api_key)
        .expect("reader still running");
    assert!(!conn.reader_is_finished());
    gate.release_one();
    wait_for_finished_reader(&conn);
    // Checkout owns the socket now; completion does not mutate ownership or
    // consume the buffered close. Existing run_turn failure/repair owns it.
    assert_eq!(pool.len(), 0);
    assert!(conn.reader_is_finished());
    let session_id = tau_proto::SessionId::parse("close-race").expect("session");
    let agent_id = tau_proto::AgentId::parse("test-agent").expect("agent");
    let request = PromptPayload {
        system_prompt: "sys",
        context: context(&[]),
        hosted_tools: &[],
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: false,
    };
    let mut dispatched = 0;
    let error = match conn.run_turn(
        &config,
        "raced-close",
        &request,
        None,
        None,
        &mut NeverAbort,
        &mut |_| dispatched += 1,
        &mut |_| {},
    ) {
        Ok(_) => panic!("closed checked-out socket cannot complete"),
        Err(error) => error,
    };
    assert!(
        is_recoverable_ws_error(&error),
        "existing repair classification"
    );
    assert_eq!(
        dispatched, 1,
        "post-check close is still an attempted dispatch"
    );
}
