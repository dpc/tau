//! Content-free compaction counts, completion labels, and provider-terminal
//! gating.

use tau_proto::ProviderResponseCompactionStatus;

use super::{CompactionStatus, EventRenderer, format_token_count};

/// Adds backend-neutral counts without treating transient activity as
/// completion.
pub(super) fn append_compaction_progress(
    text: &mut String,
    progress: Option<(Option<u64>, Option<u64>)>,
    canonical_success: bool,
) {
    let Some((current, total)) = progress else {
        return;
    };
    let total = total.or_else(|| canonical_success.then_some(current).flatten());
    if current.is_none() && total.is_none() {
        return;
    }
    let current = current.map_or_else(|| "?".to_owned(), |n| n.to_string());
    let total = total.map_or_else(|| "?".to_owned(), |n| n.to_string());
    text.push_str(&format!(" {current}/{total}"));
}

/// Formats ordinary inline provider compaction, whose status is provider-owned.
pub(super) fn update_compaction_status(
    update: &tau_proto::ProviderResponseUpdated,
) -> Option<(CompactionStatus, String)> {
    let compaction = update.compaction.as_ref()?;
    match compaction.status {
        ProviderResponseCompactionStatus::Started => Some((
            CompactionStatus::Progress,
            EventRenderer::compaction_progress_status(compaction.original_input_tokens),
        )),
        ProviderResponseCompactionStatus::Completed => Some((
            CompactionStatus::Success,
            EventRenderer::compaction_success_status(compaction.original_input_tokens),
        )),
    }
}

impl EventRenderer {
    /// Formats a compact token-count chip.
    fn compaction_token_chip(tokens: u64) -> String {
        format!("#{}", format_token_count(tokens))
    }

    /// Formats an inline compaction still in progress.
    fn compaction_progress_status(original_input_tokens: Option<u64>) -> String {
        original_input_tokens
            .map(|tokens| {
                format!(
                    "{} {}",
                    Self::compaction_token_chip(tokens),
                    tau_proto::PROGRESS_INDICATOR_TEXT
                )
            })
            .unwrap_or_else(|| tau_proto::PROGRESS_INDICATOR_TEXT.to_owned())
    }

    /// Formats provider-owned inline compaction success.
    pub(super) fn compaction_success_status(original_input_tokens: Option<u64>) -> String {
        original_input_tokens.map_or_else(
            || "ok".to_owned(),
            |original| format!("{} → ? ok", Self::compaction_token_chip(original)),
        )
    }

    /// Formats successful standalone compaction from request input and exact
    /// first transaction-owned continuation input, when available.
    pub(crate) fn standalone_compaction_success_status(
        original: Option<tau_proto::TokenCount>,
        after: Option<tau_proto::TokenCount>,
    ) -> String {
        match (original, after) {
            (Some(original), Some(after)) if original.get() != 0 => {
                let retained = (u128::from(after.get()) * 100 + u128::from(original.get()) / 2)
                    / u128::from(original.get());
                format!(
                    "{} → {} ({retained}%) ok",
                    Self::compaction_token_chip(original.get()),
                    Self::compaction_token_chip(after.get()),
                )
            }
            (Some(original), Some(after)) => format!(
                "{} → {} ok",
                Self::compaction_token_chip(original.get()),
                Self::compaction_token_chip(after.get()),
            ),
            (Some(original), None) => {
                format!("{} → ? ok", Self::compaction_token_chip(original.get()))
            }
            (None, Some(after)) => {
                format!("? → {} ok", Self::compaction_token_chip(after.get()))
            }
            (None, None) => "ok".to_owned(),
        }
    }

    /// A provider terminal breaks calibration adjacency but cannot finish a
    /// standalone transaction's row or activity, including on hidden agents.
    pub(super) fn ignore_standalone_provider_terminal(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) -> bool {
        let transcript = if self
            .transcript
            .runtime
            .prompts
            .contains_key(&finished.agent_prompt_id)
        {
            Some(&mut self.transcript)
        } else {
            self.selection
                .agents_ui_state
                .get_mut(&finished.agent_id)
                .map(|state| &mut state.transcript)
        };
        let Some(transcript) = transcript else {
            return false;
        };
        if !transcript
            .runtime
            .prompts
            .get(&finished.agent_prompt_id)
            .is_some_and(|state| state.is_standalone_compaction)
        {
            return false;
        }
        transcript.history.turn_stats_predecessor = None;
        true
    }
}
