use super::*;

/// Accepted manual-compaction work is a durable user-visible lifecycle
/// fact, so late subscribers must receive it before a matching
/// transaction start.
#[test]
fn late_subscriber_replays_manual_compaction_acceptance() {
    let event = Event::AgentManualCompactionRequested(tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse("cr-1-0").expect("request id"),
        caller_agent_id: crate::parse_agent_id("manager"),
        target_agent_id: crate::parse_agent_id("worker"),
        initiating_agent_prompt_id: "ap-manager-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        initiating_tool_call_id: "call-1".into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
        visible_tool_name: tau_proto::ToolName::new("agent_compact"),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: 0,
        model: "test/model".parse().expect("model id"),
        resume_inference: false,
    });

    assert!(should_replay_agent_event_to_late_subscriber(&event));
}
