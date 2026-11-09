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

/// Matches the sole child ID in the production result; the tool owner
/// identifies the parent without a redundant result field.
pub(super) fn result_ids_match(
    result: &CborValue,
    _main_agent_id: &AgentId,
    worker_agent_id: &AgentId,
) -> bool {
    result
        == &CborValue::Map(vec![(
            CborValue::Text("sub_agent_id".to_owned()),
            CborValue::Text(worker_agent_id.to_string()),
        )])
}
