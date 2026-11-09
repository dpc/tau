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
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(&format!("{current}/{total}"));
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
    /// Adds a ready archive to its existing transaction-owned row, never to an
    /// unrelated compaction or by parsing provider/model text.
    pub(super) fn show_compaction_recall_artifact(
        &mut self,
        artifact: &tau_proto::CompactionRecallArtifact,
    ) {
        self.transcript
            .runtime
            .compaction_recall_artifacts
            .insert(artifact.transaction_id.clone(), artifact.key.clone());
        let call_id = self
            .transcript
            .runtime
            .self_compaction_tools
            .iter()
            .find_map(|(call_id, tool)| {
                if tool.transaction_id.as_ref() != Some(&artifact.transaction_id) {
                    return None;
                }
                Some(call_id.clone())
            });
        if let Some(call_id) = call_id
            && let Some((status, text)) = self.transcript.runtime.self_compaction_tools[&call_id]
                .status
                .clone()
        {
            self.update_self_compaction_tool_status(&call_id, status, text);
        }
        let Some(block_id) = self
            .transcript
            .runtime
            .compaction_history_blocks
            .get(&artifact.transaction_id)
            .copied()
        else {
            return;
        };
        let standalone = self
            .transcript
            .history
            .compaction_history
            .iter()
            .any(|entry| entry.block_id == block_id);
        let Some(entry) = self
            .transcript
            .history
            .tool_history
            .iter_mut()
            .chain(self.transcript.history.compaction_history.iter_mut())
            .find(|entry| entry.block_id == block_id)
        else {
            return;
        };
        entry.display.args = tau_proto::artifact_uri(&artifact.key);
        let display = entry.display.clone();
        let block = if standalone {
            self.render_compact_tool_block(&display)
        } else {
            self.render_tool_history_block(&display)
        };
        self.resources.handle.set_block(block_id, block);
        self.resources.handle.redraw();
    }

    /// Shows accepted intent as transient state, never as a durable notice or
    /// an internal correlation identifier. Self-compaction keeps its tool row.
    pub(super) fn show_queued_compaction(
        &mut self,
        requested: &tau_proto::AgentManualCompactionRequested,
    ) {
        if let Some(call_id) = self
            .transcript
            .runtime
            .self_compaction_tools
            .iter()
            .find_map(|(call_id, tool)| {
                (tool.request_id == requested.request_id).then(|| call_id.clone())
            })
        {
            self.update_self_compaction_tool_status(
                &call_id,
                CompactionStatus::Progress,
                "(queued)",
            );
            return;
        }
        if self
            .transcript
            .runtime
            .queued_compactions
            .contains_key(&requested.request_id)
        {
            return;
        }
        let block = super::render_compaction_block(
            &self.resources.theme,
            "(queued)",
            CompactionStatus::Progress,
        );
        let id = self.resources.handle.new_block("compaction-queued", block);
        self.resources
            .handle
            .push_above_active_before_any(id, self.watched_agent_anchor_ids());
        self.transcript
            .runtime
            .queued_compactions
            .insert(requested.request_id.clone(), id);
        self.resources.handle.redraw();
    }

    /// Retires only the matching target-owned queue projection.
    pub(super) fn remove_queued_compaction(&mut self, request_id: &tau_proto::CompactionRequestId) {
        if let Some(id) = self
            .transcript
            .runtime
            .queued_compactions
            .remove(request_id)
        {
            self.resources.handle.remove_block(id);
            self.resources.handle.redraw();
        }
    }

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

    /// Formats the persisted same-snapshot estimates, never unrelated usage.
    pub(crate) fn standalone_compaction_success_status(
        effectiveness: Option<&tau_proto::CompactionEffectiveness>,
    ) -> String {
        let chip = |value: Option<tau_proto::TokenCount>| {
            value.map_or_else(
                || "?".to_owned(),
                |value| format!("~{}", Self::compaction_token_chip(value.get())),
            )
        };
        format!(
            "{}/{} -> {}/{} ok",
            chip(effectiveness.and_then(|value| value.prefix_before)),
            chip(effectiveness.and_then(|value| value.total_before)),
            chip(effectiveness.and_then(|value| value.replacement_after)),
            chip(effectiveness.and_then(|value| value.total_after)),
        )
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

/// Keeps pre-start failures useful without exposing request identifiers.
pub(super) fn request_failure_status(
    reason: tau_proto::ManualCompactionRequestFailureReason,
) -> &'static str {
    use tau_proto::ManualCompactionRequestFailureReason as Reason;
    match reason {
        Reason::PrefixTooLarge => "compaction prefix too large",
        Reason::Cancelled => "cancelled",
        Reason::TargetUnloaded => "target unloaded",
        Reason::ModelChanged => "model changed",
        Reason::Unsupported => "compaction unsupported",
        Reason::RouteFailed => "provider route unavailable",
        Reason::StaleBranch => "transcript branch changed",
    }
}
