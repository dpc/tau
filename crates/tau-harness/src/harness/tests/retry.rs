use super::*;
use crate::agent::Agent;

fn retry_request(id: &str, session_id: &str, agent_id: Option<&str>) -> Event {
    Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
        request_id: tau_proto::RetryPromptRequestId::parse(id).expect("valid retry request id"),
        session_id: session_id.into(),
        target_agent_id: agent_id.map(crate::parse_agent_id),
        agent_prompt_id: None,
    })
}

fn retry_result(
    id: tau_proto::RetryPromptRequestId,
    prompt_id: &str,
    status: tau_proto::RetryPromptStatus,
) -> Event {
    Event::ProviderRetryPromptResultReported(tau_proto::ProviderRetryPromptResult {
        request_id: id,
        agent_prompt_id: prompt_id.into(),
        status,
    })
}

fn provider_request_id(h: &Harness, ui_id: &str) -> tau_proto::RetryPromptRequestId {
    h.pending_retry_prompts
        .iter()
        .find(|(_, pending)| pending.ui_request_id.as_str() == ui_id)
        .map(|(id, _)| id.clone())
        .expect("pending provider retry token")
}

fn add_routed_prompt(h: &mut Harness, agent_id: &str, prompt_id: &str, provider_id: Option<&str>) {
    let cid = crate::parse_agent_id(agent_id);
    let mut agent = Agent::new(
        cid.clone(),
        1,
        h.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    agent.agent_id = Some(agent_id.to_owned());
    agent.display_name = Some(format!("{agent_id} label"));
    agent.in_flight_prompt = Some(prompt_id.into());
    h.agents.insert(cid.clone(), agent);
    h.agent_routes.insert(agent_id.to_owned(), cid.clone());
    h.prompt_agents.insert(prompt_id.into(), cid);
    if let Some(provider_id) = provider_id {
        h.pending_provider_prompts
            .insert(prompt_id.into(), provider_id.into());
    }
}

fn matching_events(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    predicate: impl Fn(&Event) -> bool,
) -> usize {
    sink.lock()
        .expect("sink")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame))
        .filter(|event| predicate(event))
        .count()
}

/// A retry captures one agent's exact APID and routes only to that APID's
/// recorded provider. Results are accepted only from that provider with the
/// matching APID, consumed once, and delivered only to the invoking UI.
#[test]
fn retry_routes_exact_prompt_and_trusts_only_correlated_provider_result() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider_a = connect_ready_configured_extension(
        &mut h,
        "provider-a",
        "provider-a",
        tau_proto::ClientKind::Provider,
    );
    let provider_b = connect_ready_configured_extension(
        &mut h,
        "provider-b",
        "provider-b",
        tau_proto::ClientKind::Provider,
    );
    let requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);
    let observer = connect_test_client(&mut h, "observer", tau_proto::ClientKind::Ui);
    add_routed_prompt(&mut h, "agent-a", "prompt-a", Some("provider-a"));
    add_routed_prompt(&mut h, "agent-b", "prompt-b", Some("provider-b"));

    h.handle_client_event_inner("requester", retry_request("retry-1", "s1", Some("agent-b")))
        .expect("retry request");
    let provider_token = provider_request_id(&h, "retry-1");

    assert_eq!(
        matching_events(&provider_a, |event| matches!(
            event,
            Event::UiRetryPrompt(_)
        )),
        0
    );
    let provider_b_events = provider_b.lock().expect("provider B sink");
    let targeted = provider_b_events
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame))
        .find_map(|event| match event {
            Event::UiRetryPrompt(request) => Some(request),
            _ => None,
        })
        .expect("targeted retry");
    assert_eq!(targeted.agent_prompt_id.as_deref(), Some("prompt-b"));
    assert_eq!(targeted.target_agent_id.as_deref(), Some("agent-b"));
    drop(provider_b_events);

    h.handle_extension_event(
        "provider-a",
        TestProtocolItem::Event(retry_result(
            provider_token.clone(),
            "prompt-b",
            tau_proto::RetryPromptStatus::Accepted,
        )),
    )
    .expect("spoofed result");
    h.handle_extension_event(
        "provider-b",
        TestProtocolItem::Event(retry_result(
            provider_token.clone(),
            "prompt-a",
            tau_proto::RetryPromptStatus::Accepted,
        )),
    )
    .expect("mismatched result");
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(_)
        )),
        0
    );

    h.pending_provider_prompts.remove("prompt-b");
    h.prompt_agents.remove("prompt-b");
    h.handle_extension_event(
        "provider-b",
        TestProtocolItem::Event(retry_result(
            provider_token.clone(),
            "prompt-b",
            tau_proto::RetryPromptStatus::Accepted,
        )),
    )
    .expect("matching result");
    h.handle_extension_event(
        "provider-b",
        TestProtocolItem::Event(retry_result(
            provider_token,
            "prompt-b",
            tau_proto::RetryPromptStatus::NotParked,
        )),
    )
    .expect("late duplicate result");
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(result)
                if result.status == Some(tau_proto::RetryPromptStatus::Accepted)
                    && result.target_agent_id.as_deref() == Some("agent-b")
        )),
        1
    );
    assert_eq!(
        matching_events(&observer, |event| matches!(
            event,
            Event::UiRetryPromptResult(_)
        )),
        0
    );
}

/// Harness-side authority rejects stale sessions, absent selections, agents
/// without an in-flight prompt, unavailable routes, and replayed request ids
/// without sending any provider control.
#[test]
fn retry_rejects_invalid_targets_and_duplicate_request_ids() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);
    add_routed_prompt(&mut h, "no-route", "prompt-no-route", None);
    let idle = crate::parse_agent_id("idle-agent");
    let mut idle_agent = Agent::new(
        idle.clone(),
        2,
        h.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    idle_agent.agent_id = Some("idle-agent".to_owned());
    h.agents.insert(idle.clone(), idle_agent);
    h.agent_routes.insert("idle-agent".to_owned(), idle);

    for request in [
        retry_request("stale", "old", Some("no-route")),
        retry_request("none", "s1", None),
        retry_request("idle", "s1", Some("idle-agent")),
        retry_request("route", "s1", Some("no-route")),
        retry_request("missing", "s1", Some("unknown-agent")),
        retry_request("stale", "s1", Some("no-route")),
    ] {
        h.handle_client_event_inner("requester", request)
            .expect("rejection");
    }

    assert_eq!(
        matching_events(&provider, |event| matches!(event, Event::UiRetryPrompt(_))),
        0
    );
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(_)
        )),
        6
    );
}

/// Provider disconnect and session rollover each resolve a pending retry once
/// to its requester; a provider's subsequent late result cannot revive it or
/// leak a completion to another UI.
#[test]
fn retry_pending_requests_resolve_on_disconnect_and_session_rollover() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);
    let observer = connect_test_client(&mut h, "observer", tau_proto::ClientKind::Ui);
    add_routed_prompt(&mut h, "agent", "prompt", Some("provider"));

    h.handle_client_event_inner(
        "requester",
        retry_request("disconnect", "s1", Some("agent")),
    )
    .expect("disconnect request");
    h.handle_disconnect("provider");
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(result) if result.request_id.as_str() == "disconnect"
                && result.status.is_none()
        )),
        1
    );

    let _provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    h.pending_provider_prompts
        .insert("prompt".into(), "provider".into());
    h.handle_client_event_inner("requester", retry_request("rollover", "s1", Some("agent")))
        .expect("rollover request");
    let rollover_token = provider_request_id(&h, "rollover");
    h.switch_session("s2".into(), tau_proto::SessionStartReason::New)
        .expect("session rollover");
    h.handle_extension_event(
        "provider",
        TestProtocolItem::Event(retry_result(
            rollover_token,
            "prompt",
            tau_proto::RetryPromptStatus::Accepted,
        )),
    )
    .expect("late result");
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(result) if result.request_id.as_str() == "rollover"
                && result.status.is_none()
        )),
        1
    );
    assert_eq!(
        matching_events(&observer, |event| matches!(
            event,
            Event::UiRetryPromptResult(_)
        )),
        0
    );
}

/// Replay tombstones remain bounded even under high-cardinality rejected
/// requests; wire-level identifier validation is covered by `tau-proto`.
#[test]
fn retry_request_tombstones_are_bounded() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);

    for index in 0..1_100 {
        h.handle_client_event_inner(
            "requester",
            retry_request(&format!("request-{index}"), "s1", None),
        )
        .expect("bounded rejection");
    }
    assert_eq!(h.seen_retry_prompt_requests.len(), 1_024);
    assert_eq!(h.seen_retry_prompt_request_order.len(), 1_024);
}

/// Identical UI correlation IDs belong to the requesting connection, while
/// provider-stage tokens remain globally unique and results cannot cross UIs.
#[test]
fn retry_same_ui_id_isolated_across_requesters() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let first = connect_test_client(&mut h, "first", tau_proto::ClientKind::Ui);
    let second = connect_test_client(&mut h, "second", tau_proto::ClientKind::Ui);
    add_routed_prompt(&mut h, "agent", "prompt", Some("provider"));

    for requester in ["first", "second"] {
        h.handle_client_event_inner(requester, retry_request("same", "s1", Some("agent")))
            .expect("retry request");
    }
    let tokens = h.pending_retry_prompts.keys().cloned().collect::<Vec<_>>();
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1], "provider tokens must be unique");
    for token in tokens {
        let requester = h
            .pending_retry_prompts
            .get(&token)
            .expect("pending")
            .requester_client_id
            .clone();
        h.handle_extension_event(
            "provider",
            TestProtocolItem::Event(retry_result(
                token,
                "prompt",
                tau_proto::RetryPromptStatus::Accepted,
            )),
        )
        .expect("provider result");
        let own = if requester.as_str() == "first" {
            &first
        } else {
            &second
        };
        assert_eq!(
            matching_events(own, |event| matches!(
                event,
                Event::UiRetryPromptResult(result) if result.request_id.as_str() == "same"
            )),
            1
        );
    }
}

/// A nonresponsive provider cannot grow pending correlation state without
/// bound; excess controls are rejected without creating provider work.
#[test]
fn retry_pending_nonresponsive_provider_is_bounded() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);
    for index in 0..1_025 {
        add_routed_prompt(
            &mut h,
            &format!("agent-{index}"),
            &format!("prompt-{index}"),
            Some("provider"),
        );
        h.handle_client_event_inner(
            "requester",
            retry_request(
                &format!("request-{index}"),
                "s1",
                Some(&format!("agent-{index}")),
            ),
        )
        .expect("request handled");
    }
    assert_eq!(h.pending_retry_prompts.len(), 1_024);
    assert_eq!(
        matching_events(&provider, |event| matches!(event, Event::UiRetryPrompt(_))),
        1_024
    );
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(result) if result.request_id.as_str() == "request-1024"
        )),
        1
    );
}

/// Evicting an old UI replay tombstone permits reuse, but the newly minted
/// provider token differs, so a late old result cannot consume the new request.
#[test]
fn retry_tombstone_eviction_does_not_reuse_provider_token() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let requester = connect_test_client(&mut h, "requester", tau_proto::ClientKind::Ui);
    add_routed_prompt(&mut h, "agent", "prompt", Some("provider"));
    h.handle_client_event_inner("requester", retry_request("reuse", "s1", Some("agent")))
        .expect("old request");
    let old = provider_request_id(&h, "reuse");
    h.pending_retry_prompts.remove(&old);
    for index in 0..1_024 {
        h.handle_client_event_inner(
            "requester",
            retry_request(&format!("evict-{index}"), "s1", None),
        )
        .expect("eviction request");
    }
    h.handle_client_event_inner("requester", retry_request("reuse", "s1", Some("agent")))
        .expect("reused UI id");
    let new = provider_request_id(&h, "reuse");
    assert_ne!(old, new);
    h.handle_extension_event(
        "provider",
        TestProtocolItem::Event(retry_result(
            old,
            "prompt",
            tau_proto::RetryPromptStatus::Accepted,
        )),
    )
    .expect("late old result");
    assert!(h.pending_retry_prompts.contains_key(&new));
    assert_eq!(
        matching_events(&requester, |event| matches!(
            event,
            Event::UiRetryPromptResult(result) if result.request_id.as_str() == "reuse"
        )),
        0
    );
}
