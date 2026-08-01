//! Shared exact typed projection for S8's sole production `agent_start`.

use tau_proto::{AgentId, CborValue};

use super::WORKER_PROMPT;

/// Matches the closed production `agent_start` argument map.
pub(super) fn arguments_match(arguments: &CborValue) -> bool {
    arguments
        == &CborValue::Map(vec![
            (
                CborValue::Text("prompt".to_owned()),
                CborValue::Text(WORKER_PROMPT.to_owned()),
            ),
            (
                CborValue::Text("role".to_owned()),
                CborValue::Text("deterministic-worker".to_owned()),
            ),
        ])
}

/// Matches the exact main/worker IDs in the production tool result.
pub(super) fn result_ids_match(
    result: &CborValue,
    main_agent_id: &AgentId,
    worker_agent_id: &AgentId,
) -> bool {
    result
        == &CborValue::Map(vec![
            (
                CborValue::Text("self_agent_id".to_owned()),
                CborValue::Text(main_agent_id.to_string()),
            ),
            (
                CborValue::Text("sub_agent_id".to_owned()),
                CborValue::Text(worker_agent_id.to_string()),
            ),
        ])
}
