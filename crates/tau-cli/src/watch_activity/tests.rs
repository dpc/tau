use super::*;

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("test agent id")
}

fn topology(
    edges: &[(&str, &str)],
) -> (
    HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
    HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
) {
    let mut forward: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>> = HashMap::new();
    let mut reverse: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>> = HashMap::new();
    for (watcher, watched) in edges {
        forward
            .entry(agent_id(watcher))
            .or_default()
            .push(agent_id(watched));
        reverse
            .entry(agent_id(watched))
            .or_default()
            .push(agent_id(watcher));
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
    let projection = WatchGraphProjection::new(
        &forward,
        &reverse,
        HashSet::from([(agent_id("b"), agent_id("c"))]),
    );

    assert!(projection.watcher_is_active(&agent_id("a")));
    assert!(projection.watcher_is_active(&agent_id("b")));
    assert_eq!(
        projection.effective_targets,
        HashSet::from([agent_id("b"), agent_id("c")])
    );
    assert_eq!(
        projection.witness_for(&agent_id("b"), &forward).as_deref(),
        Some("c")
    );
}

/// Equal-depth witnesses must use stable id regardless of edge insertion
/// order.
#[test]
fn selects_a_stable_witness_from_a_fork() {
    let (forward, reverse) = topology(&[("a", "c"), ("a", "b"), ("b", "z"), ("c", "y")]);
    let projection = WatchGraphProjection::new(
        &forward,
        &reverse,
        HashSet::from([
            (agent_id("b"), agent_id("z")),
            (agent_id("c"), agent_id("y")),
        ]),
    );

    assert_eq!(
        projection.witness_for(&agent_id("a"), &forward).as_deref(),
        Some("y")
    );
}

/// A reconverging diamond must count its shared active descendant only once.
#[test]
fn deduplicates_a_shared_diamond_descendant() {
    let (forward, reverse) = topology(&[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")]);
    let projection = WatchGraphProjection::new(
        &forward,
        &reverse,
        HashSet::from([
            (agent_id("b"), agent_id("d")),
            (agent_id("c"), agent_id("d")),
        ]),
    );

    assert_eq!(
        projection.effective_targets,
        HashSet::from([agent_id("b"), agent_id("c"), agent_id("d")])
    );
}

/// Activity only propagates from descendants toward watchers, never down
/// from a directly running incoming edge.
#[test]
fn does_not_propagate_activity_downward() {
    let (forward, reverse) = topology(&[("parent", "a"), ("a", "b")]);
    let projection = WatchGraphProjection::new(
        &forward,
        &reverse,
        HashSet::from([(agent_id("parent"), agent_id("a"))]),
    );

    assert!(!projection.watcher_is_active(&agent_id("a")));
    assert!(!projection.watcher_is_active(&agent_id("b")));
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
    let projection = WatchGraphProjection::new(
        &forward,
        &reverse,
        HashSet::from([(agent_id("a2047"), agent_id("a2048"))]),
    );

    assert!(projection.watcher_is_active(&agent_id("a0")));
    assert_eq!(projection.effective_targets.len(), 2_048);
}

fn visible_ids(rows: &[VisibleWatchRow]) -> Vec<(&str, usize, Option<&str>)> {
    rows.iter()
        .map(|row| (row.agent_id.as_str(), row.depth, row.via.as_deref()))
        .collect()
}

/// Eight visible descendants expand completely, while the ninth switches the
/// result atomically back to the direct set.
#[test]
fn visible_rows_switch_from_eight_to_direct_only_at_nine() {
    let edges = (0..9)
        .map(|index| {
            let parent = if index == 0 {
                agent_id("root")
            } else {
                agent_id(&format!("a{}", index - 1))
            };
            (parent, agent_id(&format!("a{index}")))
        })
        .collect::<Vec<_>>();
    let borrowed = edges
        .iter()
        .map(|(watcher, watched)| (watcher.as_str(), watched.as_str()))
        .collect::<Vec<_>>();
    let (forward, _) = topology(&borrowed);

    let eight = WatchGraphProjection::visible_rows(
        &agent_id("root"),
        &forward,
        |agent_id| agent_id.as_str() != "a8",
        8,
    );
    assert_eq!(eight.len(), 8);
    assert_eq!(eight[7].agent_id.as_str(), "a7");
    assert_eq!(eight[7].depth, 8);

    let nine = WatchGraphProjection::visible_rows(&agent_id("root"), &forward, |_| true, 8);
    assert_eq!(visible_ids(&nine), vec![("a0", 1, None)]);
}

/// Recursive overflow never truncates an explicit direct set, even when that
/// set alone is larger than the expansion limit.
#[test]
fn visible_rows_keep_every_direct_watch_above_limit() {
    let edges = (0..10)
        .map(|index| (agent_id("root"), format!("d{index:02}")))
        .collect::<Vec<_>>();
    let borrowed = edges
        .iter()
        .map(|(watcher, watched)| (watcher.as_str(), watched.as_str()))
        .collect::<Vec<_>>();
    let (forward, _) = topology(&borrowed);

    let rows = WatchGraphProjection::visible_rows(&agent_id("root"), &forward, |_| true, 8);
    assert_eq!(rows.len(), 10);
    assert!(rows.iter().all(|row| row.depth == 1 && row.via.is_none()));
}

/// Malformed cycles must terminate, deduplicate their members, and never
/// reintroduce the viewed root as a watched row.
#[test]
fn visible_rows_are_cycle_safe_and_exclude_root() {
    let (forward, _) = topology(&[("root", "a"), ("a", "b"), ("b", "root")]);

    let rows = WatchGraphProjection::visible_rows(&agent_id("root"), &forward, |_| true, 8);
    assert_eq!(
        visible_ids(&rows),
        vec![("a", 1, None), ("b", 2, Some("a"))]
    );
}

/// Duplicate reachability chooses a shortest path and the lexicographically
/// first predecessor when equal-depth paths reconverge.
#[test]
fn visible_rows_choose_shortest_then_lexicographic_paths() {
    let (forward, _) = topology(&[
        ("root", "a"),
        ("root", "b"),
        ("root", "c"),
        ("a", "x"),
        ("x", "short"),
        ("b", "short"),
        ("b", "shared"),
        ("c", "shared"),
    ]);

    let rows = WatchGraphProjection::visible_rows(&agent_id("root"), &forward, |_| true, 8);
    let short = rows
        .iter()
        .find(|row| row.agent_id.as_str() == "short")
        .expect("shortest-path row");
    assert_eq!((short.depth, short.via.as_deref()), (2, Some("b")));
    let shared = rows
        .iter()
        .find(|row| row.agent_id.as_str() == "shared")
        .expect("equal-depth row");
    assert_eq!((shared.depth, shared.via.as_deref()), (2, Some("b")));
}

/// Hidden Done intermediates remain traversal nodes, while topology-only
/// descendants without stats remain visible and retain immediate context.
#[test]
fn visible_rows_traverse_hidden_intermediates_and_keep_missing_stats() {
    let (forward, _) = topology(&[("root", "done"), ("done", "no-stats")]);

    let rows = WatchGraphProjection::visible_rows(
        &agent_id("root"),
        &forward,
        |agent_id| agent_id.as_str() != "done",
        8,
    );
    assert_eq!(visible_ids(&rows), vec![("no-stats", 2, Some("done"))]);
}

/// Display order is stable by depth and then agent id, independent of edge
/// insertion order or lexicographic path-selection order.
#[test]
fn visible_rows_order_by_depth_then_agent_id() {
    let (forward, _) = topology(&[("root", "z"), ("root", "a"), ("z", "b"), ("a", "c")]);

    let rows = WatchGraphProjection::visible_rows(&agent_id("root"), &forward, |_| true, 8);
    assert_eq!(
        visible_ids(&rows),
        vec![
            ("a", 1, None),
            ("z", 1, None),
            ("b", 2, Some("z")),
            ("c", 2, Some("a")),
        ]
    );
}
