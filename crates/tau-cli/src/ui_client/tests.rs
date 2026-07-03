use super::*;

/// Ensures the chat UI stays subscribed to lightweight prompt lifecycle and
/// live agent-state events without receiving full provider prompt payloads.
#[test]
fn chat_subscription_uses_prompt_started_not_prompt_created() {
    let selectors = chat_subscription_selectors();

    assert!(selectors.contains(&EventSelector::Exact(EventName::AGENT_PROMPT_STARTED)));
    assert!(selectors.contains(&EventSelector::Exact(EventName::AGENT_STATE)));
    assert!(!selectors.contains(&EventSelector::Exact(EventName::AGENT_PROMPT_CREATED)));
    assert!(!selectors.contains(&EventSelector::Prefix("agent.".to_owned())));
}
