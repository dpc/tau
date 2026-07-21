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
    assert!(selectors.contains(&EventSelector::Exact(EventName::TOOL_RESULT)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::AGENT_STATE)));
    assert!(!selectors.contains(&EventSelector::Prefix("agent.".to_owned())));
}

/// Ensures the chat UI subscription stays as an explicit event allow-list so
/// newly-added protocol events do not silently expand UI traffic or replay
/// catch-up.
#[test]
fn chat_subscription_uses_no_prefix_selectors() {
    let selectors = chat_subscription_selectors();

    let expected = [
        EventName::UI_PROMPT_SUBMITTED,
        EventName::UI_SHELL_COMMAND,
        EventName::UI_CANCEL_PROMPT,
        EventName::ACTION_SCHEMA_PUBLISHED,
        EventName::ACTION_RESULT,
        EventName::ACTION_ERROR,
        EventName::AGENT_START_REQUEST,
        EventName::AGENT_START_ACCEPTED,
        EventName::AGENT_START_RESULT,
        EventName::AGENT_MESSAGE_SENT,
        EventName::AGENT_MESSAGE_RECEIVED,
        EventName::MESSAGE_DELIVERED,
        EventName::MESSAGE_EDITED,
        EventName::MESSAGE_DELETED,
        EventName::MESSAGE_REACTION_ADDED,
        EventName::MESSAGE_REACTION_REMOVED,
        EventName::MESSAGE_SENT,
        EventName::AGENT_PROMPT_SUBMITTED,
        EventName::AGENT_PROMPT_QUEUED,
        EventName::AGENT_PROMPT_RECALLED,
        EventName::AGENT_PROMPT_STEERED,
        EventName::AGENT_COMPACTION_TRIGGERED,
        EventName::AGENT_PROMPT_STARTED,
        EventName::AGENT_PROMPT_TERMINATED,
        EventName::AGENT_WATCHES_UPDATED,
        EventName::AGENT_STATS_UPDATED,
        EventName::AGENT_STARTED,
        EventName::AGENT_DISPLAY_NAME_SET,
        EventName::SESSION_STARTED,
        EventName::SESSION_SHUTDOWN,
        EventName::SESSION_AGENT_UNLOADED,
        EventName::PROVIDER_TOOL_ERROR,
        EventName::PROVIDER_PROMPT_SUBMITTED,
        EventName::PROVIDER_RESPONSE_UPDATED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::TOOL_STARTED,
        EventName::TOOL_REJECTED,
        EventName::TOOL_RESULT,
        EventName::TOOL_ERROR,
        EventName::TOOL_BACKGROUND_RESULT,
        EventName::TOOL_BACKGROUND_ERROR,
        EventName::TOOL_PROGRESS,
        EventName::TOOL_CANCELLED,
        EventName::SHELL_COMMAND_PROGRESS,
        EventName::SHELL_COMMAND_FINISHED,
        EventName::EXTENSION_STARTING,
        EventName::EXTENSION_READY,
        EventName::EXTENSION_EXITED,
        EventName::EXTENSION_SKILL_AVAILABLE,
        EventName::EXTENSION_AGENTS_MD_AVAILABLE,
        EventName::EXTENSION_CONTEXT_READY,
        EventName::HARNESS_NOTICE,
        EventName::HARNESS_SESSION_DIR,
        EventName::HARNESS_UI_DIR,
        EventName::HARNESS_MODELS_AVAILABLE,
        EventName::HARNESS_ROLES_AVAILABLE,
        EventName::HARNESS_ROLE_SELECTED,
        EventName::HARNESS_CONTEXT_USAGE_CHANGED,
        EventName::HARNESS_AGENT_CONTEXT_USAGE_CHANGED,
        EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
        EventName::HARNESS_EFFORTS_AVAILABLE,
        EventName::HARNESS_VERBOSITIES_AVAILABLE,
        EventName::HARNESS_THINKING_SUMMARIES_AVAILABLE,
        EventName::TERM_OSC1337_SET_USER_VAR,
        EventName::TERM_BELL,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect::<Vec<_>>();

    assert_eq!(selectors, expected);
    let HarnessInputMessage::Subscribe(subscription) = subscribe_message(selectors.clone()) else {
        panic!("chat subscription must produce Subscribe")
    };
    assert_eq!(subscription.historical_selectors, selectors);
    assert_eq!(subscription.live_selectors, selectors);
}

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

    for event in [EventName::TERM_OSC1337_SET_USER_VAR, EventName::TERM_BELL] {
        let selector = EventSelector::Exact(event);
        assert!(!subscription.historical_selectors.contains(&selector));
        assert!(subscription.live_selectors.contains(&selector));
    }

    for event in [
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::TOOL_RESULT,
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
