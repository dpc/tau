use super::*;

/// Ensures the chat UI stays subscribed to lightweight prompt lifecycle events
/// without receiving full provider prompt payloads or unhandled agent
/// snapshots.
#[test]
fn chat_subscription_uses_prompt_started_not_prompt_created() {
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
/// Ensures chat omits the unused tool request, keeps transient starts and
/// terminal side effects live-only, and still requests durable terminal facts.
#[test]
fn chat_subscription_keeps_runtime_side_effects_live_only() {
    let HarnessInputMessage::Subscribe(subscription) = chat_subscribe_message() else {
        panic!("chat subscription must produce Subscribe")
    };
    let request = EventSelector::Exact(EventName::TOOL_REQUEST);
    assert!(!subscription.historical_selectors.contains(&request));
    assert!(!subscription.live_selectors.contains(&request));

    let started = EventSelector::Exact(EventName::TOOL_STARTED);
    assert!(!subscription.historical_selectors.contains(&started));
    assert!(subscription.live_selectors.contains(&started));

    let prompt_failed = EventSelector::Exact(EventName::AGENT_PROMPT_FAILED);
    assert!(!subscription.historical_selectors.contains(&prompt_failed));
    assert!(subscription.live_selectors.contains(&prompt_failed));

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
