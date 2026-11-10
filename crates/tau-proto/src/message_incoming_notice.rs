use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// Immutable local guidance associated with a publisher-authenticated endpoint.
///
/// Publishers must derive `scope` from their configured/authenticated endpoint,
/// never from a remote sender's self-reported identity. The harness namespaces
/// it by canonical publisher and renders the first nonempty notice per scope
/// in each recipient's selected context. It grants no additional authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "NoticeFields", into = "NoticeFields")]
pub struct MessageIncomingNotice {
    /// Publisher-local stable endpoint identity, bounded to 256 UTF-8 bytes.
    scope: String,
    /// Local advisory text, bounded to 64 KiB UTF-8.
    text: String,
}

/// Wire fields validated together during deserialization.
#[derive(Serialize, Deserialize)]
struct NoticeFields {
    /// Stable publisher-local endpoint identity.
    scope: String,
    /// Configured local guidance, preserving explicit empty text.
    text: String,
}

impl MessageIncomingNotice {
    /// Validate an endpoint scope and its immutable local guidance snapshot.
    pub fn new(scope: String, text: String) -> Result<Self, String> {
        if scope.is_empty() || scope.len() > 256 {
            return Err("incoming notice scope must contain 1 to 256 UTF-8 bytes".to_owned());
        }
        if text.len() > crate::INTER_SESSION_NOTICE_MAX_BYTES {
            return Err("incoming notice exceeds the 64 KiB limit".to_owned());
        }
        Ok(Self { scope, text })
    }

    /// Return the publisher-local authenticated endpoint scope.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Return the configured local guidance, which may be empty.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Render local advisory guidance separately from the remote body, within
    /// the existing registered message family. The deduplication scope stays
    /// internal rather than adding irrelevant identity metadata to the prompt.
    #[must_use]
    pub fn render(&self, publisher: &crate::MessagePublisherId) -> String {
        crate::MESSAGE_PAYLOAD_ENVELOPE
            .render_attributed(
                &[
                    (
                        "publisher",
                        crate::visible_escape_metadata(publisher.as_str()),
                    ),
                    ("origin", "recipient_config".to_owned()),
                    ("authority", "advisory".to_owned()),
                ],
                &self.text,
            )
            .expect("incoming notice attributes follow the shared registry")
    }
}

impl TryFrom<NoticeFields> for MessageIncomingNotice {
    type Error = String;

    fn try_from(fields: NoticeFields) -> Result<Self, Self::Error> {
        Self::new(fields.scope, fields.text)
    }
}

impl From<MessageIncomingNotice> for NoticeFields {
    fn from(notice: MessageIncomingNotice) -> Self {
        Self {
            scope: notice.scope,
            text: notice.text,
        }
    }
}
