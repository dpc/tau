use crate::{CandidateSet, Corpus, RunOrigin, evaluate};

const CORPUS: &[u8] = include_bytes!("../fixtures/corpus-v1.json");
const CANDIDATES: &[u8] = include_bytes!("../fixtures/offline-candidates-v1.json");

/// The checked-in synthetic corpus and deterministic oracle remain valid and
/// complete.
#[test]
fn checked_in_offline_oracle_passes_every_fact_and_case() {
    let result = evaluate(CORPUS, CANDIDATES).expect("score");

    assert_eq!(result.coverage_basis_points, 10_000);
    assert_eq!(result.passing_cases, result.total_cases);
    assert_eq!(result.matched_required_facts, result.total_required_facts);
}

/// Scoring stays deterministic and reports missing facts and forbidden claims
/// separately.
#[test]
fn scoring_reports_a_failed_candidate_without_semantic_guessing() {
    let mut candidates: CandidateSet =
        serde_json::from_slice(CANDIDATES).expect("fixture candidates");
    candidates.candidates[0].summary = "Arrays are rejected.".into();

    let changed = serde_json::to_vec(&candidates).expect("changed candidates");
    let first = evaluate(CORPUS, &changed).expect("first score");
    let second = evaluate(CORPUS, &changed).expect("second score");

    assert_eq!(first.coverage_basis_points, second.coverage_basis_points);
    assert!(!first.cases[0].passed);
    assert_eq!(first.cases[0].missing_fact_ids.len(), 3);
    assert_eq!(
        first.cases[0].matched_forbidden_claims,
        ["arrays are rejected"]
    );
    let encoded = serde_json::to_string(&first).expect("serialize result");
    assert!(!encoded.contains("Arrays are rejected"));
    assert!(!encoded.contains("developer asks for a parser"));
}

/// Public corpus validation blocks common accidental credential and host-path
/// markers.
#[test]
fn synthetic_public_corpus_rejects_suspicious_private_markers() {
    let mut corpus: Corpus = serde_json::from_slice(CORPUS).expect("fixture corpus");
    corpus.cases[0].input = "Read /home/example/private-session.json".into();

    let error = corpus.validate().expect_err("private path must fail");

    assert!(error.contains("suspicious marker"));
}

/// Live provenance accepts only complete metadata with the explicit
/// cost/privacy token.
#[test]
fn live_provenance_requires_explicit_opt_in_and_exact_metadata() {
    let mut value: serde_json::Value =
        serde_json::from_slice(CANDIDATES).expect("fixture candidates");
    value["origin"] = serde_json::json!({
        "kind": "live",
        "provider": "example",
        "model": "summary-model",
        "model_version": "2026-08-27",
        "configuration": "temperature=0",
        "date_utc": "2026-08-27"
    });
    let missing = serde_json::from_value::<CandidateSet>(value.clone())
        .expect_err("missing opt-in must fail");
    assert!(missing.to_string().contains("opt_in"));

    value["origin"]["opt_in"] = serde_json::json!("I_REVIEWED_PRIVACY_AND_ACCEPT_PROVIDER_COST");
    let candidates: CandidateSet = serde_json::from_value(value).expect("explicit live opt-in");
    assert!(matches!(candidates.origin, RunOrigin::Live { .. }));
    candidates.validate().expect("complete live provenance");
}

/// Result provenance rejects likely credentials instead of copying them into
/// durable output.
#[test]
fn provenance_rejects_secret_like_configuration() {
    let mut candidates: CandidateSet =
        serde_json::from_slice(CANDIDATES).expect("fixture candidates");
    if let RunOrigin::Offline { configuration, .. } = &mut candidates.origin {
        *configuration = "Authorization: Bearer private-value".into();
    }

    let error = candidates
        .validate()
        .expect_err("secret-like metadata must fail");

    assert!(error.contains("secret-like marker"));
}

/// Validation rejects normalized-empty scoring text before it can match every
/// summary.
#[test]
fn whitespace_only_fact_and_summary_are_invalid() {
    let mut corpus: Corpus = serde_json::from_slice(CORPUS).expect("fixture corpus");
    corpus.cases[0].required_facts[0].any_of[0] = " \n\t ".into();
    assert!(
        corpus
            .validate()
            .expect_err("empty fact")
            .contains("must contain")
    );

    let mut candidates: CandidateSet =
        serde_json::from_slice(CANDIDATES).expect("fixture candidates");
    candidates.candidates[0].summary = " \n\t ".into();
    assert!(
        candidates
            .validate()
            .expect_err("empty summary")
            .contains("must contain")
    );
}

/// Live metadata accepts real leap days but rejects impossible calendar dates.
#[test]
fn live_date_requires_a_real_gregorian_date() {
    let mut value: serde_json::Value =
        serde_json::from_slice(CANDIDATES).expect("fixture candidates");
    value["origin"] = serde_json::json!({
        "kind": "live",
        "provider": "example",
        "model": "summary-model",
        "model_version": "snapshot",
        "configuration": "temperature=0",
        "date_utc": "2026-02-31",
        "opt_in": "I_REVIEWED_PRIVACY_AND_ACCEPT_PROVIDER_COST"
    });
    let invalid: CandidateSet = serde_json::from_value(value.clone()).expect("candidate shape");
    assert!(
        invalid
            .validate()
            .expect_err("impossible date")
            .contains("date_utc")
    );

    value["origin"]["date_utc"] = serde_json::json!("2024-02-29");
    let leap: CandidateSet = serde_json::from_value(value).expect("leap candidate shape");
    leap.validate().expect("real leap date");
}

/// Atomic evaluation validates parsed bytes instead of exposing unchecked
/// aggregation.
#[test]
fn evaluation_rejects_zero_fact_input_without_panicking() {
    let mut value: serde_json::Value = serde_json::from_slice(CORPUS).expect("fixture corpus");
    value["cases"][0]["required_facts"] = serde_json::json!([]);
    let invalid = serde_json::to_vec(&value).expect("invalid corpus bytes");

    let error = evaluate(&invalid, CANDIDATES).expect_err("zero facts must fail");

    assert!(error.contains("required facts"));
}
