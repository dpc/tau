use super::*;

/// Keeps the chat subscription explicit and excludes peer draft-liveness facts,
/// which must never become another terminal's editable prompt state.
#[test]
fn chat_subscription_excludes_peer_drafts_and_unhandled_payloads() {
    let selectors = chat_subscription_selectors();

    assert!(selectors.contains(&EventSelector::Exact(EventName::AGENT_PROMPT_STARTED)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::AGENT_PROMPT_CREATED)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::PROVIDER_TOOL_RESULT)));
    assert!(selectors.contains(&EventSelector::Exact(EventName::TOOL_RESULT_DISPLAY)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::TOOL_RESULT)));
    assert!(selectors.contains(&EventSelector::Exact(
        EventName::TOOL_BACKGROUND_RESULT_DISPLAY
    )));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::TOOL_BACKGROUND_RESULT)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::AGENT_STATE)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::UI_PROMPT_DRAFT)));
    assert!(!selectors.contains(&EventSelector::Prefix("agent.".to_owned())));
}

/// Manual-compaction lifecycle facts must reach the interactive renderer
/// through both live delivery and durable historical catch-up.
#[test]
fn chat_subscription_includes_manual_compaction_lifecycle() {
    let HarnessInputMessage::Subscribe(subscription) = chat_subscribe_message() else {
        panic!("chat subscription must produce Subscribe")
    };
    for event in [
        EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
        EventName::AGENT_STANDALONE_COMPACTION_STARTED,
        EventName::AGENT_STANDALONE_COMPACTION_FAILED,
        EventName::AGENT_COMPACTED,
    ] {
        let selector = EventSelector::Exact(event);
        assert!(subscription.historical_selectors.contains(&selector));
        assert!(subscription.live_selectors.contains(&selector));
    }
}

/// Cold attach requests running shell snapshots and omission notices while
/// retaining live terminal delivery.
#[test]
fn chat_subscription_includes_shell_reconciliation_events() {
    let HarnessInputMessage::Subscribe(subscription) = chat_subscribe_message() else {
        panic!("chat subscription must produce Subscribe")
    };
    for event in [EventName::UI_SHELL_COMMAND, EventName::HARNESS_NOTICE] {
        assert!(
            subscription
                .historical_selectors
                .contains(&EventSelector::Exact(event))
        );
    }
    assert!(
        subscription
            .live_selectors
            .contains(&EventSelector::Exact(EventName::SHELL_COMMAND_FINISHED))
    );
}

/// Ensures the chat UI subscription stays as an explicit event allow-list so
/// newly-added protocol events do not silently expand UI traffic or replay
/// catch-up.
/// Ensures chat reconstructs pending tools from durable dispatched starts and
/// never subscribes to pre-dispatch requests.
#[test]
fn chat_subscription_keeps_runtime_side_effects_live_only() {
    let HarnessInputMessage::Subscribe(subscription) = chat_subscribe_message() else {
        panic!("chat subscription must produce Subscribe")
    };
    let request = EventSelector::Exact(EventName::TOOL_REQUEST);
    assert!(!subscription.historical_selectors.contains(&request));
    assert!(!subscription.live_selectors.contains(&request));

    let started = EventSelector::Exact(EventName::TOOL_STARTED);
    assert!(subscription.historical_selectors.contains(&started));
    assert!(subscription.live_selectors.contains(&started));

    let prompt_failed = EventSelector::Exact(EventName::AGENT_PROMPT_FAILED);
    assert!(!subscription.historical_selectors.contains(&prompt_failed));
    assert!(subscription.live_selectors.contains(&prompt_failed));
    let prompt_rejected = EventSelector::Exact(EventName::AGENT_PROMPT_REJECTED);
    assert!(!subscription.historical_selectors.contains(&prompt_rejected));
    assert!(subscription.live_selectors.contains(&prompt_rejected));

    for event in [EventName::TERM_OSC1337_SET_USER_VAR, EventName::TERM_BELL] {
        let selector = EventSelector::Exact(event);
        assert!(!subscription.historical_selectors.contains(&selector));
        assert!(subscription.live_selectors.contains(&selector));
    }

    for event in [
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::TOOL_RESULT_DISPLAY,
        EventName::TOOL_BACKGROUND_RESULT_DISPLAY,
        EventName::TOOL_ERROR,
    ] {
        let selector = EventSelector::Exact(event);
        assert!(subscription.historical_selectors.contains(&selector));
        assert!(subscription.live_selectors.contains(&selector));
    }
}

/// Keeps provider errors available for cold-attach normalization without
/// delivering their ignored live form, while canonical tool errors remain live.
#[test]
fn chat_subscription_keeps_provider_tool_errors_historical_only() {
    let HarnessInputMessage::Subscribe(subscription) = chat_subscribe_message() else {
        panic!("chat subscription must produce Subscribe")
    };
    let provider_error = EventSelector::Exact(EventName::PROVIDER_TOOL_ERROR);
    let tool_error = EventSelector::Exact(EventName::TOOL_ERROR);

    assert!(subscription.historical_selectors.contains(&provider_error));
    assert!(!subscription.live_selectors.contains(&provider_error));
    assert!(subscription.historical_selectors.contains(&tool_error));
    assert!(subscription.live_selectors.contains(&tool_error));
}

/// Generic UI subscription construction remains capable of requesting
/// `tool.request`; only the interactive chat allow-list drops the event.
#[test]
fn generic_ui_subscription_preserves_tool_request() {
    let request = EventSelector::Exact(EventName::TOOL_REQUEST);
    let HarnessInputMessage::Subscribe(subscription) = subscribe_message(vec![request.clone()])
    else {
        panic!("generic subscription must produce Subscribe")
    };

    assert_eq!(
        subscription.historical_selectors,
        std::slice::from_ref(&request)
    );
    assert_eq!(subscription.live_selectors, [request]);
}
