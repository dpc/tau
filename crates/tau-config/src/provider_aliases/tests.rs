use super::*;

/// Parses the same scalar/list wire shape used by layered settings.
fn aliases(yaml: &str) -> BTreeMap<ProviderName, ProviderAliasTargets> {
    serde_yaml_ng::from_str(yaml).expect("valid alias fixture")
}

/// Known zero still beats unknown; duplicates and ties do not weight accounts.
#[test]
fn known_first_and_random_ties_are_deterministic() {
    let aliases = aliases("codex: [a, b, b, c]");
    for draw in 0..3 {
        let chosen = select(
            &aliases,
            |_| None,
            |len| {
                assert_eq!(len, 3);
                draw
            },
        )
        .expect("valid unknown aliases");
        assert_eq!(
            chosen[&ProviderName::new("codex")].as_str(),
            ["a", "b", "c"][draw]
        );
    }
    let chosen = select(
        &aliases,
        |name| (name.as_str() == "b").then_some(0),
        |len| {
            assert_eq!(len, 1);
            0
        },
    )
    .expect("valid known aliases");
    assert_eq!(chosen[&ProviderName::new("codex")].as_str(), "b");
    for draw in 0..2 {
        let chosen = select(
            &aliases,
            |name| Some(if name.as_str() == "a" { 10 } else { 20 }),
            |len| {
                assert_eq!(len, 2);
                draw
            },
        )
        .expect("valid tied aliases");
        assert_eq!(
            chosen[&ProviderName::new("codex")].as_str(),
            ["b", "c"][draw]
        );
    }
}

/// Unselected cycles and empty lists must fail before any random selection.
#[test]
fn entire_graph_is_validated_before_selection() {
    for yaml in [
        "unused: []",
        "codex: [a, b]\nb: c\nc: b",
        "unused: [unused, b]\nb: unused",
    ] {
        assert!(select(&aliases(yaml), |_| None, |_| panic!("invalid graph drew")).is_err());
    }
}

/// Chains share their child's choice, and convergent branches are not weights.
#[test]
fn chain_choices_are_memoized_and_concrete() {
    let aliases = aliases("a: [a, b]\nb: b\nc: [a, a]\nd: a");
    let chosen = select(&aliases, |_| None, |len| len - 1).expect("valid alias chains");
    for name in ["a", "b", "c", "d"] {
        assert_eq!(chosen[&ProviderName::new(name)].as_str(), "b");
    }
}
