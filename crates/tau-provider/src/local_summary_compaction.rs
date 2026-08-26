//! Shared policy for Tau-owned cache-aligned summary compaction.

use std::num::{NonZeroU32, NonZeroU64};

/// Conservative one-byte-per-token reserve for instructions and framing.
pub const REQUEST_OVERHEAD_TOKENS: u64 = 1024;
/// Default maximum summary generation.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;
/// Approximate prefix bytes charged per projected context token.
const PROJECTED_BYTES_PER_TOKEN: u64 = 4;
/// Smallest useful prefix budget, including fixed request framing.
const MIN_INPUT_BYTES: u64 = 256;

/// Validated limits for Tau-owned summary compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Target model context window.
    context_window_tokens: NonZeroU64,
    /// Prefix budget used to derive proactive scheduling.
    max_input_bytes: NonZeroU64,
    /// Maximum generated summary tokens.
    max_output_tokens: NonZeroU32,
    /// Maximum accepted narrative or reasoning bytes.
    max_output_bytes: NonZeroU64,
}

impl Config {
    /// Validate explicit limits.
    ///
    /// Returns `None` unless the declared and advertised windows match, the
    /// input budget is at least 256 bytes, output bytes fit the harness cap,
    /// and checked input, output, and framing budgets fit the model window.
    #[must_use]
    pub fn new(
        declared_context_window_tokens: NonZeroU64,
        advertised_context_window_tokens: u64,
        max_input_bytes: NonZeroU64,
        max_output_tokens: NonZeroU32,
        max_output_bytes: NonZeroU64,
    ) -> Option<Self> {
        let request_tokens = max_input_bytes
            .get()
            .checked_add(max_output_tokens.get().into())
            .and_then(|total| total.checked_add(REQUEST_OVERHEAD_TOKENS));
        (declared_context_window_tokens.get() == advertised_context_window_tokens
            && max_input_bytes.get() >= MIN_INPUT_BYTES
            && max_output_bytes.get() <= tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64
            && request_tokens.is_some_and(|total| total <= advertised_context_window_tokens))
        .then_some(Self {
            context_window_tokens: declared_context_window_tokens,
            max_input_bytes,
            max_output_tokens,
            max_output_bytes,
        })
    }

    /// Derive conservative limits from one model context window.
    ///
    /// Returns `None` below the smallest window that can retain the 256-byte
    /// input floor together with output and framing reserves.
    #[must_use]
    pub fn default_for(context_window_tokens: u64) -> Option<Self> {
        let available = context_window_tokens.checked_sub(REQUEST_OVERHEAD_TOKENS + 2)?;
        let max_output_tokens = u32::try_from(available / 8)
            .unwrap_or(u32::MAX)
            .clamp(1, DEFAULT_MAX_OUTPUT_TOKENS);
        let max_input_bytes = context_window_tokens
            .checked_sub(REQUEST_OVERHEAD_TOKENS + u64::from(max_output_tokens))?;
        Self::new(
            NonZeroU64::new(context_window_tokens)?,
            context_window_tokens,
            NonZeroU64::new(max_input_bytes)?,
            NonZeroU32::new(max_output_tokens)?,
            NonZeroU64::new(tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64)?,
        )
    }

    /// Return the configured prefix budget used for proactive scheduling.
    #[must_use]
    pub const fn max_input_bytes(self) -> u64 {
        self.max_input_bytes.get()
    }

    /// Return the output-token request cap.
    #[must_use]
    pub const fn max_output_tokens(self) -> u32 {
        self.max_output_tokens.get()
    }

    /// Return the accepted output byte cap.
    #[must_use]
    pub const fn max_output_bytes(self) -> u64 {
        self.max_output_bytes.get()
    }

    /// Return the proactive projected-token threshold derived from the prefix
    /// budget.
    #[must_use]
    pub const fn proactive_threshold(self) -> u64 {
        self.max_input_bytes.get() / PROJECTED_BYTES_PER_TOKEN
    }
}

/// Harness-authored user message appended after the ordinary request prefix.
///
/// Keeping the ordinary system prompt, tools, and history ahead of this exact
/// message lets compatible providers reuse their warmed prefix cache.
pub const REQUEST: &str = concat!(
    "<tau_internal>\n",
    "The context window is being compacted. Summarize the preceding conversation in your response ",
    "so the task can continue effectively using only the normal system prompt, tool definitions, ",
    "and your summary.\n\n",
    "Preserve the current goal, user requirements, decisions, constraints, completed work, important ",
    "results, exact identifiers, paths and commands, current state, open problems, blockers, and the ",
    "next concrete actions.\n\n",
    "Do not continue the task now. Do not make or request any tool calls. Return only the summary.\n",
    "&lt;/tau_internal&gt;"
);

/// Replaces the harness standalone trigger with the cache-aligned user request.
///
/// The trigger is a harness/provider-native control marker, not part of the
/// preceding ordinary request. Requiring it as the final one-item block
/// prevents a malformed standalone prompt from silently changing its selected
/// prefix.
pub fn replace_trailing_trigger(
    context: &mut tau_proto::PromptContext,
) -> Result<(), &'static str> {
    let Some(tau_proto::ContextBlock::UserInput(block)) = context.blocks.last() else {
        return Err("local compaction prompt lacks its trailing harness trigger");
    };
    if block.items.as_slice() != [tau_proto::ContextItem::CompactionTrigger] {
        return Err("local compaction prompt has a malformed harness trigger");
    }
    context.blocks.pop();
    context.blocks.push(tau_proto::ContextBlock::UserInput(
        tau_proto::UserInputBlock {
            items: vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::User,
                content: vec![tau_proto::ContentPart::Text {
                    text: REQUEST.to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        },
    ));
    Ok(())
}

/// Measure the canonical historical `PromptContext` prefix without its final
/// standalone trigger.
///
/// This is the adapter-side counterpart of the harness planner's published
/// prefix-budget measurement. Provider-specific lowering may expand the
/// request, so adapters must still check the exact final wire body separately.
#[must_use]
pub fn historical_prefix_json_bytes(context: &tau_proto::PromptContext) -> Option<u64> {
    let mut historical = context.clone();
    let tau_proto::ContextBlock::UserInput(block) = historical.blocks.last_mut()? else {
        return None;
    };
    if !matches!(
        block.items.last(),
        Some(tau_proto::ContextItem::CompactionTrigger)
    ) {
        return None;
    }
    block.items.pop();
    if block.items.is_empty() {
        historical.blocks.pop();
    }
    serde_json::to_vec(&historical)
        .ok()
        .and_then(|encoded| u64::try_from(encoded.len()).ok())
}

#[cfg(test)]
mod tests;
