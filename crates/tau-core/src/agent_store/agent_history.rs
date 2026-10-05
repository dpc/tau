//! Accepted replay records and exact facts needed without scanning those
//! records.

use std::collections::HashMap;

use tau_proto::{ContextItem, Event, ToolCallId, ToolCallRef};

use super::managed_agent_encoded_event_bytes;
use crate::PersistedAgentEvent;

/// Accepted history cache with immutable live-query facts independent of
/// residency.
#[derive(Clone, Debug, Default)]
pub(super) struct AgentHistory {
    /// Resident accepted suffix, beginning at `first_resident_seq`.
    pub(super) records: Vec<PersistedAgentEvent>,
    /// Encoded sizes parallel to resident records, for bounded pin accounting.
    pub(super) record_bytes: Vec<usize>,
    /// Sequence of the first resident record, or accepted end for an empty
    /// suffix.
    pub(super) first_resident_seq: u64,
    /// Encoded charge of all accepted records, also covering the retained tree.
    pub(super) encoded_event_bytes: usize,
    /// Exact first record, kept separately from mutable tree metadata.
    pub(super) first_record: Option<PersistedAgentEvent>,
    /// First encountered creation role; historical recovery does not require
    /// creation at sequence zero, unlike managed live admission.
    pub(super) first_started_role: Option<String>,
    /// First declaration occurrence for every historical provider-visible call
    /// id.
    pub(super) tool_declarations: HashMap<ToolCallId, ToolCallRef>,
    /// First unused numeric suffix across all four persisted prompt-id
    /// families.
    pub(super) next_prompt_index: u64,
    /// Accepted record count, independent of the resident record vector.
    pub(super) accepted_count: u64,
}

impl AgentHistory {
    /// Scans a memory-only history without cloning records or constructing
    /// indexes.
    pub(super) fn next_prompt_index_in(records: &[PersistedAgentEvent]) -> u64 {
        records
            .iter()
            .filter_map(Self::prompt_index)
            .max()
            .unwrap_or(0)
    }

    /// Preserves first-occurrence lookup for memory-only histories.
    pub(super) fn tool_declaration_in(
        records: &[PersistedAgentEvent],
        call_id: &ToolCallId,
    ) -> Option<ToolCallRef> {
        records.iter().find_map(|record| {
            let Event::ProviderResponseFinished(response) = &record.event else {
                return None;
            };
            response
                .output_items
                .iter()
                .position(
                    |item| matches!(item, ContextItem::ToolCall(call) if &call.call_id == call_id),
                )
                .and_then(|index| u32::try_from(index).ok())
                .map(|item_index| ToolCallRef {
                    declaration: record.observation_id,
                    item_index,
                })
        })
    }

    /// Reconstructs live facts from the same validated records used by tree
    /// replay.
    pub(super) fn from_records(records: Vec<PersistedAgentEvent>) -> Self {
        let mut history = Self::default();
        for record in &records {
            history.observe(record);
        }
        history.record_bytes = records
            .iter()
            .map(|record| managed_agent_encoded_event_bytes(std::slice::from_ref(record)))
            .collect();
        history.encoded_event_bytes = history
            .record_bytes
            .iter()
            .fold(0_usize, |sum, size| sum.saturating_add(*size));
        history.records = records;
        history
    }

    /// Advances records and facts together inside an off-side admission
    /// candidate.
    pub(super) fn push(&mut self, record: PersistedAgentEvent, measured_bytes: usize) {
        self.observe(&record);
        self.encoded_event_bytes = self.encoded_event_bytes.saturating_add(measured_bytes);
        self.record_bytes.push(measured_bytes);
        self.records.push(record);
    }

    /// Releases a complete resident prefix without retaining its vector
    /// capacity.
    pub(super) fn evict_before(&mut self, next_seq: u64) {
        let next_seq = next_seq
            .min(self.accepted_count)
            .max(self.first_resident_seq);
        let count = (next_seq - self.first_resident_seq) as usize;
        if count == 0 {
            return;
        }
        self.records = self.records.split_off(count);
        self.record_bytes = self.record_bytes.split_off(count);
        self.first_resident_seq = next_seq;
    }

    /// Folds exact live facts without depending on any currently resident
    /// prefix.
    fn observe(&mut self, record: &PersistedAgentEvent) {
        if self.accepted_count == 0 {
            self.first_record = Some(record.clone());
        }
        self.accepted_count = self.accepted_count.saturating_add(1);
        if self.first_started_role.is_none()
            && let Event::AgentStarted(started) = &record.event
        {
            self.first_started_role = Some(started.role.clone());
        }
        if let Event::ProviderResponseFinished(response) = &record.event {
            for (index, item) in response.output_items.iter().enumerate() {
                let ContextItem::ToolCall(call) = item else {
                    continue;
                };
                let Ok(item_index) = u32::try_from(index) else {
                    break;
                };
                self.tool_declarations
                    .entry(call.call_id.clone())
                    .or_insert(ToolCallRef {
                        declaration: record.observation_id,
                        item_index,
                    });
            }
        }
        if let Some(index) = Self::prompt_index(record) {
            self.next_prompt_index = self.next_prompt_index.max(index);
        }
    }

    /// Extracts one checked numeric suffix using the existing saturating cursor
    /// rule.
    fn prompt_index(record: &PersistedAgentEvent) -> Option<u64> {
        let transaction_id;
        let prompt_id = match &record.event {
            Event::AgentPromptStarted(prompt) => prompt.agent_prompt_id.as_str(),
            Event::ProviderResponseFinished(response) => response.agent_prompt_id.as_str(),
            Event::AgentInferenceDispatchStarted(checkpoint) => checkpoint.agent_prompt_id.as_str(),
            Event::AgentStandaloneCompactionStarted(started) => {
                transaction_id = started.transaction_id.to_string();
                &transaction_id
            }
            _ => return None,
        };
        prompt_id
            .rsplit('-')
            .next()
            .and_then(|suffix| suffix.parse::<u64>().ok())
            .map(|index| index.saturating_add(1))
    }
}
