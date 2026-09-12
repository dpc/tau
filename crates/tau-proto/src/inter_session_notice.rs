use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// Maximum UTF-8 byte length of one configured inter-session notice.
pub const INTER_SESSION_NOTICE_MAX_BYTES: usize = 64 * 1024;

/// Validated advisory text attached to an inter-session message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InterSessionNotice {
    /// Validated UTF-8 notice text, including an explicitly configured empty
    /// value.
    text: String,
}

impl InterSessionNotice {
    /// Validates and constructs one notice while preserving empty text.
    pub fn new(text: String) -> Result<Self, String> {
        if text.len() > INTER_SESSION_NOTICE_MAX_BYTES {
            return Err("inter-session notice exceeds the 64 KiB limit".to_owned());
        }
        Ok(Self { text })
    }

    /// Borrows the configured notice text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl AsRef<str> for InterSessionNotice {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for InterSessionNotice {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl TryFrom<String> for InterSessionNotice {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<InterSessionNotice> for String {
    fn from(notice: InterSessionNotice) -> Self {
        notice.text
    }
}

#[cfg(test)]
mod tests;
