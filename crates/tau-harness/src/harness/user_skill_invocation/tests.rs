use std::borrow::Cow;

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

/// Ensures file-backed skill prompts escape source labels while preserving the
/// display path and opaque multiline arguments byte-for-byte outside the block.
#[test]
fn formats_file_skill_prompt_with_escaped_location_and_opaque_arguments() {
    let source = DiscoveredSkillSource::File("/tmp/skill & \"quoted\" directory/example.md".into());

    let prompt = format_user_invoked_skill_prompt(
        "example",
        &source,
        "Read the local instructions.",
        None,
        "first argument line\nsecond argument line  \n",
    );

    assert_eq!(
        prompt,
        concat!(
            "<skill name=\"example\" location=\"/tmp/skill &amp; &quot;quoted&quot; directory/example.md\">\n",
            "References are relative to /tmp/skill & \"quoted\" directory.\n",
            "\n",
            "Read the local instructions.\n",
            "</skill>\n",
            "\n",
            "first argument line\n",
            "second argument line  \n",
        )
    );
}

/// Ensures built-in skill prompts place their truncation note inside the block
/// and add no argument separator when the user supplies no arguments.
#[test]
fn formats_truncated_builtin_skill_prompt_without_arguments() {
    let source = DiscoveredSkillSource::BuiltIn {
        content: Cow::Borrowed("built-in source"),
    };

    let prompt = format_user_invoked_skill_prompt(
        "builtin",
        &source,
        "Use the built-in instructions.",
        Some(70_000),
        "",
    );

    assert_eq!(
        prompt,
        concat!(
            "<skill name=\"builtin\" location=\"built-in skill\">\n",
            "References are relative to <builtin>.\n",
            "\n",
            "Use the built-in instructions.\n",
            "\n",
            "[skill content truncated at 65536 bytes; file has 70000 bytes]\n",
            "</skill>"
        )
    );
}
