//! Cheap, observational estimates of the ordinary context a boundary replaces.
//!
//! The Codex profile uses model-visible UTF-8 bytes / 4 (rounded up), plus four
//! tokens per message/tool item and three request framing tokens. These framing
//! allowances are deliberately coarse, not a claim about provider tokenization.
//! Opaque normalization follows openai/codex at
//! 1c414e0d6525ea385aeabad32fac252467a6b836,
//! codex-rs/core/src/context_manager/history.rs and
//! codex-rs/protocol/src/protocol.rs: reasoning removes the encoded envelope;
//! compaction accounts for sampled-token replay cache expansion. Ciphertext and
//! transport JSON are never counted as plain text. Unsupported content is
//! unknown. Changing any formula requires a new estimator ID, not replay
//! recomputation.

use tau_proto::{
    AgentHead, AgentPromptCreated, AgentPromptId, CompactionEffectiveness, ContentPart,
    ContextItem, ContextRole, ProviderBackendKind, ProviderResponseFinished, TokenCount, ToolType,
};

use super::Harness;

/// Versioned profile for the private Codex Responses family, including Lite.
const ESTIMATOR: &str = "tau-codex-visible-v1";
/// Approximate per-message/tool role and delimiter overhead.
const ITEM_FRAMING: u64 = 4;

impl Harness {
    /// Freeze the comparison at the accepted boundary, without affecting
    /// acceptance.
    pub(super) fn compaction_effectiveness(
        &self,
        cid: &tau_proto::AgentId,
        response: &ProviderResponseFinished,
        prompt_id: &AgentPromptId,
        cut: AgentHead,
        suffix_end: AgentHead,
        replacement: &[ContextItem],
    ) -> Option<CompactionEffectiveness> {
        // Backend identity is observed on this accepted terminal, not guessed
        // from a configurable provider namespace or model spelling.
        if response.backend.as_ref()?.kind != ProviderBackendKind::Responses {
            return None;
        }
        let owner = self
            .prompt_coordination
            .standalone_accounting
            .owners
            .get(prompt_id)?;
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let tree = self
            .session_runtime
            .agent_store
            .agent(agent.identity.agent_id.as_deref()?)?;
        let compatible = self.compatible_provider_replay_sources(&owner.model);
        let project = |cut| {
            crate::prompt::assemble_prompt_context_for_provider(
                tree,
                suffix_end.as_option(),
                cut,
                &owner.model.provider,
                &compatible,
            )
            .ok()
            .map(|(context, _)| context.context.flatten())
        };
        let prefix = project(Some(cut))?;
        let before = project(None)?;
        // A split inside a regrouped item would lose framing. Fail only this
        // observation rather than inventing a subtraction across different
        // shapes.
        let suffix = before.strip_prefix(prefix.as_slice())?;
        let replacement = crate::prompt::project_compaction_replacement_items(replacement);
        Some(measure(
            owner.context_recipe_tokens,
            &prefix,
            suffix,
            &replacement,
        ))
    }
}

/// Estimate one frozen ordinary recipe, excluding the compact
/// trigger/instruction.
///
/// Definitions remain declared during compaction. Local continuation
/// instructions, output policy, cache IDs and operation controls do not
/// contribute to this counterfactual ordinary input. Hosted tools are not yet
/// audited.
pub(super) fn recipe_tokens(prompt: &AgentPromptCreated) -> Option<u64> {
    if prompt.tools_ref.is_some() || !prompt.hosted_tools.is_empty() {
        return None;
    }
    let mut total = text_tokens(&prompt.system_prompt)?.checked_add(ITEM_FRAMING + 3)?;
    for tool in &prompt.tools {
        let name = tool.model_visible_name.as_ref().unwrap_or(&tool.name);
        let mut bytes = u64::try_from(name.chars().count()).ok()?;
        bytes = bytes
            .checked_add(u64::try_from(tool.description.as_deref().unwrap_or("").len()).ok()?)?;
        match tool.tool_type {
            ToolType::Function => {
                if let Some(schema) = &tool.parameters {
                    bytes = bytes.checked_add(json_bytes(schema)?)?;
                }
            }
            ToolType::Custom => {
                if let Some(format) = &tool.format {
                    bytes = bytes.checked_add(json_bytes(format)?)?;
                }
            }
        }
        total = total.checked_add(bytes.div_ceil(4).checked_add(ITEM_FRAMING)?)?;
    }
    Some(total)
}

/// Compare additive item contributions with exactly the same suffix and recipe.
fn measure(
    recipe: Option<u64>,
    prefix: &[ContextItem],
    suffix: &[ContextItem],
    replacement: &[ContextItem],
) -> CompactionEffectiveness {
    let prefix = items_tokens(prefix);
    let replacement = items_tokens(replacement);
    let common = recipe.and_then(|recipe| recipe.checked_add(items_tokens(suffix)?));
    CompactionEffectiveness {
        estimator: ESTIMATOR.to_owned(),
        prefix_before: prefix.map(TokenCount::new),
        total_before: common
            .and_then(|common| common.checked_add(prefix?))
            .map(TokenCount::new),
        replacement_after: replacement.map(TokenCount::new),
        total_after: common
            .and_then(|common| common.checked_add(replacement?))
            .map(TokenCount::new),
    }
}

/// Sum complete item contributions, propagating unknown content and overflow.
fn items_tokens(items: &[ContextItem]) -> Option<u64> {
    items
        .iter()
        .try_fold(0u64, |sum, item| sum.checked_add(item_tokens(item)?))
}

/// Estimate only supported model-visible fields, never outer serialized items.
fn item_tokens(item: &ContextItem) -> Option<u64> {
    let bytes = match item {
        ContextItem::Message(message) => {
            if !matches!(message.role, ContextRole::User | ContextRole::Assistant) {
                return Some(0); // Codex ordinary lowering omits these history roles.
            }
            // Raw assistant sidecars may contain media or future content parts.
            if let Some(raw) = &message.responses_raw_json {
                let raw: serde_json::Value = serde_json::from_str(raw).ok()?;
                if raw.get("content")?.as_array()?.iter().any(|part| {
                    !matches!(
                        part.get("type").and_then(|kind| kind.as_str()),
                        Some("output_text" | "text")
                    )
                }) {
                    return None;
                }
            }
            message.content.iter().try_fold(0u64, |sum, part| {
                let text = match part {
                    ContentPart::Text { text }
                    | ContentPart::SyntheticCompactionSummary { text }
                    | ContentPart::HarnessInternalText { text } => text,
                    ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => {
                        return Some(sum);
                    }
                };
                sum.checked_add(u64::try_from(text.len()).ok()?)
            })?
        }
        ContextItem::ToolCall(call) => {
            let arguments = match call.tool_type {
                ToolType::Function => match &call.raw_arguments_json {
                    Some(raw) => u64::try_from(raw.len()).ok()?,
                    None => json_bytes(&call.arguments)?,
                },
                ToolType::Custom => match &call.arguments {
                    tau_proto::CborValue::Text(text) => u64::try_from(text.len()).ok()?,
                    other => json_bytes(other)?,
                },
            };
            arguments.checked_add(u64::try_from(call.name.chars().count()).ok()?)?
        }
        ContextItem::ToolResult(result) => {
            if !result.provider_content.is_empty() {
                return None;
            }
            u64::try_from(result.render_provider_text().len()).ok()?
        }
        ContextItem::ReasoningText(_) => return Some(0),
        ContextItem::Reasoning(opaque) | ContextItem::Compaction(opaque) => {
            let raw: serde_json::Value = serde_json::from_str(opaque.raw_json()).ok()?;
            let encoded = u64::try_from(raw.get("encrypted_content")?.as_str()?.len()).ok()?;
            if matches!(item, ContextItem::Compaction(_)) {
                encoded.div_ceil(3)
            } else {
                encoded.checked_mul(3)?.checked_div(4)?.saturating_sub(650)
            }
        }
        ContextItem::LocalCompactionNarrative(_)
        | ContextItem::CompactionTrigger
        | ContextItem::UnknownProviderItem(_) => return None,
    };
    bytes.div_ceil(4).checked_add(ITEM_FRAMING)
}

/// Count model-visible structured arguments/schema, not transcript
/// serialization.
fn json_bytes(value: &impl serde::Serialize) -> Option<u64> {
    u64::try_from(serde_json::to_vec(value).ok()?.len()).ok()
}

/// Coarse text contribution in tokens, excluding framing.
fn text_tokens(text: &str) -> Option<u64> {
    Some(u64::try_from(text.len()).ok()?.div_ceil(4))
}

#[cfg(test)]
mod tests;
