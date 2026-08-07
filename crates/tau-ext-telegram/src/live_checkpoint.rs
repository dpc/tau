//! Ordered canonical-confirmation checkpoints for local Telegram polling.

use std::collections::BTreeMap;

use tau_proto::{
    CborValue, Event, MessageAgentTarget, MessageDelivered, MessageExtensionData, MessageFactId,
};

use crate::RuntimeConfig;

/// Extension-data key carrying one local-poll report identity.
pub(super) const TELEGRAM_REPORT_ID_KEY: &str = "telegram_report_id";

/// Typed Telegram update identity accepted by the local checkpoint queue.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(try_from = "i64", into = "i64")]
pub(super) struct TelegramUpdateId {
    /// Raw Telegram Bot API update identifier.
    value: i64,
}

impl TryFrom<i64> for TelegramUpdateId {
    type Error = String;

    /// Validate a raw persisted or wire update identifier.
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
            .ok_or_else(|| "Telegram update ID has no representable successor".to_owned())
    }
}

impl From<TelegramUpdateId> for i64 {
    /// Return the raw update identifier for persistence or transport.
    fn from(update_id: TelegramUpdateId) -> Self {
        update_id.as_i64()
    }
}

impl TelegramUpdateId {
    /// Validate an update ID and reserve its successor as a representable API
    /// offset.
    pub(super) fn new(value: i64) -> Option<Self> {
        value.checked_add(1).map(|_| Self { value })
    }

    /// Return the raw Bot API update identifier.
    pub(super) fn as_i64(self) -> i64 {
        self.value
    }

    /// Build the exclusive Telegram API offset following this update.
    pub(super) fn next_offset(self) -> TelegramUpdateOffset {
        TelegramUpdateOffset {
            value: self
                .value
                .checked_add(1)
                .expect("TelegramUpdateId construction reserves its successor"),
        }
    }
}

impl std::fmt::Display for TelegramUpdateId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(formatter)
    }
}

/// Exclusive Telegram `getUpdates` cursor offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TelegramUpdateOffset {
    /// Raw Bot API offset sent on the next poll.
    value: i64,
}

impl TelegramUpdateOffset {
    /// Return the wire integer consumed by the Telegram client.
    pub(super) fn as_i64(self) -> i64 {
        self.value
    }
}

/// Typed opaque identity shared by a routed report and its canonical fact.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub(super) struct TelegramReportId {
    /// Opaque domain-separated digest value.
    value: String,
}

impl TryFrom<String> for TelegramReportId {
    type Error = String;

    /// Validate one persisted or wire report identity.
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let digest = value
            .strip_prefix("telegram-report:")
            .or_else(|| value.strip_prefix("telegram-gateway-report:"))
            .ok_or_else(|| "Telegram report ID has an unknown domain".to_owned())?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Telegram report ID has an invalid digest".to_owned());
        }
        Ok(Self { value })
    }
}

impl From<TelegramReportId> for String {
    /// Return the validated opaque report identity.
    fn from(report_id: TelegramReportId) -> Self {
        report_id.value
    }
}

impl TelegramReportId {
    /// Derive a stable report identity from one Telegram update stream and
    /// update ID.
    fn for_update(cfg: &RuntimeConfig, update_id: TelegramUpdateId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tau-ext-telegram/live-report-id/v1\0");
        hasher.update(cfg.stream_identity().fingerprint().as_bytes());
        hasher.update(b"\0");
        hasher.update(&update_id.value.to_be_bytes());
        Self {
            value: format!("telegram-report:{}", hasher.finalize().to_hex()),
        }
    }

    /// Derive a stable gateway report identity from its persisted stream
    /// fingerprint and update ID.
    pub(super) fn for_gateway(stream_hash: &str, update_id: TelegramUpdateId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tau-ext-telegram/gateway-report-id/v1\0");
        hasher.update(stream_hash.as_bytes());
        hasher.update(b"\0");
        hasher.update(&update_id.value.to_be_bytes());
        Self {
            value: format!("telegram-gateway-report:{}", hasher.finalize().to_hex()),
        }
    }

    /// Wrap one report identity received from the cooperative gateway.
    pub(super) fn from_gateway(value: String) -> Option<Self> {
        value
            .starts_with("telegram-gateway-report:")
            .then(|| Self::try_from(value).ok())
            .flatten()
    }

    /// Return the opaque wire value.
    pub(super) fn as_str(&self) -> &str {
        &self.value
    }

    /// Parse the exact private report correlation field from a canonical fact.
    pub(super) fn from_extension_data(data: &MessageExtensionData) -> Option<Self> {
        let CborValue::Map(fields) = data.value() else {
            return None;
        };
        fields.iter().find_map(|(key, value)| {
            if !matches!(key, CborValue::Text(key) if key == TELEGRAM_REPORT_ID_KEY) {
                return None;
            }
            match value {
                CborValue::Text(value) => Self::try_from(value.clone()).ok(),
                _ => None,
            }
        })
    }

    /// Encode the report identity preserved unchanged by canonicalization.
    pub(super) fn extension_data(&self) -> MessageExtensionData {
        MessageExtensionData::new(CborValue::Map(vec![(
            CborValue::Text(TELEGRAM_REPORT_ID_KEY.to_owned()),
            CborValue::Text(self.value.clone()),
        )]))
        .expect("fixed Telegram report correlation data is bounded")
    }
}

/// Exact correlated report retained from routing through canonical echo.
pub(super) struct RoutedUpdate {
    /// Stable report identity carried through canonicalization.
    report_id: TelegramReportId,
    /// Exact target agent expected on the canonical fact.
    agent_id: MessageAgentTarget,
    /// Exact message identity expected on the canonical fact.
    message_id: MessageFactId,
    /// Exact report replayed after a missing echo.
    report: Box<Event>,
}

impl RoutedUpdate {
    /// Build a correlated routed report and install its private report ID.
    pub(super) fn new(
        cfg: &RuntimeConfig,
        update_id: TelegramUpdateId,
        mut delivered: MessageDelivered<tau_proto::RawMessagePublisherId>,
    ) -> Self {
        let report_id = TelegramReportId::for_update(cfg, update_id);
        delivered.extension_data = report_id.extension_data();
        let agent_id = delivered.agent_id.clone();
        let message_id = delivered.message_id.clone();
        Self {
            report_id,
            agent_id,
            message_id,
            report: Box::new(Event::MessageDeliveredReported(delivered)),
        }
    }

    /// Clone the retained event for output without exposing correlation fields.
    pub(super) fn report(&self) -> Box<Event> {
        self.report.clone()
    }

    /// Return whether a canonical fact exactly confirms this routed report.
    fn matches(&self, fact: &MessageDelivered, report_id: &TelegramReportId) -> bool {
        &self.report_id == report_id
            && self.agent_id.as_str() == fact.agent_id.as_str()
            && self.message_id == fact.message_id
    }
}

/// One observed Telegram update waiting behind the local poll cursor.
enum UpdateCheckpoint {
    /// A routed report plus whether its exact canonical echo has returned.
    Routed {
        /// Correlated report bundle retained for replay and matching.
        route: RoutedUpdate,
        /// Whether the matching canonical fact has returned.
        acknowledged: bool,
    },
    /// An update that emitted no Tau event and completed local processing.
    NonRouted,
}

impl UpdateCheckpoint {
    /// Return whether this checkpoint permits cursor advancement.
    fn is_acknowledged(&self) -> bool {
        match self {
            Self::Routed { acknowledged, .. } => *acknowledged,
            Self::NonRouted => true,
        }
    }
}

/// Existing handling for one Telegram update already in the checkpoint queue.
pub(super) enum ExistingUpdate {
    /// This update has already advanced beyond the live cursor.
    Acknowledged,
    /// Replay this exact routed report without recomputing routing.
    Routed(Box<Event>),
    /// Repeat best-effort non-routed processing without changing
    /// classification.
    NonRouted,
    /// This update has not been classified in the current queue.
    New,
}

/// Ordered local-poll checkpoints that own cursor advancement.
#[derive(Default)]
pub(super) struct LiveCheckpoints {
    /// Observed but not yet cursor-retired updates, ordered by update ID.
    checkpoints: BTreeMap<TelegramUpdateId, UpdateCheckpoint>,
}

impl LiveCheckpoints {
    /// Classify a fetched update against the current cursor and pending queue.
    pub(super) fn existing_update(
        &self,
        update_id: TelegramUpdateId,
        next_update_offset: Option<TelegramUpdateOffset>,
    ) -> ExistingUpdate {
        if next_update_offset.is_some_and(|offset| update_id.value < offset.value) {
            return ExistingUpdate::Acknowledged;
        }
        match self.checkpoints.get(&update_id) {
            Some(UpdateCheckpoint::Routed { route, .. }) => ExistingUpdate::Routed(route.report()),
            Some(UpdateCheckpoint::NonRouted) => ExistingUpdate::NonRouted,
            None => ExistingUpdate::New,
        }
    }

    /// Retain a routed report before exposing it to an early canonical echo.
    pub(super) fn insert_routed(&mut self, update_id: TelegramUpdateId, route: RoutedUpdate) {
        self.checkpoints
            .entry(update_id)
            .or_insert(UpdateCheckpoint::Routed {
                route,
                acknowledged: false,
            });
    }

    /// Mark a newly completed non-routed update as immediately acknowledged.
    pub(super) fn insert_non_routed(&mut self, update_id: TelegramUpdateId) {
        self.checkpoints
            .entry(update_id)
            .or_insert(UpdateCheckpoint::NonRouted);
    }

    /// Confirm one exact routed checkpoint from its canonical message echo.
    ///
    /// Returns `true` only when a previously unacknowledged checkpoint changes
    /// state. Unrelated or repeated canonical facts return `false`.
    pub(super) fn acknowledge_canonical(
        &mut self,
        publisher_matches: bool,
        fact: &MessageDelivered,
    ) -> bool {
        if !publisher_matches {
            return false;
        }
        let Some(report_id) = TelegramReportId::from_extension_data(&fact.extension_data) else {
            return false;
        };
        let Some(acknowledged) = self.checkpoints.values_mut().find_map(|checkpoint| {
            let UpdateCheckpoint::Routed {
                route,
                acknowledged,
            } = checkpoint
            else {
                return None;
            };
            route.matches(fact, &report_id).then_some(acknowledged)
        }) else {
            return false;
        };
        if *acknowledged {
            return false;
        }
        *acknowledged = true;
        true
    }

    /// Remove the contiguous acknowledged prefix and return the resulting
    /// Telegram offset.
    pub(super) fn advance_acknowledged_prefix(
        &mut self,
        mut next_update_offset: Option<TelegramUpdateOffset>,
    ) -> Option<TelegramUpdateOffset> {
        while self
            .checkpoints
            .first_key_value()
            .is_some_and(|(_, checkpoint)| checkpoint.is_acknowledged())
        {
            let (update_id, _) = self
                .checkpoints
                .pop_first()
                .expect("checked first checkpoint");
            next_update_offset = Some(update_id.next_offset());
        }
        next_update_offset
    }

    /// Return whether any update remains blocked behind canonical confirmation.
    pub(super) fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// Forget all checkpoints when their Telegram stream identity retires.
    pub(super) fn clear(&mut self) {
        self.checkpoints.clear();
    }
}
