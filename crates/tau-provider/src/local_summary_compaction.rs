//! Shared policy for Tau-owned cache-aligned summary compaction.

use std::io::{self, Write};
use std::num::{NonZeroU32, NonZeroU64};

use serde::Serialize;

/// Default maximum summary generation.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Validated limits for Tau-owned summary compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Independent historical-prefix byte work cap, when configured.
    max_input_bytes: Option<tau_proto::ByteCount>,
    /// Maximum generated summary tokens.
    max_output_tokens: NonZeroU32,
    /// Maximum accepted narrative or reasoning bytes.
    max_output_bytes: NonZeroU64,
}

impl Config {
    /// Validate explicit limits.
    ///
    /// Returns `None` unless the declared and advertised windows match, output
    /// bytes fit the harness cap, and the output-token cap does not exceed the
    /// token window.
    #[must_use]
    pub fn new(
        declared_context_window_tokens: NonZeroU64,
        advertised_context_window_tokens: u64,
        max_input_bytes: NonZeroU64,
        max_output_tokens: NonZeroU32,
        max_output_bytes: NonZeroU64,
    ) -> Option<Self> {
        (declared_context_window_tokens.get() == advertised_context_window_tokens
            && max_output_bytes.get() <= tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64
            && u64::from(max_output_tokens.get()) <= advertised_context_window_tokens)
            .then_some(Self {
                max_input_bytes: Some(tau_proto::ByteCount::new(max_input_bytes.get())),
                max_output_tokens,
                max_output_bytes,
            })
    }

    /// Build the generic no-prefix-cap fallback using only same-domain token
    /// output policy and the independent narrative byte cap.
    #[must_use]
    pub fn default_for(context_window_tokens: u64) -> Option<Self> {
        let max_output_tokens = u32::try_from(context_window_tokens / 8)
            .unwrap_or(u32::MAX)
            .clamp(1, DEFAULT_MAX_OUTPUT_TOKENS);
        NonZeroU64::new(context_window_tokens)?;
        Some(Self {
            max_input_bytes: None,
            max_output_tokens: NonZeroU32::new(max_output_tokens)?,
            max_output_bytes: NonZeroU64::new(
                tau_proto::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES as u64,
            )?,
        })
    }

    /// Return the configured prefix budget used for proactive scheduling.
    #[must_use]
    pub const fn max_input_bytes(self) -> Option<tau_proto::ByteCount> {
        self.max_input_bytes
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
/// preceding ordinary request.
///
/// # Errors
///
/// Returns an error unless the context contains exactly one compaction trigger
/// anywhere and that trigger is the sole item in the final `UserInput` block.
/// This prevents a malformed standalone prompt from silently changing its
/// selected prefix.
pub fn replace_trailing_trigger(
    context: &mut tau_proto::PromptContext,
) -> Result<(), &'static str> {
    validate_trailing_trigger(context)?;
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

/// Validates the exact trailing standalone-compaction trigger shape without
/// cloning or mutating the provider context.
///
/// # Errors
///
/// Returns an error unless the context contains exactly one compaction trigger
/// anywhere and that trigger is the sole item in the final `UserInput` block.
pub fn validate_trailing_trigger(context: &tau_proto::PromptContext) -> Result<(), &'static str> {
    let trigger_count = context
        .blocks
        .iter()
        .map(|block| match block {
            tau_proto::ContextBlock::UserInput(block) => block
                .items
                .iter()
                .filter(|item| matches!(item, tau_proto::ContextItem::CompactionTrigger))
                .count(),
            tau_proto::ContextBlock::AssistantResponse(block) => block
                .output_items
                .iter()
                .filter(|item| matches!(item, tau_proto::ContextItem::CompactionTrigger))
                .count(),
            tau_proto::ContextBlock::ToolResults(_) => 0,
        })
        .sum::<usize>();
    if trigger_count != 1 {
        return Err("local compaction prompt must contain exactly one harness trigger");
    }
    let Some(tau_proto::ContextBlock::UserInput(block)) = context.blocks.last() else {
        return Err("local compaction prompt lacks its trailing harness trigger");
    };
    if block.items.as_slice() != [tau_proto::ContextItem::CompactionTrigger] {
        return Err("local compaction prompt has a malformed harness trigger");
    }
    Ok(())
}

/// Checks whether the exact canonical historical prefix fits `budget` without
/// cloning the context or materializing its serialized JSON.
///
/// The caller must first validate the exact trailing trigger shape with
/// [`validate_trailing_trigger`]. `None` reports a malformed shape or an
/// unexpected serialization failure; `Some(false)` reports budget exhaustion.
#[must_use]
pub fn historical_prefix_fits_json_budget(
    context: &tau_proto::PromptContext,
    budget: tau_proto::ByteCount,
) -> Option<bool> {
    let (last_block, historical_blocks) = context.blocks.split_last()?;
    if !matches!(
        last_block,
        tau_proto::ContextBlock::UserInput(block)
            if block.items.as_slice() == [tau_proto::ContextItem::CompactionTrigger]
    ) {
        return None;
    }
    let mut writer = JsonBudgetWriter::new(budget);
    let result = serde_json::to_writer(
        &mut writer,
        &HistoricalPromptContext {
            blocks: historical_blocks,
        },
    );
    match result {
        Ok(()) => Some(true),
        Err(_) if writer.exceeded => Some(false),
        Err(_) => None,
    }
}

/// Borrowed serialization view of a validated historical prompt prefix.
#[derive(Serialize)]
struct HistoricalPromptContext<'a> {
    /// Ordered blocks before the standalone compaction trigger.
    blocks: &'a [tau_proto::ContextBlock],
}

/// Counting JSON sink that rejects the first write beyond an exact byte budget.
struct JsonBudgetWriter {
    /// Bytes still available to the serializer.
    remaining: u64,
    /// Whether the serializer attempted to cross the budget.
    exceeded: bool,
}

impl JsonBudgetWriter {
    /// Start a sink with the addressable part of the selected byte budget.
    fn new(budget: tau_proto::ByteCount) -> Self {
        Self {
            remaining: budget.get(),
            exceeded: false,
        }
    }
}

impl Write for JsonBudgetWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.remaining < byte_count {
            self.exceeded = true;
            return Err(io::Error::other("historical prompt prefix exceeds budget"));
        }
        self.remaining -= byte_count;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Measure the canonical historical `PromptContext` prefix without its final
/// standalone trigger.
///
/// This is the adapter-side counterpart of the harness planner's published
/// prefix-budget measurement. Adapters compare this fully materialized
/// historical prefix only with the published historical-prefix byte budget.
/// Provider-specific whole-wire expansion requires an independent byte-domain
/// limit and must never be compared with token-window metadata.
#[must_use]
pub fn historical_prefix_json_bytes(
    context: &tau_proto::PromptContext,
) -> Option<tau_proto::ByteCount> {
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
        .map(tau_proto::ByteCount::new)
}

#[cfg(test)]
mod tests;
