//! Restricted ChatGPT plan request policy over the shared finite SSE adapter.

use std::collections::BTreeMap;

use serde_json::value::{RawValue, to_raw_value};
use tau_provider_responses::{
    AttemptConfig, AttemptModel, PrepareSseRequestError, PreparedSseRequest, Transport,
};

#[cfg(test)]
mod tests;

/// Lower an already destination-projected transcript without changing opaque
/// replay bytes or granting compatibility with the legacy Codex provider.
///
/// Functions are supplied as a developer `additional_tools` item before the
/// complete history. Hosted tools and reference-only definitions are rejected.
/// The selected route never enables API billing, stateful HTTP continuation,
/// provider-native compaction, or unsupported generation controls.
pub fn lower(
    prompt: &tau_proto::AgentPromptCreated,
    model: &AttemptModel,
) -> Result<PreparedSseRequest, PrepareSseRequestError> {
    if !prompt.hosted_tools.is_empty() || prompt.tools_ref.is_some() {
        return Err(PrepareSseRequestError);
    }
    let config = AttemptConfig {
        base_url: crate::RESOURCE.into(),
        api_key: String::new(),
        max_output_tokens: 0,
        transport: Transport::Sse,
        prompt_cache: None,
    };
    let baseline = PreparedSseRequest::lower(prompt, &config, model)?;
    let mut fields: BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(baseline.json().get()).map_err(|_| PrepareSseRequestError)?;
    let mut input: Vec<Box<RawValue>> =
        serde_json::from_str(fields["input"].get()).map_err(|_| PrepareSseRequestError)?;
    // System instructions remain trusted instructions, not a new user payload.
    // Only synthesized non-assistant messages are rewritten; admitted opaque
    // provider output keeps its exact lexical JSON representation.
    for item in &mut input {
        let value: serde_json::Value =
            serde_json::from_str(item.get()).map_err(|_| PrepareSseRequestError)?;
        if value["role"] == "system" {
            let mut message: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(item.get()).map_err(|_| PrepareSseRequestError)?;
            message.insert("role".into(), raw(&"developer")?);
            *item = raw(&message)?;
        }
    }
    if let Some(tools) = fields.remove("tools") {
        let definitions: Vec<Box<RawValue>> =
            serde_json::from_str(tools.get()).map_err(|_| PrepareSseRequestError)?;
        if !definitions.is_empty() {
            let additional = BTreeMap::from([
                ("type", raw(&"additional_tools")?),
                ("role", raw(&"developer")?),
                ("tools", tools),
            ]);
            input.insert(0, raw(&additional)?);
        }
    }
    fields.insert("input".into(), raw(&input)?);
    fields.insert("store".into(), raw(&false)?);
    fields.insert("include".into(), raw(&["reasoning.encrypted_content"])?);
    Ok(PreparedSseRequest::from_json(raw(&fields)?)?
        .with_required_completed_event()
        .without_transport_retries()
        .with_non_retryable_incomplete_reasons(&["max_output_tokens"]))
}

/// Serialize only locally owned fields while preserving nested raw items.
fn raw(value: &impl serde::Serialize) -> Result<Box<RawValue>, PrepareSseRequestError> {
    to_raw_value(value).map_err(|_| PrepareSseRequestError)
}
