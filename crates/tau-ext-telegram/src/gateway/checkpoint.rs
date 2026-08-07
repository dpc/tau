//! Durable ordered checkpoints for Telegram gateway updates.

use std::collections::HashSet;

use serde::de::Error as _;

use super::routing::GatewayDelivery;
use crate::live_checkpoint::{TelegramReportId, TelegramUpdateId};

/// One durably classified Telegram gateway update.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GatewayCheckpoint {
    /// A routed report retained until its canonical echo is acknowledged.
    Routed {
        /// Exact sidecar delivery replayed after loss or restart.
        delivery: GatewayDelivery,
        /// Whether the configured sidecar confirmed the canonical echo.
        acknowledged: bool,
    },
    /// A non-routed update whose required local work completed.
    NonRouted,
}

impl GatewayCheckpoint {
    /// Return whether this checkpoint permits cursor advancement.
    fn is_acknowledged(&self) -> bool {
        match self {
            Self::Routed { acknowledged, .. } => *acknowledged,
            Self::NonRouted => true,
        }
    }
}

/// Ordered durable checkpoint entry keyed by Telegram update ID.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct GatewayCheckpointEntry {
    /// Validated Telegram update identifier.
    update_id: TelegramUpdateId,
    /// Classification and acknowledgement state.
    checkpoint: GatewayCheckpoint,
}

/// Ordered durable checkpoints controlling gateway cursor advancement.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub(super) struct GatewayCheckpoints {
    /// Entries in ascending Telegram update order.
    entries: Vec<GatewayCheckpointEntry>,
}

impl<'de> serde::Deserialize<'de> for GatewayCheckpoints {
    /// Decode only strictly increasing, unique checkpoint entries.
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let entries = Vec::<GatewayCheckpointEntry>::deserialize(deserializer)?;
        if entries
            .windows(2)
            .any(|pair| pair[1].update_id <= pair[0].update_id)
        {
            return Err(Deserializer::Error::custom(
                "Telegram gateway checkpoints are not strictly increasing",
            ));
        }
        Ok(Self { entries })
    }
}

impl GatewayCheckpoints {
    /// Return whether an update has already been classified.
    pub(super) fn contains(&self, update_id: TelegramUpdateId) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.update_id == update_id)
    }

    /// Insert one routed update, preserving the first exact classification.
    pub(super) fn insert_routed(&mut self, update_id: TelegramUpdateId, delivery: GatewayDelivery) {
        self.insert(
            update_id,
            GatewayCheckpoint::Routed {
                delivery,
                acknowledged: false,
            },
        );
    }

    /// Insert one completed non-routed update.
    pub(super) fn insert_non_routed(&mut self, update_id: TelegramUpdateId) {
        self.insert(update_id, GatewayCheckpoint::NonRouted);
    }

    /// Mark the routed checkpoint with this exact report ID acknowledged.
    pub(super) fn acknowledge(&mut self, report_id: &TelegramReportId) -> bool {
        let Some(acknowledged) = self.entries.iter_mut().find_map(|entry| {
            let GatewayCheckpoint::Routed {
                delivery,
                acknowledged,
            } = &mut entry.checkpoint
            else {
                return None;
            };
            (delivery.request_id == *report_id).then_some(acknowledged)
        }) else {
            return false;
        };
        if *acknowledged {
            return false;
        }
        *acknowledged = true;
        true
    }

    /// Return the exact pending delivery for one validated report identity.
    pub(super) fn pending_delivery(
        &self,
        report_id: &TelegramReportId,
    ) -> Option<&GatewayDelivery> {
        self.entries
            .iter()
            .find_map(|entry| match &entry.checkpoint {
                GatewayCheckpoint::Routed {
                    delivery,
                    acknowledged: false,
                } if delivery.request_id == *report_id => Some(delivery),
                GatewayCheckpoint::Routed { .. } | GatewayCheckpoint::NonRouted => None,
            })
    }

    /// Clone all unacknowledged routed deliveries for restart recovery.
    pub(super) fn pending_deliveries(&self) -> Vec<GatewayDelivery> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.checkpoint {
                GatewayCheckpoint::Routed {
                    delivery,
                    acknowledged: false,
                } => Some(delivery.clone()),
                GatewayCheckpoint::Routed {
                    acknowledged: true, ..
                }
                | GatewayCheckpoint::NonRouted => None,
            })
            .collect()
    }

    /// Remove the contiguous acknowledged prefix and return its final offset.
    pub(super) fn advance_prefix(&mut self, mut offset: Option<i64>) -> Option<i64> {
        while self
            .entries
            .first()
            .is_some_and(|entry| entry.checkpoint.is_acknowledged())
        {
            let entry = self.entries.remove(0);
            offset = Some(entry.update_id.next_offset().as_i64());
        }
        offset
    }

    /// Copy one update's exact classification from a separately processed
    /// candidate while preserving acknowledgements committed concurrently.
    pub(super) fn merge_update_from(&mut self, candidate: &Self, update_id: TelegramUpdateId) {
        let Some(entry) = candidate
            .entries
            .iter()
            .find(|entry| entry.update_id == update_id)
        else {
            return;
        };
        self.insert(update_id, entry.checkpoint.clone());
    }

    /// Validate every routed report against its stream and update identity and
    /// reject duplicate report IDs.
    pub(super) fn validate_report_ids(&self, stream_hash: &str) -> Result<(), String> {
        let mut report_ids = HashSet::new();
        for entry in &self.entries {
            let GatewayCheckpoint::Routed { delivery, .. } = &entry.checkpoint else {
                continue;
            };
            let expected = TelegramReportId::for_gateway(stream_hash, entry.update_id);
            if delivery.request_id != expected {
                return Err(format!(
                    "Telegram gateway checkpoint {} has a mismatched report ID",
                    entry.update_id
                ));
            }
            if !report_ids.insert(delivery.request_id.clone()) {
                return Err("Telegram gateway checkpoints contain a duplicate report ID".to_owned());
            }
        }
        Ok(())
    }

    /// Insert a checkpoint in update order unless it already exists.
    fn insert(&mut self, update_id: TelegramUpdateId, checkpoint: GatewayCheckpoint) {
        match self
            .entries
            .binary_search_by_key(&update_id, |entry| entry.update_id)
        {
            Ok(_) => {}
            Err(index) => self.entries.insert(
                index,
                GatewayCheckpointEntry {
                    update_id,
                    checkpoint,
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests;
