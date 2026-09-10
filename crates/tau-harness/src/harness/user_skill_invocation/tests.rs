use super::*;

/// Ensures the user skill command parser recognizes spaced and compact forms,
/// preserves opaque args, and does not capture unrelated commands.
#[test]
fn parses_user_skill_command_edges() {
    assert_eq!(
        parse_user_skill_command(":skill demo arg text"),
        Some(("demo", "arg text"))
    );
    assert_eq!(
        parse_user_skill_command("  :skill:demo arg text"),
        Some(("demo", "arg text"))
    );
    assert_eq!(parse_user_skill_command(":skill"), Some(("", "")));
    assert_eq!(parse_user_skill_command(":skill:"), Some(("", "")));
    assert_eq!(parse_user_skill_command("/skill demo"), None);
    assert_eq!(parse_user_skill_command("/skill:demo"), None);
    assert_eq!(parse_user_skill_command(":skillx demo"), None);
    assert_eq!(parse_user_skill_command("hello :skill demo"), None);
}

/// Ensures user `:skill` rejects the same too-long unclosed frontmatter case
/// as the model-visible `skill` tool instead of injecting YAML as body.
#[test]
fn rejects_frontmatter_truncated_before_closing_fence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("huge.md");
    let content = format!(
        "---\nname: huge\ndescription: {}",
        "x".repeat(MAX_USER_INVOKED_SKILL_BYTES)
    );
    std::fs::write(&path, content).expect("write skill");
    let source = DiscoveredSkillSource::File(path);

    let error = read_user_invoked_skill_body(&source).expect_err("frontmatter error");
    assert!(error.contains("frontmatter closing fence was not found"));
}

/// User `:skill` expansion must hide maintenance comments from model context
/// while leaving the source file untouched for editors and other consumers.
#[test]
fn filters_comments_without_modifying_user_skill_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("commented.md");
    let content =
        "---\nname: commented\ndescription: commented skill\n---\nvisible\n<!-- hidden -->\nend";
    std::fs::write(&path, content).expect("write skill");
    let source = DiscoveredSkillSource::File(path.clone());

    let loaded = read_user_invoked_skill_body(&source).expect("load skill body");

    assert_eq!(loaded.body, "visible\nend");
    assert_eq!(std::fs::read_to_string(path).expect("read source"), content);
}
