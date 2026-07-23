use tau_proto::{NodeId, UiTreeNavigationTarget};

use super::parse_tree_navigation_target;

/// The shared `:tree` argument parser is used by both the interactive input
/// loop and `tau send`; numeric arguments are prompt anchors, while raw
/// node ids require the explicit expert `node` keyword.
#[test]
fn tree_navigation_parser_separates_anchors_root_and_raw_nodes() {
    assert_eq!(
        parse_tree_navigation_target("42"),
        Ok(UiTreeNavigationTarget::PromptAnchor(42))
    );
    assert_eq!(
        parse_tree_navigation_target("0"),
        Ok(UiTreeNavigationTarget::Root)
    );
    assert_eq!(
        parse_tree_navigation_target("root"),
        Ok(UiTreeNavigationTarget::Root)
    );
    assert_eq!(
        parse_tree_navigation_target("node 42"),
        Ok(UiTreeNavigationTarget::Node(NodeId::new(42)))
    );
    assert_eq!(parse_tree_navigation_target("nope"), Err(()));
}
