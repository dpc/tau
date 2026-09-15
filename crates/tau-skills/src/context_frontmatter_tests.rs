//! Focused fail-open and role selection oracles shared by skill and AGENTS
//! users.

use std::path::Path;

use super::*;

/// Absence, explicit emptiness, union, and exclusion order are distinct policy
/// states; preserving them prevents an allowlist from overriding exclusions.
#[test]
fn role_policy_matching_table() {
    for (header, role, group, visible) in [
        ("", "engineer-senior", "engineer", true),
        ("only-roles: []", "engineer-senior", "engineer", false),
        ("only-role-groups: []", "engineer-senior", "engineer", false),
        (
            "only-roles: [engineer-senior]",
            "engineer-senior",
            "engineer",
            true,
        ),
        (
            "only-roles: [Engineer-senior]",
            "engineer-senior",
            "engineer",
            false,
        ),
        (
            "only-roles: [unknown]",
            "engineer-senior",
            "engineer",
            false,
        ),
        (
            "except-roles: [unknown]",
            "engineer-senior",
            "engineer",
            true,
        ),
        (
            "only-roles: []\nonly-role-groups: [engineer]",
            "engineer-senior",
            "engineer",
            true,
        ),
        (
            "only-roles: [researcher]\nonly-role-groups: [engineer]",
            "researcher",
            "research",
            true,
        ),
        (
            "only-roles: [researcher]\nonly-role-groups: [engineer]",
            "engineer-junior",
            "engineer",
            true,
        ),
        (
            "only-role-groups: [engineer]\nexcept-roles: [engineer-junior]",
            "engineer-junior",
            "engineer",
            false,
        ),
        (
            "only-roles: [engineer-senior]\nexcept-role-groups: [engineer]",
            "engineer-senior",
            "engineer",
            false,
        ),
        (
            "only-role-groups: [engineer]",
            "engineer-senior",
            "custom",
            false,
        ),
        ("only-role-groups: [custom]", "custom", "custom", true),
    ] {
        let source = format!("---\n{header}\n---\nBody");
        let parsed = parse_context_frontmatter(&source);
        // An empty YAML document is not a mapping; it recovers unrestricted.
        assert_eq!(parsed.visibility.allows(role, group), visible, "{header}");
    }
}

/// A malformed recognized list must disable the entire policy, not leave a
/// valid exclusion active, while preserving unrelated metadata and body.
#[test]
fn malformed_filter_fails_open_without_losing_other_metadata() {
    for value in [
        "null",
        "engineer",
        "{}",
        "[true]",
        "[1]",
        "[\"\"]",
        "[\" engineer\"]",
        "[\"engineer \"]",
    ] {
        let source = format!(
            "---\nname: example\ndescription: Useful\nadvertise: false\nexcept-roles: [engineer]\nonly-role-groups: {value}\n---\nBody"
        );
        let parsed = parse_context_frontmatter(&source);
        assert!(parsed.visibility.allows("engineer", "engineer"), "{value}");
        assert_eq!(parsed.fields["name"], "example");
        assert_eq!(parsed.fields["advertise"], "false");
        assert_eq!(parsed.body, "Body");
        assert!(
            parsed
                .warning
                .expect("mandatory warning")
                .contains("only-role-groups")
        );
        let (skill, diagnostics) =
            load_skill_from_content(&source, Path::new("/skills/example/SKILL.md"));
        let skill = skill.expect("malformed policy remains discoverable");
        assert!(!skill.add_to_prompt);
        assert!(skill.visibility.allows("engineer", "engineer"));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::Frontmatter)
        );
    }
}

/// Unparseable and unfinished headers must not remove useful source bytes from
/// either context-file parsing or on-demand skill preparation.
#[test]
fn malformed_headers_preserve_raw_instructions() {
    for source in [
        "---\nname: [broken\n---\nBody",
        "---\n- not-a-mapping\n---\nBody",
        "---\nname: unfinished\nUseful instructions",
        "\u{feff}---\r\nname: [broken\r\n---\r\nBody",
    ] {
        let parsed = parse_context_frontmatter(source);
        assert_eq!(parsed.body, source);
        assert!(parsed.warning.is_some());
        assert!(parsed.visibility.allows("any", "group"));
        let prepared = read_skill_text_prefix(source, source.len() + 1)
            .prepare()
            .expect("bounded complete malformed file");
        assert_eq!(prepared.body, source);
    }
}

/// Typed policy parsing must accept real YAML lists/anchors and existing BOM,
/// CRLF, and unrelated-key conventions without changing scalar metadata.
#[test]
fn valid_yaml_policy_strips_header_and_ignores_unknown_keys() {
    let source = "\u{feff}---\r\nname: example\r\ndescription: Useful\r\nonly-roles: &roles\r\n  - engineer\r\nexcept-roles: *roles\r\nunknown: [ignored]\r\n---\r\nBody";
    let parsed = parse_context_frontmatter(source);
    assert!(parsed.warning.is_none());
    assert_eq!(parsed.body, "Body");
    assert_eq!(parsed.fields["name"], "example");
    assert!(!parsed.fields.contains_key("unknown"));
    assert!(!parsed.visibility.allows("engineer", "engineer"));
}

/// A bad declared identity can recover through the already-established path
/// identity, without synthesizing a new alias or losing valid unrelated flags.
#[test]
fn malformed_skill_identity_recovers_to_existing_path_name() {
    let source = "---\nname: Bad_Name\ndescription: Useful\nuser-invocable: false\nonly-roles: []\n---\nBody";
    let (skill, diagnostics) =
        load_skill_from_content(source, Path::new("/skills/example/SKILL.md"));
    let skill = skill.expect("path-derived identity");
    assert_eq!(skill.name.as_str(), "example");
    assert!(!skill.user_invocable);
    assert!(skill.visibility.allows("any", "group"));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::Frontmatter
                && diagnostic.message.contains("path-derived"))
    );
}

/// Lists in legacy scalar fields must not silently disappear and leave an
/// exclusion policy active; valid unrelated metadata still survives recovery.
#[test]
fn malformed_scalar_metadata_fails_open() {
    for field in ["name: [bad]", "advertise: [true]", "user-invocable: maybe"] {
        let source =
            format!("---\n{field}\ndescription: Useful\nexcept-roles: [engineer]\n---\nBody");
        let (skill, diagnostics) =
            load_skill_from_content(&source, Path::new("/skills/example/SKILL.md"));
        let skill = skill.expect("recover scalar metadata");
        assert_eq!(skill.description, "Useful");
        assert!(skill.visibility.allows("engineer", "engineer"));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.kind == DiagnosticKind::Frontmatter)
        );
    }
}

/// Essential identity cannot be invented; rejecting an unidentifiable skill
/// still requires a mandatory file-specific warning, unlike minor filter
/// faults.
#[test]
fn unidentifiable_skill_is_rejected_with_frontmatter_warning() {
    for source in [
        "---\ninvalid: [\nuseful body",
        "---\ndescription: useful\n---\nbody",
    ] {
        let path = Path::new("/skills/Bad_Name/SKILL.md");
        let (skill, diagnostics) = load_skill_from_content(source, path);
        assert!(skill.is_none());
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::Frontmatter
                && diagnostic.path == path
                && diagnostic
                    .message
                    .contains("cannot recover a valid skill name")
        }));
    }
}

/// On-demand preparation must report malformed skill metadata even when its
/// YAML and role lists are valid, without suppressing the live instructions.
#[test]
fn live_skill_preparation_reports_scalar_and_required_metadata_faults() {
    for header in [
        "name: [bad]\ndescription: useful",
        "name: Bad_Name\ndescription: useful",
        "name: valid\nuser-invocable: maybe\ndescription: useful",
        "name: valid",
        "name: valid\ndescription: ''",
    ] {
        let source = format!("---\n{header}\n---\nLIVE BODY");
        let prepared = read_skill_text_prefix(&source, source.len())
            .prepare()
            .expect("recovery");
        assert_eq!(prepared.model_body, "LIVE BODY");
        assert!(prepared.frontmatter_warning.is_some(), "{header}");
    }
}
