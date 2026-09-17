use super::*;

/// Creates a validated prompt identifier for compact-membership tests.
fn prompt_id(value: &str) -> tau_proto::AgentPromptId {
    tau_proto::AgentPromptId::parse(value).expect("valid prompt id")
}

/// Canonical sequential IDs merge despite duplicate and out-of-order insertion.
#[test]
fn canonical_ids_merge_into_exact_ranges() {
    let mut ids = CompletedPromptIds::default();
    assert!(ids.insert(prompt_id("ap-agent-2")));
    assert!(ids.insert(prompt_id("ap-agent-0")));
    assert!(ids.insert(prompt_id("ap-agent-1")));
    assert!(!ids.insert(prompt_id("ap-agent-2")));

    assert_eq!(ids.numeric_ranges.get("ap-agent-"), Some(&vec![(0, 2)]));
    for value in ["ap-agent-0", "ap-agent-1", "ap-agent-2"] {
        assert!(ids.contains(&prompt_id(value)));
    }
    assert!(!ids.contains(&prompt_id("ap-agent-3")));
}

/// Prefixes remain independent and boundary values never overflow adjacency
/// checks.
#[test]
fn prefixes_and_u64_boundaries_remain_independent() {
    let mut ids = CompletedPromptIds::default();
    for value in ["ap-a-0", "ap-a-18446744073709551615", "ap-b-0", "ap-b-1"] {
        assert!(ids.insert(prompt_id(value)));
    }

    assert_eq!(
        ids.numeric_ranges["ap-a-"],
        vec![(0, 0), (u64::MAX, u64::MAX)]
    );
    assert_eq!(ids.numeric_ranges["ap-b-"], vec![(0, 1)]);
}

/// Removing members preserves gaps, trims endpoints, and splits interior ranges
/// exactly.
#[test]
fn removal_preserves_gaps_and_splits_ranges() {
    let mut ids = CompletedPromptIds::default();
    for number in 0..=6 {
        assert!(ids.insert(prompt_id(&format!("ap-agent-{number}"))));
    }

    assert!(ids.remove(&prompt_id("ap-agent-3")));
    assert!(ids.remove(&prompt_id("ap-agent-0")));
    assert!(ids.remove(&prompt_id("ap-agent-6")));
    assert!(!ids.remove(&prompt_id("ap-agent-3")));
    assert_eq!(ids.numeric_ranges["ap-agent-"], vec![(1, 2), (4, 5)]);
    for value in ["ap-agent-1", "ap-agent-2", "ap-agent-4", "ap-agent-5"] {
        assert!(ids.contains(&prompt_id(value)));
    }
    for value in ["ap-agent-1", "ap-agent-2", "ap-agent-4", "ap-agent-5"] {
        assert!(ids.remove(&prompt_id(value)));
    }
    assert!(!ids.numeric_ranges.contains_key("ap-agent-"));
}

/// Noncanonical, overflowing, and opaque IDs retain exact string identity in
/// fallback storage.
#[test]
fn noncanonical_ids_use_exact_fallback() {
    let mut ids = CompletedPromptIds::default();
    for value in [
        "ap-agent-01",
        "ap-agent-18446744073709551616",
        "opaque",
        "opaque-tail",
    ] {
        assert!(ids.insert(prompt_id(value)));
        assert!(ids.contains(&prompt_id(value)));
    }

    assert_eq!(ids.fallback.len(), 4);
    assert!(ids.insert(prompt_id("ap-agent-1")));
    assert!(ids.contains(&prompt_id("ap-agent-01")));
    assert!(ids.contains(&prompt_id("ap-agent-1")));
    assert!(ids.remove(&prompt_id("ap-agent-01")));
    assert!(!ids.contains(&prompt_id("ap-agent-01")));
    assert!(ids.contains(&prompt_id("ap-agent-1")));
}
