//! Provider-neutral search and fetch preferences.

use std::collections::BTreeSet;

/// Maximum configured provider-side content budget.
const MAX_CONTENT_CHARS: u32 = 512 * 1024;
/// Maximum configured cache age.
const MAX_CACHE_AGE_SECONDS: u64 = 365 * 24 * 60 * 60;
/// Maximum number of configured excluded domains.
const MAX_EXCLUDED_DOMAINS: usize = 100;

/// Requested PDF parsing behavior for providers that expose it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum PdfParsing {
    /// Ask the provider not to parse PDF contents.
    Disabled,
    /// Prefer the provider's fast PDF parser.
    Fast,
    /// Let the provider choose the PDF parser.
    Auto,
    /// Prefer optical character recognition.
    Ocr,
}

impl PdfParsing {
    /// Return the provider wire spelling for an enabled parser mode.
    pub(super) const fn enabled_mode(self) -> Option<&'static str> {
        match self {
            Self::Disabled => None,
            Self::Fast => Some("fast"),
            Self::Auto => Some("auto"),
            Self::Ocr => Some("ocr"),
        }
    }
}

/// Requested search freshness window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SearchRecency {
    /// Prefer results from the last day.
    Day,
    /// Prefer results from the last week.
    Week,
    /// Prefer results from the last month.
    Month,
    /// Prefer results from the last year.
    Year,
}

impl SearchRecency {
    /// Return the common long-form provider spelling.
    pub(super) const fn as_long(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    /// Return Brave's compact freshness spelling.
    pub(super) const fn as_brave(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    /// Return Firecrawl's Google-style freshness spelling.
    pub(super) const fn as_firecrawl(self) -> &'static str {
        match self {
            Self::Day => "qdr:d",
            Self::Week => "qdr:w",
            Self::Month => "qdr:m",
            Self::Year => "qdr:y",
        }
    }
}

/// Requested provider search effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SearchDepth {
    /// Prefer the lowest-latency search mode.
    Fast,
    /// Prefer the provider's ordinary balanced mode.
    Balanced,
    /// Prefer a deeper, potentially slower or more expensive mode.
    Deep,
}

/// Validated provider-neutral preferences copied into adapter runtime state.
#[derive(Clone, Debug, Default)]
pub(super) struct ProviderOptions {
    /// Optional PDF parsing behavior.
    pub(super) fetch_pdf_parsing: Option<PdfParsing>,
    /// Optional PDF page limit.
    pub(super) fetch_pdf_max_pages: Option<u32>,
    /// Optional search freshness window.
    pub(super) search_recency: Option<SearchRecency>,
    /// Soft provider-side excluded-domain hints.
    pub(super) search_exclude_domains: Box<[String]>,
    /// Optional uppercase ISO alpha-2 search country.
    pub(super) search_country: Option<String>,
    /// Optional lowercase BCP-47-like search language.
    pub(super) search_language: Option<String>,
    /// Optional search effort.
    pub(super) search_depth: Option<SearchDepth>,
    /// Optional provider-side search content budget.
    pub(super) search_max_content_chars: Option<u32>,
    /// Optional provider-side fetch content budget.
    pub(super) fetch_max_content_chars: Option<u32>,
    /// Optional provider-side search cache age.
    pub(super) search_cache_max_age_seconds: Option<u64>,
    /// Optional provider-side fetch cache age.
    pub(super) fetch_cache_max_age_seconds: Option<u64>,
}

/// Raw provider-neutral configuration fields.
#[derive(Debug, Default)]
pub(super) struct RawProviderOptions {
    /// Optional PDF parsing behavior.
    pub(super) fetch_pdf_parsing: Option<PdfParsing>,
    /// Optional PDF page limit.
    pub(super) fetch_pdf_max_pages: Option<u32>,
    /// Optional search freshness window.
    pub(super) search_recency: Option<SearchRecency>,
    /// Soft provider-side excluded-domain hints.
    pub(super) search_exclude_domains: Vec<String>,
    /// Optional search country.
    pub(super) search_country: Option<String>,
    /// Optional search language.
    pub(super) search_language: Option<String>,
    /// Optional search effort.
    pub(super) search_depth: Option<SearchDepth>,
    /// Optional provider-side search content budget.
    pub(super) search_max_content_chars: Option<u32>,
    /// Optional provider-side fetch content budget.
    pub(super) fetch_max_content_chars: Option<u32>,
    /// Optional provider-side search cache age.
    pub(super) search_cache_max_age_seconds: Option<u64>,
    /// Optional provider-side fetch cache age.
    pub(super) fetch_cache_max_age_seconds: Option<u64>,
}

impl RawProviderOptions {
    /// Validate and normalize provider-neutral preferences.
    pub(super) fn validate(self) -> Result<ProviderOptions, String> {
        if self.fetch_pdf_parsing == Some(PdfParsing::Disabled)
            && self.fetch_pdf_max_pages.is_some()
        {
            return Err(
                "`fetch_pdf_max_pages` cannot be set when PDF parsing is disabled".to_owned(),
            );
        }
        if let Some(pages) = self.fetch_pdf_max_pages
            && !(1..=10_000).contains(&pages)
        {
            return Err("`fetch_pdf_max_pages` must be between 1 and 10000".to_owned());
        }
        for (field, value) in [
            ("search_max_content_chars", self.search_max_content_chars),
            ("fetch_max_content_chars", self.fetch_max_content_chars),
        ] {
            if let Some(value) = value
                && !(1..=MAX_CONTENT_CHARS).contains(&value)
            {
                return Err(format!(
                    "`{field}` must be between 1 and {MAX_CONTENT_CHARS}"
                ));
            }
        }
        for (field, value) in [
            (
                "search_cache_max_age_seconds",
                self.search_cache_max_age_seconds,
            ),
            (
                "fetch_cache_max_age_seconds",
                self.fetch_cache_max_age_seconds,
            ),
        ] {
            if let Some(value) = value
                && MAX_CACHE_AGE_SECONDS < value
            {
                return Err(format!(
                    "`{field}` must be no greater than {MAX_CACHE_AGE_SECONDS}"
                ));
            }
        }
        let search_country = self.search_country.map(validate_country).transpose()?;
        let search_language = self.search_language.map(validate_language).transpose()?;
        let search_exclude_domains = validate_domains(self.search_exclude_domains)?;
        if let Some(seconds) = self.fetch_cache_max_age_seconds {
            seconds.checked_mul(1000).ok_or_else(|| {
                "`fetch_cache_max_age_seconds` is too large to convert to milliseconds".to_owned()
            })?;
        }
        Ok(ProviderOptions {
            fetch_pdf_parsing: self.fetch_pdf_parsing,
            fetch_pdf_max_pages: self.fetch_pdf_max_pages,
            search_recency: self.search_recency,
            search_exclude_domains,
            search_country,
            search_language,
            search_depth: self.search_depth,
            search_max_content_chars: self.search_max_content_chars,
            fetch_max_content_chars: self.fetch_max_content_chars,
            search_cache_max_age_seconds: self.search_cache_max_age_seconds,
            fetch_cache_max_age_seconds: self.fetch_cache_max_age_seconds,
        })
    }
}

fn validate_country(value: String) -> Result<String, String> {
    let value = value.to_ascii_uppercase();
    if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err("`search_country` must be a two-letter country code".to_owned());
    }
    Ok(value)
}

fn validate_language(value: String) -> Result<String, String> {
    let value = value.to_ascii_lowercase();
    if !(2..=35).contains(&value.len())
        || value.starts_with('-')
        || value.ends_with('-')
        || value
            .split('-')
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        return Err("`search_language` must be a BCP-47-like language tag".to_owned());
    }
    Ok(value)
}

fn validate_domains(domains: Vec<String>) -> Result<Box<[String]>, String> {
    if domains.len() > MAX_EXCLUDED_DOMAINS {
        return Err(format!(
            "`search_exclude_domains` allows at most {MAX_EXCLUDED_DOMAINS} domains"
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, domain) in domains.iter().enumerate() {
        if domain.is_empty()
            || domain.len() > 253
            || domain != &domain.to_ascii_lowercase()
            || domain.starts_with('.')
            || domain.ends_with('.')
            || domain.contains("://")
            || domain.contains(['/', '?', '#', ':', '@', '*'])
            || domain.parse::<std::net::IpAddr>().is_ok()
            || domain.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err(format!(
                "`search_exclude_domains[{index}]` must be a lowercase DNS domain name"
            ));
        }
        if !seen.insert(domain) {
            return Err(format!(
                "`search_exclude_domains[{index}]` duplicates `{domain}`"
            ));
        }
    }
    Ok(domains.into_boxed_slice())
}
