use super::{SlackDelete, SlackEdit, SlackMessage, SlackReaction};

/// Private Slack-native identity shared by duplicate suppression and report
/// correlation.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct SlackOccurrenceKey(String);

impl SlackOccurrenceKey {
    /// Build the exact existing occurrence identity for one received message.
    pub(super) fn message(message: &SlackMessage) -> Option<Self> {
        message
            .ts
            .as_ref()
            .map(|ts| Self(format!("message:{}:{ts}", message.channel_id)))
            .or_else(|| {
                message
                    .event_id
                    .as_ref()
                    .map(|event_id| Self(format!("event:{event_id}")))
            })
    }

    /// Build the exact existing occurrence identity for one reaction event.
    pub(super) fn reaction(reaction: &SlackReaction) -> Self {
        reaction.event_id.as_ref().map_or_else(
            || {
                Self(format!(
                    "reaction:{}:{}:{}:{}:{}",
                    reaction.event_type.as_str(),
                    reaction.channel_id,
                    reaction.message_ts,
                    reaction.user_id,
                    reaction.reaction
                ))
            },
            |event_id| Self(format!("reaction:{event_id}")),
        )
    }

    /// Build the exact existing occurrence identity for one message revision.
    pub(super) fn edit(edit: &SlackEdit) -> Self {
        let received_key = edit.event_id.clone().unwrap_or_else(|| {
            format!(
                "edit:{}:{}:{}",
                edit.channel_id, edit.message_ts, edit.revision_ts
            )
        });
        Self(format!("edit:{received_key}"))
    }

    /// Build the exact existing occurrence identity for one message deletion.
    pub(super) fn delete(delete: &SlackDelete) -> Self {
        let occurrence = delete.event_id.as_ref().map_or_else(
            || format!("{}:{}", delete.channel_id, delete.message_ts),
            Clone::clone,
        );
        Self(format!("delete:{occurrence}"))
    }

    /// Borrow the exact bytes used for cache membership and report-ID hashing.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct an exact arbitrary identity only for focused cache and
    /// correlation mismatch tests.
    #[cfg(test)]
    pub(super) fn test_raw(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}
