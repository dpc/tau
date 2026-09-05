use std::{fs as path_std_fs, time as path_std_time};

use super::*;

const TOOL_VERIFICATION_ROOT: &str = "tau-tool-verification";
const TEST_DIRECTORY_ENTRY_LIMIT: usize = 3;

#[cfg(unix)]
fn write_skill(path: &Path, name: &str) {
    fs::write(
        path,
        format!("---\nname: {name}\ndescription: Fixture skill\n---\n"),
    )
    .expect("write skill");
}

fn repository_tool_verification_skills() -> Vec<Skill> {
    let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".agents/skills");
    let result = load_skills_from_skill_dirs(&[SkillDir {
        path: skills_dir,
        add_to_prompt_by_default: true,
        source_precedence: None,
    }]);
    assert!(
        result.diagnostics.is_empty(),
        "repository skills must load cleanly: {:?}",
        result.diagnostics
    );
    result
        .skills
        .into_iter()
        .filter(|skill| {
            skill.name == TOOL_VERIFICATION_ROOT
                || skill
                    .name
                    .starts_with(&format!("{TOOL_VERIFICATION_ROOT}-"))
        })
        .collect()
}

// -- Frontmatter parsing ------------------------------------------------

#[test]
fn parse_frontmatter_basic() {
    let content = "---\nname: my-skill\ndescription: Does things\n---\n# Body\n";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("Does things")
    );
    assert_eq!(body, "# Body\n");
}

#[test]
fn parse_frontmatter_none_when_missing() {
    let content = "# No frontmatter\nJust body content.";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_unclosed() {
    let content = "---\nname: broken\nno closing fence";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, content);
}

#[test]
fn parse_frontmatter_bom() {
    let content = "\u{feff}---\nname: bom-skill\ndescription: Has BOM\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("bom-skill"));
    assert_eq!(body, "Body");
}

#[test]
fn load_skill_valid_defaults_to_not_advertised() {
    let content = "---\nname: my-skill\ndescription: Does useful things\n---\n# Instructions";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "my-skill");
    assert_eq!(skill.description, "Does useful things");
    assert!(
        !skill.add_to_prompt,
        "skills must opt into auto-advertising via `advertise: true`"
    );
    assert!(diags.is_empty());
}

/// Ensures the repository verification index alone is advertised while every
/// focused plan remains available to exact-name loading.
#[test]
fn repository_tool_verification_prompt_listing_only_advertises_root() {
    let skills = repository_tool_verification_skills();
    let advertised: Vec<&str> = skills
        .iter()
        .filter(|skill| skill.add_to_prompt)
        .map(|skill| skill.name.as_str())
        .collect();

    assert_eq!(advertised, [TOOL_VERIFICATION_ROOT]);
    assert!(
        skills
            .iter()
            .any(|skill| skill.name != TOOL_VERIFICATION_ROOT),
        "expected focused verification sub-skills"
    );
    for skill in skills
        .iter()
        .filter(|skill| skill.name != TOOL_VERIFICATION_ROOT)
    {
        assert!(
            !skill.disable_model_invocation,
            "{} must remain exact-query loadable",
            skill.name
        );
    }
}

/// The root is the deliberate discovery surface for focused plans. Requiring
/// an exact set prevents both stale references and silently unindexed skills.
#[test]
fn repository_tool_verification_root_references_every_focused_skill() {
    let skills = repository_tool_verification_skills();
    let root = skills
        .iter()
        .find(|skill| skill.name == TOOL_VERIFICATION_ROOT)
        .expect("verification root");
    let root_content = fs::read_to_string(&root.file_path).expect("load verification root");
    let focused_names: std::collections::BTreeSet<&str> = skills
        .iter()
        .map(|skill| skill.name.as_str())
        .filter(|name| *name != TOOL_VERIFICATION_ROOT)
        .collect();
    let referenced_names: std::collections::BTreeSet<&str> = root_content
        .split('`')
        .filter(|token| token.starts_with(&format!("{TOOL_VERIFICATION_ROOT}-")))
        .collect();

    assert_eq!(referenced_names, focused_names);
}

/// Ensures every YAML representation of a missing description is rejected by
/// Tau's loader with the stable skip diagnostic, rather than relying on YAML's
/// null and empty-scalar details.
#[test]
fn load_skill_missing_or_blank_descriptions_are_skipped() {
    let path = Path::new("/skills/no-description/SKILL.md");
    for (label, description) in [
        ("omitted", ""),
        ("yaml null", "description: null\n"),
        ("empty quoted", "description: \"\"\n"),
        ("whitespace", "description: \"   \"\n"),
    ] {
        let content = format!("---\nname: no-description\n{description}---\nBody");
        let (skill, diagnostics) = load_skill_from_content(&content, path);
        assert!(skill.is_none(), "{label} description must not load");
        assert_eq!(
            diagnostics,
            [SkillDiagnostic {
                path: path.to_owned(),
                kind: DiagnosticKind::Skipped,
                message: "description is required".to_owned(),
            }],
            "{label} description"
        );
    }
}

#[test]
fn load_skill_truncates_long_description() {
    let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 16);
    let content = format!("---\nname: long-desc\ndescription: {long}\n---\nBody");
    let path = Path::new("/skills/long-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(&content, path);
    let skill = skill.expect("should load");
    assert_eq!(
        skill.description,
        format!("{}…", "x".repeat(MAX_DESCRIPTION_LENGTH - "…".len()))
    );
    assert!(
        diags
            .iter()
            .any(|d| { d.kind == DiagnosticKind::Warning && d.message.contains("truncating") })
    );
}

/// Ensures accepted frontmatter and directory fallback names remain the visible
/// skill name after validation assigns their semantic type.
#[test]
fn load_skill_visible_name_uses_frontmatter_or_skill_directory_fallback() {
    for (content, path, expected_name) in [
        (
            "---\nname: frontmatter-name\ndescription: Explicit name\n---\n",
            Path::new("/skills/other-directory/SKILL.md"),
            "frontmatter-name",
        ),
        (
            "---\ndescription: Inferred name\n---\n",
            Path::new("/skills/inferred-name/SKILL.md"),
            "inferred-name",
        ),
    ] {
        let (skill, _diags) = load_skill_from_content(content, path);
        assert_eq!(skill.expect("should load").name.as_str(), expected_name);
    }
}

#[test]
fn load_skill_name_mismatch_warning() {
    let content = "---\nname: wrong-name\ndescription: Mismatch test\n---\n";
    let path = Path::new("/skills/actual-dir/SKILL.md");
    let (_skill, diags) = load_skill_from_content(content, path);
    assert!(diags.iter().any(|d| d.message.contains("does not match")));
}

/// Exercises every loader validation branch so malformed frontmatter cannot
/// become a discoverable skill while the maximum valid name remains accepted.
#[test]
fn load_skill_name_validation_cases_are_skipped_or_accepted() {
    let path = Path::new("/");
    let too_long = "a".repeat(MAX_NAME_LENGTH + 1);
    let boundary = "a".repeat(MAX_NAME_LENGTH);
    for (name, expected_skip) in [
        (
            "",
            Some("name is empty (no `name:` field and no usable parent directory name)"),
        ),
        (too_long.as_str(), Some("name exceeds 64 characters (65)")),
        (
            "Bad",
            Some("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"),
        ),
        (
            "bad_name",
            Some("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"),
        ),
        ("-bad", Some("name must not start or end with a hyphen")),
        ("bad-", Some("name must not start or end with a hyphen")),
        ("a--b", Some("name must not contain consecutive hyphens")),
        (boundary.as_str(), None),
    ] {
        let content = format!("---\nname: {name}\ndescription: Valid description\n---\n");
        let (skill, diagnostics) = load_skill_from_content(&content, path);
        assert_eq!(skill.is_none(), expected_skip.is_some(), "{name:?}");
        let skipped = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.kind == DiagnosticKind::Skipped);
        assert_eq!(
            skipped.map(|diagnostic| diagnostic.path.as_path()),
            expected_skip.map(|_| path)
        );
        assert_eq!(
            skipped.map(|diagnostic| diagnostic.message.as_str()),
            expected_skip,
            "{name:?}"
        );
    }
}

/// Keeps the public validation helpers aligned with all name-rule branches and
/// their first-error messages, independently from loader fallback behavior.
#[test]
fn skill_name_validation_helpers_cover_each_rule() {
    let too_long = "a".repeat(MAX_NAME_LENGTH + 1);
    let boundary = "a".repeat(MAX_NAME_LENGTH);
    for (name, expected) in [
        ("", Some("name is empty")),
        (too_long.as_str(), Some("name exceeds 64 characters (65)")),
        (
            "Bad",
            Some("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"),
        ),
        (
            "bad_name",
            Some("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"),
        ),
        ("-bad", Some("name must not start or end with a hyphen")),
        ("bad-", Some("name must not start or end with a hyphen")),
        ("a--b", Some("name must not contain consecutive hyphens")),
        (boundary.as_str(), None),
    ] {
        assert_eq!(skill_name_validation_message(name).as_deref(), expected);
        assert_eq!(is_valid_skill_name(name), expected.is_none());
    }
}

#[test]
fn load_skill_advertise_accepts_case_and_one() {
    for value in ["true", "True", "TRUE", "1"] {
        let content =
            format!("---\nname: shown\ndescription: visible\nadvertise: {value}\n---\nBody");
        let path = Path::new("/skills/shown/SKILL.md");
        let (skill, _diags) = load_skill_from_content(&content, path);
        let skill = skill.expect("should load");
        assert!(skill.add_to_prompt, "advertise: {value} should be truthy");
    }
}

#[test]
fn load_skill_advertise_rejects_other_truthy_words() {
    // `yes` / `on` are not accepted boolean spellings; the loader warns and
    // falls back to the default false value for single-file loading.
    // (documented behavior).
    let content = "---\nname: hidden\ndescription: visible\nadvertise: yes\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    assert!(!skill.expect("should load").add_to_prompt);
}

/// Keeps user invocation and model-visibility policy independent, including the
/// forced user invocation required by a model-disabled skill.
#[test]
fn load_skill_user_invocation_policy_combinations() {
    let path = Path::new("/skills/manual/SKILL.md");
    for (label, fields, explicit, disabled, invocable, warns) in [
        ("defaults", "", false, false, true, false),
        (
            "explicitly not user invocable",
            "user-invocable: false\n",
            true,
            false,
            false,
            false,
        ),
        (
            "model disabled",
            "disable-model-invocation: true\n",
            false,
            true,
            true,
            false,
        ),
        (
            "model disabled forces user invocation",
            "user-invocable: false\ndisable-model-invocation: true\n",
            true,
            true,
            true,
            true,
        ),
    ] {
        let content = format!("---\nname: manual\ndescription: Manual skill\n{fields}---\n");
        let (skill, diagnostics) = load_skill_from_content(&content, path);
        let skill = skill.expect(label);
        assert_eq!(skill.user_invocable_explicit, explicit, "{label}");
        assert_eq!(skill.disable_model_invocation, disabled, "{label}");
        assert_eq!(skill.user_invocable, invocable, "{label}");
        assert!(
            !skill.add_to_prompt,
            "model-disabled skills must not become advertised: {label}"
        );
        assert_eq!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.kind == DiagnosticKind::Warning
                    && diagnostic.message.contains("implies user-invocable")
            }),
            warns,
            "{label}"
        );
    }
}

#[test]
fn load_skill_invalid_bool_warns_and_uses_defaults() {
    let content = "---\nname: invalid-bool\ndescription: Invalid bool\nuser-invocable: maybe\ndisable-model-invocation: nope\n---\n";
    let path = Path::new("/skills/invalid-bool/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(skill.user_invocable);
    assert!(!skill.disable_model_invocation);
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.message.contains("invalid boolean"))
            .count(),
        2
    );
}

/// Ensures a byte-limited argument hint truncates at a character boundary and
/// retains both the warning category and its owning skill path.
#[test]
fn load_skill_truncates_multibyte_argument_hint() {
    let hint = "é".repeat(MAX_ARGUMENT_HINT_LENGTH);
    let content = format!("---\nname: hint\ndescription: Hint skill\nargument-hint: {hint}\n---\n");
    let path = Path::new("/skills/hint/SKILL.md");
    let (skill, diags) = load_skill_from_content(&content, path);
    let skill = skill.expect("should load");
    let hint = skill.argument_hint.expect("hint");
    assert_eq!(
        hint,
        format!(
            "{}…",
            "é".repeat((MAX_ARGUMENT_HINT_LENGTH - "…".len()) / "é".len())
        )
    );
    assert_eq!(
        diags,
        [SkillDiagnostic {
            path: path.to_owned(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "argument-hint exceeds {MAX_ARGUMENT_HINT_LENGTH} bytes ({}); truncating",
                "é".len() * MAX_ARGUMENT_HINT_LENGTH
            ),
        }]
    );
}

#[test]
fn parse_frontmatter_unescapes_double_quoted_strings() {
    // serde_yaml_ng (real YAML) handles escapes inside double-quoted
    // scalars; the previous handwritten parser kept the backslashes
    // literal. This pins the new behavior.
    let content = "---\nname: q\ndescription: \"a \\\"quoted\\\" thing\"\n---\n";
    let (fm, _) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some(r#"a "quoted" thing"#)
    );
}

#[test]
fn parse_frontmatter_multiline_block_scalar() {
    // Block scalars (`>`) fold newlines into a single string. The
    // contract is "stringified scalar", so this round-trips into the
    // map without losing content.
    let content = "---\nname: ml\ndescription: >\n  line one\n  line two\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("line one line two\n")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_ignores_indented_fence_in_block_scalar() {
    let content = "---\nname: ml\ndescription: |\n  before\n  ---\n  after\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("before\n---\nafter\n")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_drops_non_scalar_values() {
    // Lists / mappings / null don't fit the BTreeMap<String, String>
    // contract; the parser silently drops them.
    let content = "---\nname: x\ndescription: x\ntags:\n  - a\n  - b\nempty: null\n---\n";
    let (fm, _) = parse_frontmatter(content);
    assert!(fm.contains_key("name"));
    assert!(fm.contains_key("description"));
    assert!(!fm.contains_key("tags"), "lists are dropped");
    assert!(!fm.contains_key("empty"), "null values are dropped");
}

#[test]
fn parse_frontmatter_invalid_yaml_treats_as_no_frontmatter() {
    // Garbage inside the fence shouldn't panic; it should just yield
    // an empty map (and the body still flows through).
    let content = "---\nname: x\n  bad: indent : here\n  more\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert!(fm.is_empty());
    assert_eq!(body, "Body");
}

#[test]
fn load_skill_invalid_yaml_is_skipped_with_parse_diagnostic() {
    let content = "---\nname: x\n  bad: indent : here\n---\nBody";
    let path = Path::new("/skills/broken/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| {
        d.kind == DiagnosticKind::Skipped && d.message.contains("YAML failed to parse")
    }));
}

#[test]
fn parse_frontmatter_crlf_mixed_with_multibyte() {
    // Regression for the off-by-one in find_closing_fence with CRLF: any
    // byte-level offset slip would land inside a UTF-8 multibyte char and
    // panic on slice. With correct offsets it just returns the body.
    let content = "---\r\nname: mb\r\ndescription: café ☕\r\n---\r\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("description").map(String::as_str), Some("café ☕"));
    assert_eq!(body, "Body");
}

#[test]
fn root_md_without_name_uses_file_stem() {
    let content = "---\ndescription: A standalone skill\n---\n";
    let path = Path::new("/skills/standalone.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "standalone");
    assert!(
        diags
            .iter()
            .all(|d| !d.message.contains("does not match parent directory")),
        "standalone file should not be compared with parent dir: {diags:?}"
    );
}

#[test]
fn discover_skill_md_in_subdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("my-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Test\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("my-skill/SKILL.md"));
}

#[test]
fn discover_root_md_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("z-standalone.md"),
        "---\nname: z-standalone\ndescription: A standalone skill\n---\n",
    )
    .expect("write");
    fs::write(
        tmp.path().join("a-standalone.md"),
        "---\ndescription: A standalone skill\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("a-standalone.md"));
    assert!(paths[1].ends_with("z-standalone.md"));
}

#[test]
fn discover_skips_dot_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let hidden = tmp.path().join(".hidden");
    fs::create_dir_all(&hidden).expect("mkdir");
    fs::write(
        hidden.join("SKILL.md"),
        "---\nname: hidden\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_skips_node_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nm = tmp.path().join("node_modules").join("some-skill");
    fs::create_dir_all(&nm).expect("mkdir");
    fs::write(
        nm.join("SKILL.md"),
        "---\nname: some-skill\ndescription: Should be skipped\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert!(paths.is_empty());
}

#[test]
fn discover_does_not_recurse_past_skill_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let parent = tmp.path().join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).expect("mkdir");
    fs::write(
        parent.join("SKILL.md"),
        "---\nname: parent\ndescription: Parent skill\n---\n",
    )
    .expect("write");
    fs::write(
        child.join("SKILL.md"),
        "---\nname: child\ndescription: Should not be found\n---\n",
    )
    .expect("write");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("parent/SKILL.md"));
}

#[test]
fn discover_nonexistent_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("missing");
    let paths = discover_skill_paths(&missing);
    assert!(paths.is_empty());
}

// -- Multi-directory loading --------------------------------------------

fn set_skill_mtime(path: &Path, seconds_since_epoch: u64) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open skill file");
    let modified = std::time::UNIX_EPOCH + path_std_time::Duration::from_secs(seconds_since_epoch);
    file.set_times(path_std_fs::FileTimes::new().set_modified(modified))
        .expect("set modified time");
}

/// Ensures duplicate skills across configured roots select the newest file, so
/// root order does not hide a fresher local skill.
#[test]
fn load_from_dirs_collision_winner_is_newest_modified() {
    let dir1 = tempfile::tempdir().expect("tempdir");
    let dir2 = tempfile::tempdir().expect("tempdir");

    let s1 = dir1.path().join("my-skill");
    fs::create_dir_all(&s1).expect("mkdir");
    fs::write(
        s1.join("SKILL.md"),
        "---\nname: my-skill\ndescription: First\n---\n",
    )
    .expect("write");
    set_skill_mtime(&s1.join("SKILL.md"), 1_700_000_000);

    let s2 = dir2.path().join("my-skill");
    fs::create_dir_all(&s2).expect("mkdir");
    fs::write(
        s2.join("SKILL.md"),
        "---\nname: my-skill\ndescription: Second\n---\n",
    )
    .expect("write");
    set_skill_mtime(&s2.join("SKILL.md"), 1_700_000_100);

    let result = load_skills_from_dirs(&[dir1.path().to_owned(), dir2.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "Second");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision)
    );
}

/// Ensures callers can assign explicit root precedence for migrations, so a
/// preferred root can beat a newer legacy-root duplicate without changing the
/// default modified-time behavior for callers that omit precedence.
#[test]
fn load_from_skill_dirs_collision_prefers_explicit_root_precedence() {
    let preferred = tempfile::tempdir().expect("preferred tempdir");
    let legacy = tempfile::tempdir().expect("legacy tempdir");

    let preferred_skill = preferred.path().join("same-skill");
    fs::create_dir_all(&preferred_skill).expect("mkdir preferred");
    fs::write(
        preferred_skill.join("SKILL.md"),
        "---\nname: same-skill\ndescription: Preferred\n---\n",
    )
    .expect("write preferred");
    set_skill_mtime(&preferred_skill.join("SKILL.md"), 1_700_000_000);

    let legacy_skill = legacy.path().join("same-skill");
    fs::create_dir_all(&legacy_skill).expect("mkdir legacy");
    fs::write(
        legacy_skill.join("SKILL.md"),
        "---\nname: same-skill\ndescription: Legacy\n---\n",
    )
    .expect("write legacy");
    set_skill_mtime(&legacy_skill.join("SKILL.md"), 1_700_000_100);

    let result = load_skills_from_skill_dirs(&[
        SkillDir {
            path: preferred.path().to_owned(),
            add_to_prompt_by_default: false,
            source_precedence: Some(0),
        },
        SkillDir {
            path: legacy.path().to_owned(),
            add_to_prompt_by_default: false,
            source_precedence: Some(1),
        },
    ]);

    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "Preferred");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("higher-priority skill root"))
    );
}

/// Ensures duplicate skills discovered in one root use modified time rather
/// than path sorting as the winner heuristic.
#[test]
fn load_from_dir_collision_newest_beats_path_sort() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (dir, description) in [("a-skill", "First"), ("z-skill", "Second")] {
        let skill_dir = tmp.path().join(dir);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: same-name\ndescription: {description}\n---\n"),
        )
        .expect("write");
        let mtime = if description == "First" {
            1_700_000_000
        } else {
            1_700_000_100
        };
        set_skill_mtime(&skill_dir.join("SKILL.md"), mtime);
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].description, "Second");
}

/// Ensures typed duplicate keys keep the previous winner and lexical output
/// order, rather than inheriting traversal order from the filesystem.
#[test]
fn load_from_dirs_collision_winner_and_lexical_order_are_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (directory, name, description, modified) in [
        ("alpha", "alpha", "Alpha", 1_700_000_000),
        ("older-middle", "middle", "Older middle", 1_700_000_000),
        ("newer-middle", "middle", "Newer middle", 1_700_000_100),
        ("zulu", "zulu", "Zulu", 1_700_000_000),
    ] {
        let skill_dir = tmp.path().join(directory);
        fs::create_dir_all(&skill_dir).expect("mkdir");
        let path = skill_dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n"),
        )
        .expect("write skill");
        set_skill_mtime(&path, modified);
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "middle", "zulu"]
    );
    assert_eq!(result.skills[1].description, "Newer middle");
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.kind == DiagnosticKind::Collision)
            .count(),
        1
    );
}

#[test]
fn load_from_dirs_reads_only_bounded_discovery_metadata_for_large_body() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("large-body");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    let mut content = format!(
        "---\nname: large-body\ndescription: Large body\n---\n{}",
        "x".repeat(MAX_SKILL_DISCOVERY_BYTES)
    )
    .into_bytes();
    content.push(0xff);
    fs::write(skill_dir.join("SKILL.md"), content).expect("write");

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "large-body");
    assert!(result.diagnostics.is_empty());
}

/// Ensures a multibyte body character cut by the bounded discovery read cannot
/// corrupt already-closed UTF-8 frontmatter or make metadata loading panic.
#[test]
fn load_from_dirs_accepts_utf8_body_crossing_discovery_cut() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("utf8-cut");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    let frontmatter = "---\nname: utf8-cut\ndescription: café\n---\n";
    let body_prefix = MAX_SKILL_DISCOVERY_BYTES - frontmatter.len() - 1;
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("{frontmatter}{}éignored", "x".repeat(body_prefix)),
    )
    .expect("write");

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);

    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "utf8-cut");
    assert_eq!(result.skills[0].description, "café");
    assert!(result.diagnostics.is_empty());
}

#[test]
fn load_from_dirs_skips_oversized_unclosed_frontmatter() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let skill_dir = tmp.path().join("large-frontmatter");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: large-frontmatter\ndescription: {}\n",
            "x".repeat(MAX_SKILL_DISCOVERY_BYTES * 2)
        ),
    )
    .expect("write");

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Skipped
            && diagnostic.message.contains("discovery read limit")
    }));
}

const BUILT_IN_SKILL_NAMES: &[&str] = &[
    "tau-self-knowledge",
    "tau-self-knowledge-introduction",
    "tau-self-knowledge-architecture",
    "tau-self-knowledge-harness",
    "tau-self-knowledge-config",
    "tau-self-knowledge-secrets",
    "tau-self-knowledge-isolation",
    "tau-self-knowledge-cli-ui",
    "tau-self-knowledge-email",
    "tau-self-knowledge-ext-pim",
    "tau-self-knowledge-ext-rostra",
    "tau-self-knowledge-ext-provider-builtin",
    "tau-self-knowledge-ext-rhai",
    "tau-self-knowledge-ext-shell",
    "tau-self-knowledge-ext-slack",
    "tau-self-knowledge-ext-zulip",
    "tau-self-knowledge-ext-swarm",
    "tau-self-knowledge-ext-std-notifications",
    "tau-self-knowledge-ext-test-dummy",
    "tau-self-knowledge-ext-websearch",
    "tau-self-knowledge-prompt-templating",
    "tau-self-knowledge-source-code",
    "tau-self-knowledge-community",
    "tau-self-knowledge-debugging",
    "tau-self-knowledge-debugging-extensions",
    "tau-self-knowledge-tracing",
    "tau-self-knowledge-e2e-testing",
];

/// Ensures every embedded Markdown source satisfies the ordinary loader's
/// frontmatter contract before `built_in_skills` converts a skip into a panic.
#[test]
fn built_in_skill_sources_load_with_matching_frontmatter_names() {
    for source in BUILT_IN_SKILL_SOURCES {
        let content = render_built_in_skill_content(source.content);
        let path = Path::new(source.diagnostic_path);
        let (skill, diagnostics) = load_skill_from_content(&content, path);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.kind != DiagnosticKind::Skipped),
            "{}: {diagnostics:?}",
            source.diagnostic_path
        );
        let skill = skill.expect("embedded source must load");
        assert_eq!(
            parse_frontmatter(&content)
                .0
                .get("name")
                .map(String::as_str),
            Some(skill.name.as_str()),
            "{}",
            source.diagnostic_path
        );
    }
}

/// Keep the concise Zulip operational skill synchronized with the bounded
/// activity-summary capability and its process-local deadline boundary.
#[test]
fn zulip_self_knowledge_retains_activity_summary_contract_tokens() {
    let source = BUILT_IN_SKILL_SOURCES
        .iter()
        .find(|source| source.diagnostic_path == "tau-self-knowledge-ext-zulip.md")
        .expect("embedded Zulip self-knowledge");
    for token in [
        "non_allowlisted_activity",
        "message bodies",
        "process state",
        "deadline",
    ] {
        assert!(
            source.content.contains(token),
            "Zulip self-knowledge lost `{token}`"
        );
    }
}

/// Ensures the secrets skill keeps the practical distinction between a source
/// value, harness delivery authority, and extension-local secret selection.
#[test]
fn secrets_self_knowledge_retains_three_layer_delivery_contract() {
    let source = BUILT_IN_SKILL_SOURCES
        .iter()
        .find(|source| source.diagnostic_path == "tau-self-knowledge-secrets.md")
        .expect("embedded secrets self-knowledge");
    for token in [
        "<state_dir>/secrets/foo_api_key.yaml",
        "foo_api_key_secret: foo_api_key",
        "`Configure.secrets`",
        "owns the value",
        "authorizes Tau to deliver",
        "is defined by",
        "Config references do not grant delivery",
        "declarations do not prescribe",
        "undeclared names are not delivered",
        "prevents Tau from delivering every available secret",
    ] {
        assert!(
            source.content.contains(token),
            "secrets self-knowledge lost `{token}`"
        );
    }
}

/// Ensures the websearch skill retains its provider inventory, externally dated
/// operating facts, bounded failover semantics, and secret-safe configuration.
#[test]
fn websearch_self_knowledge_retains_practical_provider_overview() {
    let source = BUILT_IN_SKILL_SOURCES
        .iter()
        .find(|source| source.diagnostic_path == "tau-self-knowledge-ext-websearch.md")
        .expect("embedded websearch self-knowledge");
    let overview = source
        .content
        .split_once("## Practical provider overview\n")
        .and_then(|(_, following)| following.split_once("\nHarness-level `agents.web_tools`"))
        .map(|(overview, _)| overview.replace('\n', " "))
        .expect("websearch practical provider overview");
    for token in [
        "separate provider pools for `web_search` and `web_fetch`",
        "search defaults to Exa, Parallel, and anonymous You.com; fetch defaults to Exa and Parallel",
        "| Exa | ✓ | ✓ | Default, optional named secret |",
        "| Parallel | ✓ | ✓ | Default, optional named secret |",
        "| You.com | ✓ | — | Default, optional named secret |",
        "| Brave | ✓ | — | Optional named secret |",
        "| Tavily | ✓ | ✓ | Optional named secret |",
        "| Firecrawl | ✓ | ✓ | Optional named secret |",
        "verified September 5, 2026",
        "100 queries/day",
        "$5/1,000 calls",
        "1,000 credits/month",
        "$16/month billed yearly for 5,000 credits/month",
        "Firecrawl now offers [keyless access][firecrawl-keyless], but Tau's current REST adapter still sends bearer auth and requires a key",
        "round-robin cursors are independent",
        "sequentially in circular order",
        "first non-empty success",
        "at most three attempts",
        "one shared 45-second deadline",
        "may consume quota or incur cost",
        "not watched",
        "restart resets both cursors",
        "never put key bytes in ordinary config",
        "exa_api_key_secret: exa",
        "parallel_api_key_secret: parallel",
        "you_api_key_secret: you",
        "brave_api_key_secret: brave_search",
        "tavily_api_key_secret: tavily",
        "firecrawl_api_key_secret: firecrawl",
        "[exa-mcp]: https://exa.ai/",
        "[parallel-mcp]: https://docs.parallel.ai/",
        "[you-mcp]: https://you.com/",
        "[brave-plans]: https://api-dashboard.search.brave.com/",
        "[tavily-credits]: https://docs.tavily.com/",
        "[firecrawl-pricing]: https://www.firecrawl.dev/",
        "[firecrawl-keyless]: https://www.firecrawl.dev/",
    ] {
        assert!(
            overview.contains(token),
            "websearch self-knowledge lost `{token}`"
        );
    }
}

/// Pins the built-in loader's stable source order and complete name inventory.
#[test]
fn built_in_skills_have_expected_name_order_and_set() {
    let skills = built_in_skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, BUILT_IN_SKILL_NAMES);
    assert_eq!(
        names.into_iter().collect::<BTreeSet<_>>(),
        BUILT_IN_SKILL_NAMES.iter().copied().collect()
    );
}

/// Ensures only the overview is advertised while every focused built-in remains
/// available for user invocation and model-side loading.
#[test]
fn built_in_skills_keep_expected_visibility() {
    let skills = built_in_skills();
    assert_eq!(
        skills
            .iter()
            .filter(|skill| skill.add_to_prompt)
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["tau-self-knowledge"]
    );
    assert!(
        skills
            .iter()
            .all(|skill| { skill.user_invocable && !skill.disable_model_invocation })
    );
}

/// Ensures the advertised overview indexes every and only focused built-in by
/// exact name, preventing stale references and orphaned bundled sources.
#[test]
fn built_in_skill_root_indexes_exactly_the_focused_built_ins() {
    let skills = built_in_skills();
    let root = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge")
        .expect("built-in root");
    let focused_names: BTreeSet<&str> = skills
        .iter()
        .map(|skill| skill.name.as_str())
        .filter(|name| root.name != *name)
        .collect();
    let referenced_names: BTreeSet<&str> = root
        .content
        .split('`')
        .filter(|token| token.starts_with("tau-self-knowledge-"))
        .collect();
    assert_eq!(referenced_names, focused_names);
}

/// Ensures embedded content resolves the package version token before callers
/// receive it, rather than exposing an unreplaced build-time placeholder.
#[test]
fn built_in_skill_content_resolves_runtime_version() {
    let root = built_in_skills()
        .into_iter()
        .find(|skill| skill.name == "tau-self-knowledge")
        .expect("built-in root");
    assert!(!root.content.contains(SELF_KNOWLEDGE_VERSION_TOKEN));
    assert!(
        root.content
            .contains(&format!("Tau version `{TAU_VERSION}`"))
    );
}

/// Ensures a root-level prompt default applies only when a skill omits
/// `advertise:`, preserving explicit visibility choices.
#[test]
fn load_from_scoped_dirs_applies_prompt_default_when_advertise_is_omitted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for (name, advertise) in [
        ("defaulted", ""),
        ("explicit-hidden", "advertise: false\n"),
        ("explicit-shown", "advertise: true\n"),
    ] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: x\n{advertise}---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_skill_dirs(&[SkillDir {
        path: tmp.path().to_owned(),
        add_to_prompt_by_default: true,
        source_precedence: None,
    }]);
    let prompt_flag = |name: &str| {
        result
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.add_to_prompt)
    };

    assert_eq!(prompt_flag("defaulted"), Some(true));
    assert_eq!(prompt_flag("explicit-hidden"), Some(false));
    assert_eq!(prompt_flag("explicit-shown"), Some(true));
}

#[test]
fn load_from_dirs_is_sorted_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["zebra", "alpha", "mango", "bravo"] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: x\n---\n"),
        )
        .expect("write");
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    let names: Vec<&str> = result.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "bravo", "mango", "zebra"]);
}

/// Ensures symlinked skill directories are followed so users can share skill
/// collections through filesystem indirection.
#[test]
fn discover_follows_symlinked_dirs() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let real = outside.path().join("real-skill");
    let nested = real.join("nested");
    fs::create_dir_all(&nested).expect("mkdir");
    write_skill(&nested.join("SKILL.md"), "nested");

    let link = tmp.path().join("link");
    symlink(&real, &link).expect("symlink");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths, [link.join("nested/SKILL.md")]);

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| &skill.name)
            .collect::<Vec<_>>(),
        ["nested"]
    );
    assert!(result.diagnostics.is_empty());
}

/// Ensures direct symlinked Markdown files at a skill root are discovered and
/// loaded instead of producing a warning and being skipped.
#[test]
fn discover_follows_symlinked_files() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("external.md");
    write_skill(&target, "external");
    let link = tmp.path().join("external.md");
    symlink(&target, &link).expect("symlink");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths, [link]);

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| &skill.name)
            .collect::<Vec<_>>(),
        ["external"]
    );
    assert!(result.diagnostics.is_empty());
}

/// Ensures a directory-level `SKILL.md` may be a symlink and still causes that
/// directory to be treated as a single Pi-style skill.
#[test]
fn discover_follows_symlinked_skill_md() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("SKILL.md");
    write_skill(&target, "linked");
    let skill_dir = tmp.path().join("linked");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    symlink(&target, skill_dir.join("SKILL.md")).expect("symlink");
    fs::create_dir_all(skill_dir.join("nested")).expect("mkdir nested");
    write_skill(&skill_dir.join("nested").join("SKILL.md"), "nested");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths, [skill_dir.join("SKILL.md")]);

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| &skill.name)
            .collect::<Vec<_>>(),
        ["linked"]
    );
    assert!(result.diagnostics.is_empty());
}

/// Ensures a configured skill root may itself be a symlink, matching common
/// dotfile and shared-skills layouts.
#[test]
fn load_from_dirs_follows_symlinked_root() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let real = outside.path().join("skills");
    let skill_dir = real.join("outside-skill");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    write_skill(&skill_dir.join("SKILL.md"), "outside-skill");
    let link = tmp.path().join("skills");
    symlink(&real, &link).expect("symlink");

    let result = load_skills_from_dirs(&[link]);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| &skill.name)
            .collect::<Vec<_>>(),
        ["outside-skill"]
    );
    assert!(result.diagnostics.is_empty());
}

fn create_entry_budget_overflow_fixture(dir: &Path) {
    for index in 0..=TEST_DIRECTORY_ENTRY_LIMIT {
        fs::write(dir.join(format!("entry-{index:04}")), b"entry").expect("write entry");
    }
}

/// Ensures an overlarge direct skill root is skipped with a visible diagnostic
/// instead of allowing discovery to traverse an unbounded directory.
#[test]
fn load_from_dirs_diagnoses_overlarge_skill_root() {
    assert_eq!(MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR, 1_024);
    let tmp = tempfile::tempdir().expect("tempdir");
    create_entry_budget_overflow_fixture(tmp.path());

    let result = load_skills_from_dirs_with_test_limits(
        &[tmp.path().to_owned()],
        DiscoveryLimits {
            entries_per_dir: TEST_DIRECTORY_ENTRY_LIMIT,
            ..DEFAULT_DISCOVERY_LIMITS
        },
    );
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning
            && diagnostic.message.contains("entry budget exceeded")
    }));
}

/// Ensures an overlarge symlinked skill directory is skipped with a diagnostic,
/// preserving symlink support without allowing unbounded traversal expansion.
#[test]
fn load_from_dirs_diagnoses_overlarge_symlinked_directory() {
    use std::os::unix::fs::symlink;

    assert_eq!(MAX_SKILL_DISCOVERY_ENTRIES_PER_DIR, 1_024);
    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    create_entry_budget_overflow_fixture(outside.path());
    symlink(outside.path(), tmp.path().join("link")).expect("symlink");

    let result = load_skills_from_dirs_with_test_limits(
        &[tmp.path().to_owned()],
        DiscoveryLimits {
            entries_per_dir: TEST_DIRECTORY_ENTRY_LIMIT,
            ..DEFAULT_DISCOVERY_LIMITS
        },
    );
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning
            && diagnostic.path.ends_with("link")
            && diagnostic.message.contains("entry budget exceeded")
    }));
}

/// Ensures a per-root directory budget stops traversal at the first unvisited
/// sibling, so later directories cannot bypass the aggregate limit.
#[test]
fn discovery_directory_budget_stops_the_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["a-first", "b-over-budget", "c-not-visited"] {
        let dir = tmp.path().join(name);
        fs::create_dir(&dir).expect("mkdir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Fixture\n---\n"),
        )
        .expect("write skill");
    }
    let limits = DiscoveryLimits {
        dirs_per_root: 2,
        ..DEFAULT_DISCOVERY_LIMITS
    };

    let (paths, diagnostics) = discover_skill_paths_with_test_limits(tmp.path(), limits);

    assert_eq!(paths, [tmp.path().join("a-first/SKILL.md")]);
    assert_eq!(
        diagnostics,
        [SkillDiagnostic {
            path: tmp.path().join("b-over-budget"),
            kind: DiagnosticKind::Warning,
            message: "stopping skill discovery: directory budget exceeded (max 2 per root)"
                .to_owned(),
        }]
    );
}

/// Ensures a per-root entry budget stops before traversing any accepted child,
/// unlike a per-directory overflow that only skips the offending subtree.
#[test]
fn discovery_entry_budget_stops_root_but_directory_overflow_allows_siblings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let over_budget = tmp.path().join("a-over-budget");
    let accepted = tmp.path().join("b-accepted");
    fs::create_dir(&over_budget).expect("mkdir over-budget");
    fs::create_dir(&accepted).expect("mkdir accepted");
    create_entry_budget_overflow_fixture(&over_budget);
    fs::write(
        accepted.join("SKILL.md"),
        "---\nname: b-accepted\ndescription: Accepted sibling\n---\n",
    )
    .expect("write accepted");

    let (paths, diagnostics) = discover_skill_paths_with_test_limits(
        tmp.path(),
        DiscoveryLimits {
            entries_per_dir: TEST_DIRECTORY_ENTRY_LIMIT,
            ..DEFAULT_DISCOVERY_LIMITS
        },
    );
    assert_eq!(paths, [accepted.join("SKILL.md")]);
    assert_eq!(
        diagnostics,
        [SkillDiagnostic {
            path: over_budget.clone(),
            kind: DiagnosticKind::Warning,
            message: format!(
                "skipping skill directory: entry budget exceeded (max {TEST_DIRECTORY_ENTRY_LIMIT} per directory)"
            ),
        }]
    );

    let (paths, diagnostics) = discover_skill_paths_with_test_limits(
        tmp.path(),
        DiscoveryLimits {
            entries_per_root: 1,
            ..DEFAULT_DISCOVERY_LIMITS
        },
    );
    assert!(paths.is_empty());
    assert_eq!(
        diagnostics,
        [SkillDiagnostic {
            path: tmp.path().to_owned(),
            kind: DiagnosticKind::Warning,
            message: "stopping skill discovery: entry budget exceeded (max 1 per root)".to_owned(),
        }]
    );
}

/// Ensures deeply nested skill trees are bounded so accidental or malicious
/// directory chains cannot make startup traversal arbitrarily deep.
#[test]
fn load_from_dirs_diagnoses_too_deep_skill_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut dir = tmp.path().to_owned();
    for index in 0..=MAX_SKILL_DISCOVERY_DEPTH {
        dir = dir.join(format!("level-{index}"));
        fs::create_dir(&dir).expect("mkdir level");
    }

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning
            && diagnostic.message.contains("depth budget exceeded")
    }));
}

/// Ensures following symlinked directories still cannot recurse forever when a
/// symlink points back to an already visited canonical directory.
#[test]
fn discover_symlink_cycles_do_not_recurse_forever() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    let cycle_parent = real.join("cycle-parent");
    let skill_dir = real.join("skill-dir");
    fs::create_dir_all(&cycle_parent).expect("mkdir cycle parent");
    fs::create_dir_all(&skill_dir).expect("mkdir skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: skill-dir\ndescription: nested skill\n---\n",
    )
    .expect("write");
    symlink(&real, cycle_parent.join("cycle")).expect("symlink");

    let paths = discover_skill_paths(&real);
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("skill-dir/SKILL.md"));
}

#[test]
fn truncate_description_is_utf8_safe_and_marks_truncation() {
    let description = "é".repeat(MAX_DESCRIPTION_LENGTH);
    let truncated = truncate_description(&description);
    assert!(truncated.len() <= MAX_DESCRIPTION_LENGTH);
    assert!(truncated.is_char_boundary(truncated.len()));
    assert!(truncated.ends_with('…'));
}

// -- strip_frontmatter --------------------------------------------------

#[test]
fn strip_frontmatter_returns_body() {
    let content = "---\nname: x\n---\nThe body.";
    assert_eq!(strip_frontmatter(content), "The body.");
}

#[test]
fn has_unclosed_frontmatter_detects_missing_closing_fence() {
    assert!(has_unclosed_frontmatter("---\nname: x\n"));
    assert!(has_unclosed_frontmatter("\u{feff}---\r\nname: x\r\n"));
    assert!(!has_unclosed_frontmatter("---\nname: x\n---\nBody"));
    assert!(!has_unclosed_frontmatter("--- not a fence\n"));
    assert!(!has_unclosed_frontmatter("Body only"));
}
