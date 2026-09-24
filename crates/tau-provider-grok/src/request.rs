//! Public xAI Responses request policy, without inference or credential
//! authority.

mod image_budget;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde_json::value::{RawValue, to_raw_value};
use tau_proto::{AgentPromptCreated, ContextBlock, ContextItem, ToolResultItem};
use tau_provider_responses::{
    AttemptConfig, AttemptModel, PrepareSseRequestError, PreparedSseRequest, Transport,
};

use self::image_budget::ImageBudget;

/// A fully lowered Grok request retaining exact admitted Responses replay
/// bytes.
///
/// The caller must first apply destination-origin admission. This type does not
/// grant foreign opaque items authority or perform inference. Its future shared
/// SSE attempt must use matching tools, attribution and no OpenAI cache
/// controls.
pub struct Request {
    /// Validated full-replay SSE envelope.
    prepared: PreparedSseRequest,
}

impl Request {
    /// Lower a destination-projected inference prompt using exact advertised
    /// reasoning selectors, never slug-based guesses or a fabricated default.
    ///
    /// `native_images` is the selected route's audited capability, not inferred
    /// from its name or token prices. Unsupported images remain explicit
    /// textual omissions. Server-side compaction and hosted tools are not
    /// supported.
    pub fn lower(
        prompt: &AgentPromptCreated,
        model: &AttemptModel,
        max_output_tokens: u32,
        reasoning_efforts: &[String],
        native_images: bool,
    ) -> Result<Self, PrepareSseRequestError> {
        if !prompt.operation.is_inference()
            || prompt.compaction.is_some()
            || !prompt.hosted_tools.is_empty()
            || prompt.tools_ref.is_some()
        {
            return Err(PrepareSseRequestError);
        }
        let config = AttemptConfig {
            base_url: "https://api.x.ai/v1".into(),
            api_key: String::new(),
            max_output_tokens,
            transport: Transport::Sse,
            prompt_cache: None,
        };
        let baseline = PreparedSseRequest::lower(prompt, &config, model)?;
        let mut fields: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(baseline.json().get()).map_err(|_| PrepareSseRequestError)?;
        let reasoning: serde_json::Value =
            serde_json::from_str(fields["reasoning"].get()).map_err(|_| PrepareSseRequestError)?;
        if let Some(effort) = reasoning["effort"].as_str() {
            if !reasoning_efforts
                .iter()
                .any(|supported| supported == effort)
            {
                return Err(PrepareSseRequestError);
            }
        } else {
            // Omission preserves alias-specific upstream defaults.
            fields.remove("reasoning");
        }
        fields.insert("store".into(), raw(&false)?);
        fields.insert("include".into(), raw(&["reasoning.encrypted_content"])?);
        fields.insert(
            "prompt_cache_key".into(),
            raw(&format!("tau:{}", prompt.agent_id))?,
        );
        let mut input: Vec<Box<RawValue>> =
            serde_json::from_str(fields["input"].get()).map_err(|_| PrepareSseRequestError)?;
        replace_tool_outputs(prompt, &mut input, native_images)?;
        fields.insert("input".into(), raw(&input)?);
        Ok(Self {
            prepared: PreparedSseRequest::from_json(raw(&fields)?)?
                .with_non_retryable_incomplete_reasons(&["max_prompt_tokens", "max_time_limit"]),
        })
    }

    /// Borrow the exact request for shared transport or credential-free sizing.
    pub fn prepared(&self) -> &PreparedSseRequest {
        &self.prepared
    }
}

/// Serialize owned semantic fields, leaving raw siblings untouched.
fn raw(value: &impl serde::Serialize) -> Result<Box<RawValue>, PrepareSseRequestError> {
    to_raw_value(value).map_err(|_| PrepareSseRequestError)
}

/// Collect references without cloning the prompt's opaque or image payloads.
fn tool_results(prompt: &AgentPromptCreated) -> Vec<&ToolResultItem> {
    let mut results = Vec::new();
    for block in &prompt.context.blocks {
        let items = match block {
            ContextBlock::UserInput(block) => &block.items,
            ContextBlock::AssistantResponse(block) => &block.output_items,
            ContextBlock::ToolResults(block) => {
                results.extend(&block.items);
                continue;
            }
        };
        for item in items {
            if let ContextItem::ToolResult(result) = item {
                results.push(result);
            }
        }
    }
    results
}

/// Replace only canonical function outputs, retaining input order and raw
/// syntax of every assistant, reasoning and function-call replay sibling.
fn replace_tool_outputs(
    prompt: &AgentPromptCreated,
    input: &mut [Box<RawValue>],
    native_images: bool,
) -> Result<(), PrepareSseRequestError> {
    let results = tool_results(prompt);
    let mut results = results.into_iter();
    let mut budget = ImageBudget::new(native_images);
    for item in input {
        let fields: BTreeMap<String, &RawValue> =
            serde_json::from_str(item.get()).map_err(|_| PrepareSseRequestError)?;
        let kind = fields
            .get("type")
            .and_then(|value| serde_json::from_str::<String>(value.get()).ok());
        if kind.as_deref() != Some("function_call_output") {
            continue;
        }
        let result = results.next().ok_or(PrepareSseRequestError)?;
        let call_id: String =
            serde_json::from_str(fields.get("call_id").ok_or(PrepareSseRequestError)?.get())
                .map_err(|_| PrepareSseRequestError)?;
        if call_id != result.call_id.as_str() {
            return Err(PrepareSseRequestError);
        }
        *item = raw(&serde_json::json!({
            "type": "function_call_output",
            "call_id": result.call_id,
            "output": budget.output(result),
        }))?;
    }
    if results.next().is_some() {
        return Err(PrepareSseRequestError);
    }
    Ok(())
}
