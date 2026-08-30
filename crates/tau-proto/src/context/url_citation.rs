//! Validated semantic assistant URL citations.

use serde::{Deserialize, Deserializer, Serialize, de};

/// Validated bounded HTTP(S) citation over one concatenated assistant message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UrlCitation {
    /// Inclusive Unicode-scalar start offset.
    start: u32,
    /// Exclusive Unicode-scalar end offset.
    end: u32,
    /// Canonical HTTP(S) target URL.
    url: String,
    /// Bounded untrusted display title.
    title: String,
}

impl UrlCitation {
    /// Maximum retained URL characters.
    pub const MAX_URL_CHARS: usize = 2_048;
    /// Maximum retained title characters.
    pub const MAX_TITLE_CHARS: usize = 256;

    /// Validate and canonicalize one provider citation.
    pub fn try_new(start: u32, end: u32, url: &str, title: &str) -> Result<Self, &'static str> {
        if end <= start {
            return Err("citation range must be nonempty");
        }
        if url.chars().count() > Self::MAX_URL_CHARS
            || title.chars().count() > Self::MAX_TITLE_CHARS
            || url.chars().any(char::is_control)
            || title.chars().any(char::is_control)
            || url.chars().any(char::is_whitespace)
        {
            return Err("citation metadata exceeds safety bounds");
        }
        let parsed = url::Url::parse(url).map_err(|_| "citation URL is invalid")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("citation URL must be HTTP(S) without userinfo");
        }
        let canonical_url = parsed.to_string();
        if canonical_url.len() > crate::MAX_HYPERLINK_TARGET_BYTES {
            return Err("canonical citation URL exceeds clickable target bound");
        }
        Ok(Self {
            start,
            end,
            url: canonical_url,
            title: title.to_owned(),
        })
    }

    /// Inclusive scalar start.
    #[must_use]
    pub const fn start(&self) -> u32 {
        self.start
    }

    /// Exclusive scalar end.
    #[must_use]
    pub const fn end(&self) -> u32 {
        self.end
    }

    /// Canonical HTTP(S) URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Bounded untrusted title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl<'de> Deserialize<'de> for UrlCitation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        /// Unchecked citation wire representation validated before
        /// construction.
        #[derive(Deserialize)]
        struct RawCitation {
            /// Inclusive Unicode-scalar start offset.
            start: u32,
            /// Exclusive Unicode-scalar end offset.
            end: u32,
            /// Untrusted provider URL spelling.
            url: String,
            /// Untrusted provider display title.
            title: String,
        }
        let raw = RawCitation::deserialize(deserializer)?;
        Self::try_new(raw.start, raw.end, &raw.url, &raw.title).map_err(de::Error::custom)
    }
}
