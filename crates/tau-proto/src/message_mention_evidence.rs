//! Descriptive native mention observations, independent of sender admission.

use serde::{Deserialize, Serialize};

/// Maximum number of people in one partial mention observation.
pub const MESSAGE_MENTION_PEOPLE_MAX: usize = 32;
/// Maximum number of source occurrences recorded for one person.
pub const MESSAGE_MENTION_SPANS_MAX: usize = 8;
/// Maximum byte length of one source token copied into metadata.
pub const MESSAGE_MENTION_TOKEN_MAX_BYTES: usize = 256;

/// Publisher-observed references in this exact delivered or edited text
/// version.
///
/// Absence is unknown, not false. Neither a partial people list nor any mention
/// observation classifies the intended recipient or grants reply authority.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMentionEvidence {
    /// Authoritative comparison with the bridge's authenticated transport
    /// identity. False requires a complete self-mention observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions_self: Option<bool>,
    /// Possibly partial people observations, in the publisher's participant
    /// namespace. Empty does not imply that nobody was mentioned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<MessageMentionPerson>,
}

/// One referenced participant, without sender authentication or admission
/// claims.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageMentionPerson {
    /// Opaque-to-Tau participant reference using the publisher's existing
    /// identity namespace and disclosure policy, not a send selector or global
    /// identity. This field does not authorize new raw transport ID exposure.
    pub stable_id: String,
    /// Optional resolved presentation snapshot, not identity or authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether this person matches the bridge's authenticated transport
    /// identity, not whether the message addresses this particular Tau
    /// agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_self: Option<bool>,
    /// Optional exact token locations after all producer text normalization.
    /// Omit when the token was stripped or cannot be mapped reliably.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_spans: Vec<MessageMentionSpan>,
}

/// Half-open UTF-8 byte range in this fact's exact text, never in a prior
/// version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageMentionSpan {
    /// Inclusive byte offset on a UTF-8 boundary.
    pub start: usize,
    /// Exclusive byte offset on a UTF-8 boundary, strictly greater than start.
    pub end: usize,
}

impl MessageMentionEvidence {
    /// Render bounded metadata with exact source substrings for intelligible
    /// opaque-identity mapping. Invalid optional enrichment is omitted as a
    /// whole, without suppressing delivery, activation, or the original
    /// stored evidence.
    pub(crate) fn projection(&self, text: &str) -> Option<String> {
        if self.people.len() > MESSAGE_MENTION_PEOPLE_MAX {
            return None;
        }
        let mut people = Vec::with_capacity(self.people.len());
        for person in &self.people {
            if person.stable_id.is_empty()
                || person.stable_id.len() > super::message_fact::STABLE_ID_MAX_BYTES
                || !person
                    .display_name
                    .as_deref()
                    .is_none_or(super::message_fact::valid_display)
                || person.source_spans.len() > MESSAGE_MENTION_SPANS_MAX
            {
                return None;
            }
            let mut projected = serde_json::to_value(person).ok()?;
            let mut sources = Vec::with_capacity(person.source_spans.len());
            for span in &person.source_spans {
                if span.start >= span.end || span.end - span.start > MESSAGE_MENTION_TOKEN_MAX_BYTES
                {
                    return None;
                }
                let source = text.get(span.start..span.end)?;
                sources.push(serde_json::json!({
                    "start": span.start, "end": span.end, "text": source,
                }));
            }
            if !sources.is_empty() {
                projected["source_spans"] = serde_json::Value::Array(sources);
            }
            people.push(projected);
        }
        let mut projected = serde_json::Map::new();
        if let Some(mentions_self) = self.mentions_self {
            projected.insert("mentions_self".into(), mentions_self.into());
        }
        if !people.is_empty() {
            projected.insert("people".into(), people.into());
        }
        Some(serde_json::Value::Object(projected).to_string())
    }
}
