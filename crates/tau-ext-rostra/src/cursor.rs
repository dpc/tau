//! Native, versioned Rostra pagination cursor envelopes.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rostra_client_db::news::NewsRankPaginationCursor;
use rostra_client_db::social::EventPaginationCursor;

use crate::tools::ToolFailure;

const PREFIX: &str = "rostra-v1:";
const MAX_CURSOR_BYTES: usize = 1_024;

/// Public timeline selector used to bind cursors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Timeline {
    /// Direct followees filtered by their tag selectors.
    Following,
    /// Locally projected social-news ranking.
    Network,
    /// One explicit author.
    Author,
}

impl Timeline {
    /// Return the schema spelling used in tool output and cursor binding.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Network => "network",
            Self::Author => "author",
        }
    }
}

/// Upstream continuation state; no local offset is synthesized.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum Position {
    /// Timestamp/event continuation for social posts.
    Social(EventPaginationCursor),
    /// Score/post continuation for ranked news.
    Network(NewsRankPaginationCursor),
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
/// Serialized binding between one filter and one upstream-native position.
struct Envelope {
    /// Timeline that created this cursor.
    timeline: Timeline,
    /// Canonical author filter for author timelines.
    author: Option<String>,
    /// Upstream-native continuation.
    position: Position,
}

/// Encode one upstream-native cursor in a timeline-bound opaque envelope.
pub(crate) fn encode(timeline: Timeline, author: Option<&str>, position: Position) -> String {
    let value = Envelope {
        timeline,
        author: author.map(str::to_owned),
        position,
    };
    let bytes = serde_json::to_vec(&value).expect("cursor envelope is serializable");
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Decode and validate an optional timeline-bound cursor.
pub(crate) fn decode(
    value: Option<&str>,
    timeline: Timeline,
    author: Option<&str>,
) -> Result<Option<Position>, ToolFailure> {
    let Some(value) = value else {
        return Ok(None);
    };
    if MAX_CURSOR_BYTES < value.len() {
        return Err(ToolFailure::invalid("cursor is too long"));
    }
    let encoded = value
        .strip_prefix(PREFIX)
        .ok_or_else(|| ToolFailure::invalid("cursor version is unsupported"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ToolFailure::invalid("cursor is malformed"))?;
    let envelope: Envelope =
        serde_json::from_slice(&bytes).map_err(|_| ToolFailure::invalid("cursor is malformed"))?;
    if envelope.timeline != timeline || envelope.author.as_deref() != author {
        return Err(ToolFailure::invalid(
            "cursor does not belong to this timeline and filter",
        ));
    }
    Ok(Some(envelope.position))
}
