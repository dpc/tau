use std::collections::HashSet;

use serde::Deserialize;

/// Stable schema version accepted for summary-quality corpora.
pub const CORPUS_SCHEMA_VERSION: u32 = 1;

/// A versioned collection of synthetic, public-safe summary exercises.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    /// Schema version for structural compatibility.
    pub schema_version: u32,
    /// Stable corpus identity shared with candidate sets.
    pub corpus_id: String,
    /// Monotonically increasing content version.
    pub corpus_version: u32,
    /// Required classification; versioned corpora must contain only synthetic
    /// data.
    pub privacy: CorpusPrivacy,
    /// Independently scored summary exercises.
    pub cases: Vec<CorpusCase>,
}

/// Privacy classification allowed for a versioned corpus.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CorpusPrivacy {
    /// Deliberately authored synthetic text suitable for the public repository.
    SyntheticPublic,
}

/// One synthetic transcript and its deterministic scoring rules.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusCase {
    /// Stable identifier used to join candidates and results.
    pub id: String,
    /// Synthetic source text presented to a summarizer.
    pub input: String,
    /// Facts that a useful summary must retain.
    pub required_facts: Vec<RequiredFact>,
    /// Claims whose presence makes the summary fail.
    pub forbidden_claims: Vec<String>,
}

/// One semantic fact represented by deterministic textual alternatives.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredFact {
    /// Stable fact identifier reported in detailed results.
    pub id: String,
    /// Literal alternatives, any one of which satisfies this fact.
    pub any_of: Vec<String>,
}

impl Corpus {
    /// Rejects malformed, unbounded, or suspicious corpus content before
    /// scoring.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CORPUS_SCHEMA_VERSION {
            return Err(format!(
                "unsupported corpus schema version {}",
                self.schema_version
            ));
        }
        validate_identifier("corpus_id", &self.corpus_id)?;
        if self.corpus_version == 0 {
            return Err("corpus_version must be positive".into());
        }
        if self.privacy != CorpusPrivacy::SyntheticPublic {
            return Err("only synthetic-public corpora are accepted".into());
        }
        if self.cases.is_empty() || self.cases.len() > 100 {
            return Err("corpus must contain 1..=100 cases".into());
        }
        let mut case_ids = HashSet::new();
        for case in &self.cases {
            case.validate(&mut case_ids)?;
        }
        Ok(())
    }
}

impl CorpusCase {
    /// Validates one bounded synthetic case and its unique fact identifiers.
    fn validate(&self, case_ids: &mut HashSet<String>) -> Result<(), String> {
        validate_identifier("case id", &self.id)?;
        if !case_ids.insert(self.id.clone()) {
            return Err(format!("duplicate case id {:?}", self.id));
        }
        validate_text("case input", &self.input, 16_384)?;
        reject_suspicious_private_text(&self.input)?;
        if self.required_facts.is_empty() || self.required_facts.len() > 100 {
            return Err(format!(
                "case {:?} must have 1..=100 required facts",
                self.id
            ));
        }
        let mut fact_ids = HashSet::new();
        for fact in &self.required_facts {
            fact.validate(&mut fact_ids)?;
        }
        if self.forbidden_claims.len() > 100 {
            return Err(format!("case {:?} has too many forbidden claims", self.id));
        }
        for claim in &self.forbidden_claims {
            validate_text("forbidden claim", claim, 256)?;
            reject_suspicious_private_text(claim)?;
        }
        Ok(())
    }
}

impl RequiredFact {
    /// Validates one fact and its bounded, nonempty matching alternatives.
    fn validate(&self, fact_ids: &mut HashSet<String>) -> Result<(), String> {
        validate_identifier("fact id", &self.id)?;
        if !fact_ids.insert(self.id.clone()) {
            return Err(format!("duplicate fact id {:?}", self.id));
        }
        if self.any_of.is_empty() || self.any_of.len() > 20 {
            return Err(format!("fact {:?} must have 1..=20 alternatives", self.id));
        }
        for alternative in &self.any_of {
            validate_text("fact alternative", alternative, 256)?;
            reject_suspicious_private_text(alternative)?;
        }
        Ok(())
    }
}

/// Validates a stable lowercase identifier.
fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 80
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
        });
    if valid {
        Ok(())
    } else {
        Err(format!("{label} must match [a-z0-9_-]{{1,80}}"))
    }
}

/// Validates bounded text without control characters other than ordinary
/// whitespace.
fn validate_text(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.split_whitespace().next().is_none() || value.len() > maximum {
        return Err(format!("{label} must contain 1..={maximum} bytes"));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(format!("{label} contains a disallowed control character"));
    }
    Ok(())
}

/// Flags common accidental secret and host-path markers in public corpus text.
fn reject_suspicious_private_text(value: &str) -> Result<(), String> {
    const MARKERS: &[&str] = &[
        "/home/",
        "/Users/",
        "BEGIN PRIVATE KEY",
        "api_key",
        "access_token",
        "sk-",
        "@gmail.com",
    ];
    if let Some(marker) = MARKERS.iter().find(|marker| value.contains(**marker)) {
        Err(format!(
            "synthetic-public case input contains suspicious marker {marker:?}"
        ))
    } else {
        Ok(())
    }
}
