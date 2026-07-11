use std::collections::HashMap;

use super::*;
use crate::harness::{TransportReplyRoute, live_send_tools_for_prompt};

/// Live reply projection must match source-owned internal tool identity,
/// target the same agent, and expose only the effective model alias.
#[test]
fn live_reply_projection_requires_route_agent_and_effective_internal_tool() {
    let agent = tau_proto::AgentId::parse("agent-a").expect("agent");
    let other = tau_proto::AgentId::parse("agent-b").expect("agent");
    let message_id = tau_proto::MessageId::new("msg-1");
    let route = TransportReplyRoute {
        connection_id: "slack".to_owned(),
        agent_id: agent.clone(),
        session_generation: 1,
        send_tool: Some(ToolName::new("internal_slack_send")),
        transport_name: "slack".to_owned(),
        external_endpoint: tau_proto::MessageEndpoint::User,
        conversation: None,
    };
    let routes = HashMap::from([(message_id.clone(), route)]);
    let tool = tau_proto::ToolSpec {
        name: ToolName::new("internal_slack_send"),
        model_visible_name: Some(ToolName::new("slack_send")),
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    };

    assert_eq!(
        live_send_tools_for_prompt(&routes, std::slice::from_ref(&tool), Some(&agent))
            .get(&message_id),
        Some(&ToolName::new("slack_send"))
    );
    assert!(live_send_tools_for_prompt(&routes, &[], Some(&agent)).is_empty());
    assert!(
        live_send_tools_for_prompt(&routes, std::slice::from_ref(&tool), Some(&other)).is_empty()
    );
    assert!(live_send_tools_for_prompt(&HashMap::new(), &[tool], Some(&agent)).is_empty());
}
