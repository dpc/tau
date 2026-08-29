use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Stable schema version accepted for candidate sets.
pub const CANDIDATE_SCHEMA_VERSION: u32 = 1;

/// Summaries to score against one exact corpus version.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSet {
    /// Schema version for structural compatibility.
    pub schema_version: u32,
    /// Corpus identity expected by this candidate set.
    pub corpus_id: String,
    /// Exact corpus content version used to produce the summaries.
    pub corpus_version: u32,
    /// Reproducibility metadata for the generation run.
    pub origin: RunOrigin,
    /// One generated summary per corpus case.
    pub candidates: Vec<Candidate>,
}

/// Generation provenance preserved in the stable result record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RunOrigin {
    /// A checked-in or otherwise deterministic candidate producer.
    Offline {
        /// Exact producer name and version or revision.
        generator: String,
        /// Complete stable configuration description.
        configuration: String,
    },
    /// An explicitly initiated external-provider trial.
    Live {
        /// Provider or service identity.
        provider: String,
        /// Provider model identity.
        model: String,
        /// Exact model snapshot/version, or an explicit provider-reported
        /// alias.
        model_version: String,
        /// Complete stable generation and judge configuration.
        configuration: String,
        /// UTC calendar date in `YYYY-MM-DD` form.
        date_utc: String,
        /// Required acknowledgement recorded by the operator.
        opt_in: LiveOptIn,
    },
}

/// Exact opt-in token required for live trial records.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum LiveOptIn {
    /// The operator reviewed corpus privacy and accepts external-provider cost.
    #[serde(rename = "I_REVIEWED_PRIVACY_AND_ACCEPT_PROVIDER_COST")]
    ReviewedPrivacyAndAcceptProviderCost,
}

/// One generated summary associated with a corpus case.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Stable corpus case identifier.
    pub case_id: String,
    /// Summary text to evaluate.
    pub summary: String,
}

impl CandidateSet {
    /// Rejects malformed or incomplete candidate sets before any result is
    /// emitted.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CANDIDATE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported candidate schema version {}",
                self.schema_version
            ));
        }
        validate_metadata("corpus_id", &self.corpus_id, 80)?;
        if self.corpus_version == 0 {
            return Err("corpus_version must be positive".into());
        }
        self.origin.validate()?;
        if self.candidates.is_empty() || self.candidates.len() > 100 {
            return Err("candidate set must contain 1..=100 summaries".into());
        }
        let mut ids = HashSet::new();
        for candidate in &self.candidates {
            validate_metadata("case_id", &candidate.case_id, 80)?;
            if !ids.insert(candidate.case_id.as_str()) {
                return Err(format!("duplicate candidate case {:?}", candidate.case_id));
            }
            validate_metadata("summary", &candidate.summary, 32_768)?;
        }
        Ok(())
    }
}

impl RunOrigin {
    /// Validates complete run metadata, including an unmistakable live opt-in.
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Offline {
                generator,
                configuration,
            } => {
                validate_metadata("generator", generator, 256)?;
                validate_metadata("configuration", configuration, 2_048)
            }
            Self::Live {
                provider,
                model,
                model_version,
                configuration,
                date_utc,
                opt_in,
            } => {
                validate_metadata("provider", provider, 256)?;
                validate_metadata("model", model, 256)?;
                validate_metadata("model_version", model_version, 256)?;
                validate_metadata("configuration", configuration, 2_048)?;
                validate_date(date_utc)?;
                if *opt_in != LiveOptIn::ReviewedPrivacyAndAcceptProviderCost {
                    return Err("live trial opt-in acknowledgement is required".into());
                }
                Ok(())
            }
        }
    }
}

/// Validates bounded, nonempty metadata.
fn validate_metadata(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.split_whitespace().next().is_none() || value.len() > maximum {
        return Err(format!("{label} must contain 1..={maximum} bytes"));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(format!("{label} contains a disallowed control character"));
    }
    const SECRET_MARKERS: &[&str] = &[
        "BEGIN PRIVATE KEY",
        "api_key",
        "access_token",
        "authorization:",
        "bearer ",
        "sk-",
    ];
    let lowered = value.to_ascii_lowercase();
    if let Some(marker) = SECRET_MARKERS
        .iter()
        .find(|marker| lowered.contains(&marker.to_ascii_lowercase()))
    {
        return Err(format!("{label} contains secret-like marker {marker:?}"));
    }
    Ok(())
}

/// Validates the intentionally narrow stable UTC date representation.
fn validate_date(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    let year = value.get(..4).and_then(|part| part.parse::<u32>().ok());
    let month = value.get(5..7).and_then(|part| part.parse::<u8>().ok());
    let day = value.get(8..10).and_then(|part| part.parse::<u8>().ok());
    let valid_ranges = shape
        && year
            .zip(month)
            .zip(day)
            .is_some_and(|((year, month), day)| valid_calendar_date(year, month, day));
    if valid_ranges {
        Ok(())
    } else {
        Err("date_utc must use YYYY-MM-DD".into())
    }
}

/// Checks Gregorian month lengths, including the leap-year exception.
fn valid_calendar_date(year: u32, month: u8, day: u8) -> bool {
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };
    (1..=maximum_day).contains(&day)
}
