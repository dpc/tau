use std::collections::BTreeSet;

use tau_config::settings::CliState;

/// Every advertised setting candidate must be usable and explain its effect so
/// completion never exposes an invalid or opaque registry entry.
#[test]
fn settings_registry_advertises_valid_described_unique_candidates() {
    let mut names = BTreeSet::new();
    for definition in super::SETTINGS {
        assert!(
            !definition.name.is_empty() && names.insert(definition.name),
            "setting names must be nonempty and unique: {}",
            definition.name
        );
        assert!(
            !definition.description.trim().is_empty(),
            "{} needs a user-facing description",
            definition.name
        );
        assert!(
            !definition.value_hint.trim().is_empty(),
            "{} needs a user-facing value hint",
            definition.name
        );
        let found = super::find(definition.name)
            .unwrap_or_else(|| panic!("find must return {}", definition.name));
        assert_eq!(found.description, definition.description);
        assert_eq!(found.value_hint, definition.value_hint);
        assert_eq!(
            found
                .values
                .iter()
                .map(|value| (value.value, value.description))
                .collect::<Vec<_>>(),
            definition
                .values
                .iter()
                .map(|value| (value.value, value.description))
                .collect::<Vec<_>>(),
            "find must return the registry definition for {}",
            definition.name
        );
        let state = CliState::default();
        assert_eq!(
            (found.get)(&state),
            (definition.get)(&state),
            "find must return the getter for {}",
            definition.name
        );

        let mut values = BTreeSet::new();
        for value in definition.values {
            assert!(
                values.insert(value.value),
                "{} advertises duplicate value {}",
                definition.name,
                value.value
            );
            assert!(
                !value.description.trim().is_empty()
                    && !matches!(
                        value.description.trim().to_ascii_lowercase().as_str(),
                        "item" | "value" | "setting" | "command"
                    ),
                "{}={} needs a specific completion description",
                definition.name,
                value.value
            );
            assert!(
                (definition.validate)(value.value),
                "{} advertises invalid value {}",
                definition.name,
                value.value
            );
            assert_eq!(
                (found.validate)(value.value),
                (definition.validate)(value.value),
                "find must return the validator for {}",
                definition.name
            );
        }
    }
}

/// Stable setting vocabularies and free-form numeric boundaries remain explicit
/// because scripts and documented command examples rely on these spellings.
#[test]
fn settings_registry_preserves_public_vocabularies_and_numeric_boundaries() {
    for (name, expected) in [
        (
            "show-messages",
            &[
                "none",
                "self-summary",
                "self-full",
                "all-summary",
                "all-full",
            ][..],
        ),
        ("show-internal-prompts", &["on", "off"][..]),
        (
            "notice-level",
            &["critical", "warning", "info", "debug", "trace"][..],
        ),
    ] {
        let definition = super::find(name).expect("documented setting");
        assert_eq!(
            definition
                .values
                .iter()
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            expected,
            "{name} vocabulary"
        );
    }

    let redraw_history_size =
        super::find("redraw-history-size").expect("redraw-history-size setting");
    for value in ["0", "12345"] {
        assert!((redraw_history_size.validate)(value), "{value} is accepted");
    }
    for value in ["all", "-1"] {
        assert!(
            !(redraw_history_size.validate)(value),
            "{value} is rejected"
        );
    }
}
