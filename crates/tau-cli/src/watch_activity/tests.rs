use super::*;

fn topology(
    edges: &[(&str, &str)],
) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>) {
    let mut forward: HashMap<String, Vec<String>> = HashMap::new();
    let mut reverse: HashMap<String, Vec<String>> = HashMap::new();
    for (watcher, watched) in edges {
        forward
            .entry((*watcher).to_owned())
            .or_default()
            .push((*watched).to_owned());
        reverse
            .entry((*watched).to_owned())
            .or_default()
            .push((*watcher).to_owned());
    }
    for targets in forward.values_mut() {
        targets.sort();
    }
    for watchers in reverse.values_mut() {
        watchers.sort();
    }
    (forward, reverse)
}

/// A running leaf must activate every watched ancestor without flattening
/// the projection's unique effective-target set.
#[test]
fn projects_a_recursive_chain() {
    let (forward, reverse) = topology(&[("a", "b"), ("b", "c")]);
    let projection = WatchActivityProjection::new(
        &forward,
        &reverse,
        HashSet::from([("b".to_owned(), "c".to_owned())]),
    );

    assert!(projection.watcher_is_active("a"));
    assert!(projection.watcher_is_active("b"));
    assert_eq!(
        projection.effective_targets,
        HashSet::from(["b".to_owned(), "c".to_owned()])
    );
    assert_eq!(projection.witness_for("b", &forward).as_deref(), Some("c"));
}

/// Equal-depth witnesses must use stable id regardless of edge insertion
/// order.
#[test]
fn selects_a_stable_witness_from_a_fork() {
    let (forward, reverse) = topology(&[("a", "c"), ("a", "b"), ("b", "z"), ("c", "y")]);
    let projection = WatchActivityProjection::new(
        &forward,
        &reverse,
        HashSet::from([
            ("b".to_owned(), "z".to_owned()),
            ("c".to_owned(), "y".to_owned()),
        ]),
    );

    assert_eq!(projection.witness_for("a", &forward).as_deref(), Some("y"));
}

/// A reconverging diamond must count its shared active descendant only once.
#[test]
fn deduplicates_a_shared_diamond_descendant() {
    let (forward, reverse) = topology(&[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")]);
    let projection = WatchActivityProjection::new(
        &forward,
        &reverse,
        HashSet::from([
            ("b".to_owned(), "d".to_owned()),
            ("c".to_owned(), "d".to_owned()),
        ]),
    );

    assert_eq!(
        projection.effective_targets,
        HashSet::from(["b".to_owned(), "c".to_owned(), "d".to_owned()])
    );
}

/// Activity only propagates from descendants toward watchers, never down
/// from a directly running incoming edge.
#[test]
fn does_not_propagate_activity_downward() {
    let (forward, reverse) = topology(&[("parent", "a"), ("a", "b")]);
    let projection = WatchActivityProjection::new(
        &forward,
        &reverse,
        HashSet::from([("parent".to_owned(), "a".to_owned())]),
    );

    assert!(!projection.watcher_is_active("a"));
    assert!(!projection.watcher_is_active("b"));
}

/// A deep live DAG must be handled iteratively rather than consuming call
/// stack in proportion to watch depth.
#[test]
fn projects_a_deep_chain_iteratively() {
    let edges: Vec<_> = (0..2_048)
        .map(|index| (format!("a{index}"), format!("a{}", index + 1)))
        .collect();
    let borrowed: Vec<_> = edges
        .iter()
        .map(|(watcher, watched)| (watcher.as_str(), watched.as_str()))
        .collect();
    let (forward, reverse) = topology(&borrowed);
    let projection = WatchActivityProjection::new(
        &forward,
        &reverse,
        HashSet::from([("a2047".to_owned(), "a2048".to_owned())]),
    );

    assert!(projection.watcher_is_active("a0"));
    assert_eq!(projection.effective_targets.len(), 2_048);
}
