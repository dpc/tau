use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::candidates::{CandidateSet, RunOrigin};
use crate::corpus::Corpus;

/// Stable schema version emitted for score results.
pub const RESULT_SCHEMA_VERSION: u32 = 1;
/// Version of normalization, matching, aggregation, and pass/fail semantics.
///
/// Increment this value whenever identical inputs can produce different scores
/// or case details. The complete result fixture locks the corresponding shape.
pub const SCORING_CONTRACT: &str = "literal-normalized-facts-v1";

/// Reproducible score record that excludes source and generated summary text.
#[derive(Debug, Serialize)]
pub struct ResultRecord {
    /// Schema version for structural compatibility.
    pub(crate) schema_version: u32,
    /// Name and package version of the deterministic scorer.
    pub(crate) scorer: String,
    /// Stable scoring-semantics revision, independent of package version.
    pub(crate) scoring_contract: String,
    /// Stable corpus identity.
    pub(crate) corpus_id: String,
    /// Exact corpus content version.
    pub(crate) corpus_version: u32,
    /// SHA-256 of the corpus bytes supplied to the scorer.
    pub(crate) corpus_sha256: String,
    /// SHA-256 of the candidate-set bytes supplied to the scorer.
    pub(crate) candidates_sha256: String,
    /// Reproducibility metadata copied from the candidate set.
    pub(crate) origin: RunOrigin,
    /// Total satisfied required facts across all cases.
    pub(crate) matched_required_facts: usize,
    /// Total required facts across all cases.
    pub(crate) total_required_facts: usize,
    /// Integer coverage in basis points, avoiding floating-point variation.
    pub(crate) coverage_basis_points: u32,
    /// Number of cases that retained every fact and added no forbidden claim.
    pub(crate) passing_cases: usize,
    /// Total number of scored cases.
    pub(crate) total_cases: usize,
    /// Stable case-ordered details for diagnosis and comparison.
    pub(crate) cases: Vec<CaseResult>,
}

/// Detailed deterministic outcome for one corpus case.
#[derive(Debug, Serialize)]
pub struct CaseResult {
    /// Stable corpus case identifier.
    pub(crate) case_id: String,
    /// Required fact identifiers found in the candidate.
    pub(crate) matched_fact_ids: Vec<String>,
    /// Required fact identifiers absent from the candidate.
    pub(crate) missing_fact_ids: Vec<String>,
    /// Forbidden literal claims found in the candidate.
    pub(crate) matched_forbidden_claims: Vec<String>,
    /// Whether all requirements passed for this case.
    pub(crate) passed: bool,
}

/// Scores one validated candidate set and produces a text-free result record.
fn score(
    corpus: &Corpus,
    candidates: &CandidateSet,
    corpus_bytes: &[u8],
    candidate_bytes: &[u8],
) -> Result<ResultRecord, String> {
    if corpus.corpus_id != candidates.corpus_id
        || corpus.corpus_version != candidates.corpus_version
    {
        return Err("candidate set targets a different corpus id or version".into());
    }
    let by_case: HashMap<_, _> = candidates
        .candidates
        .iter()
        .map(|candidate| (candidate.case_id.as_str(), candidate))
        .collect();
    if by_case.len() != corpus.cases.len()
        || corpus
            .cases
            .iter()
            .any(|case| !by_case.contains_key(case.id.as_str()))
    {
        return Err("candidate set must contain exactly one summary for every corpus case".into());
    }

    let mut results = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        let candidate = by_case
            .get(case.id.as_str())
            .ok_or_else(|| format!("missing candidate for case {:?}", case.id))?;
        let normalized = normalize(&candidate.summary);
        let mut matched = Vec::new();
        let mut missing = Vec::new();
        for fact in &case.required_facts {
            if fact
                .any_of
                .iter()
                .any(|alternative| normalized.contains(&normalize(alternative)))
            {
                matched.push(fact.id.clone());
            } else {
                missing.push(fact.id.clone());
            }
        }
        let forbidden: Vec<_> = case
            .forbidden_claims
            .iter()
            .filter(|claim| normalized.contains(&normalize(claim)))
            .cloned()
            .collect();
        results.push(CaseResult {
            case_id: case.id.clone(),
            passed: missing.is_empty() && forbidden.is_empty(),
            matched_fact_ids: matched,
            missing_fact_ids: missing,
            matched_forbidden_claims: forbidden,
        });
    }

    let matched_required_facts = results
        .iter()
        .map(|result| result.matched_fact_ids.len())
        .sum();
    let total_required_facts = corpus
        .cases
        .iter()
        .map(|case| case.required_facts.len())
        .sum();
    let coverage_basis_points =
        u32::try_from(matched_required_facts * 10_000 / total_required_facts)
            .map_err(|_| "coverage does not fit result schema")?;
    let passing_cases = results.iter().filter(|result| result.passed).count();

    Ok(ResultRecord {
        schema_version: RESULT_SCHEMA_VERSION,
        scorer: format!("tau-summary-eval/{}", env!("CARGO_PKG_VERSION")),
        scoring_contract: SCORING_CONTRACT.into(),
        corpus_id: corpus.corpus_id.clone(),
        corpus_version: corpus.corpus_version,
        corpus_sha256: sha256(corpus_bytes),
        candidates_sha256: sha256(candidate_bytes),
        origin: candidates.origin.clone(),
        matched_required_facts,
        total_required_facts,
        coverage_basis_points,
        passing_cases,
        total_cases: corpus.cases.len(),
        cases: results,
    })
}

/// Parses, validates, hashes, and scores the same exact input byte sequences.
///
/// Keeping these operations atomic prevents callers from attaching unrelated
/// digests to public record fields or bypassing validation before aggregation.
pub fn evaluate(corpus_bytes: &[u8], candidate_bytes: &[u8]) -> Result<ResultRecord, String> {
    let corpus: Corpus = serde_json::from_slice(corpus_bytes)
        .map_err(|error| format!("cannot parse corpus: {error}"))?;
    let candidates: CandidateSet = serde_json::from_slice(candidate_bytes)
        .map_err(|error| format!("cannot parse candidates: {error}"))?;
    corpus.validate()?;
    candidates.validate()?;
    score(&corpus, &candidates, corpus_bytes, candidate_bytes)
}

/// Normalizes matching while preserving a deliberately simple scoring contract.
fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Renders a stable lowercase SHA-256 digest without another dependency.
fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
