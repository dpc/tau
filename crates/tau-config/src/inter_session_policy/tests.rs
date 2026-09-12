use std::path::Path;

use super::InterSessionPolicy;

/// Ensures an absent allowlist remains unrestricted while deny matches veto
/// access after the allow decision.
#[test]
fn allow_then_deny_precedence_is_explicit() {
    let unrestricted = InterSessionPolicy::default();
    assert!(unrestricted.allows(Path::new("/srv/work/private")));

    let policy: InterSessionPolicy = serde_yaml_ng::from_str(
        r#"
allow_project_roots: [/srv/**]
deny_project_roots: [/srv/**/private]
"#,
    )
    .expect("valid inter-session policy");
    assert!(policy.allows(Path::new("/srv/work/public")));
    assert!(!policy.allows(Path::new("/srv/work/private")));
    assert!(!policy.allows(Path::new("/home/work/public")));
}

/// Ensures an explicitly empty allowlist denies every remote project rather
/// than behaving like an omitted unrestricted allowlist.
#[test]
fn explicit_empty_allowlist_denies_all() {
    let policy: InterSessionPolicy =
        serde_yaml_ng::from_str("allow_project_roots: []\n").expect("empty allowlist");
    assert!(!policy.allows(Path::new("/srv/work")));
}

/// Ensures project-root patterns are absolute, separator-aware path globs so
/// ambiguous relative or cross-directory single-star patterns fail closed.
#[test]
fn project_root_globs_are_absolute_and_separator_aware() {
    assert!(
        serde_yaml_ng::from_str::<InterSessionPolicy>("allow_project_roots: [relative/**]\n")
            .is_err()
    );
    let policy: InterSessionPolicy =
        serde_yaml_ng::from_str("allow_project_roots: [/srv/*]\n").expect("absolute glob");
    assert!(policy.allows(Path::new("/srv/work")));
    assert!(!policy.allows(Path::new("/srv/team/work")));
}
