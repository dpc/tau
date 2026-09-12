use std::path::Path;

use super::InterSessionPolicy;

/// Notice text is optional, preserves an explicitly configured empty value,
/// and rejects either direction above the approved 64 KiB bound.
#[test]
fn notices_preserve_optional_text_and_enforce_byte_limit() {
    let omitted = serde_yaml_ng::from_str::<InterSessionPolicy>("{}").expect("omitted notices");
    assert_eq!(omitted.outgoing_notice, None);
    assert_eq!(omitted.incoming_notice, None);

    let configured = serde_yaml_ng::from_str::<InterSessionPolicy>(
        "outgoing_notice: ''\nincoming_notice: recipient guidance\n",
    )
    .expect("configured notices");
    assert_eq!(configured.outgoing_notice.as_deref(), Some(""));
    assert_eq!(
        configured.incoming_notice.as_deref(),
        Some("recipient guidance")
    );

    let oversized = "x".repeat(tau_proto::INTER_SESSION_NOTICE_MAX_BYTES + 1);
    for field in ["outgoing_notice", "incoming_notice"] {
        let error =
            serde_yaml_ng::from_str::<InterSessionPolicy>(&format!("{field}: '{oversized}'\n"))
                .expect_err("oversized notice must fail");
        assert!(error.to_string().contains("64 KiB"), "{error}");
    }
}

/// Receiver auto-start defaults on while an omitted receiver remains disabled.
#[test]
fn receiver_is_optional_and_defaults_auto_start_on() {
    let omitted = serde_yaml_ng::from_str::<InterSessionPolicy>("{}").expect("omitted receiver");
    assert_eq!(omitted.receiver, None);

    let configured =
        serde_yaml_ng::from_str::<InterSessionPolicy>("receiver:\n  role: coordinator\n")
            .expect("configured receiver");
    let receiver = configured.receiver.expect("receiver");
    assert_eq!(receiver.role, "coordinator");
    assert!(receiver.auto_start);
}

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
