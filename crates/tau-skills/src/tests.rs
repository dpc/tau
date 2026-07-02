use super::*;

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
fn parse_frontmatter_quoted_values() {
    let content = "---\nname: \"my-skill\"\ndescription: 'A quoted description'\n---\nBody";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
    assert_eq!(
        fm.get("description").map(String::as_str),
        Some("A quoted description")
    );
    assert_eq!(body, "Body");
}

#[test]
fn parse_frontmatter_boolean_field() {
    let content = "---\nname: shown\ndescription: An advertised skill\nadvertise: true\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("advertise").map(String::as_str), Some("true"));
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
fn parse_frontmatter_comments_and_blanks() {
    let content = "---\n# comment\n\nname: foo\ndescription: bar\n---\n";
    let (fm, _body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("foo"));
    assert_eq!(fm.get("description").map(String::as_str), Some("bar"));
}

// -- Skill loading from content -----------------------------------------

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

#[test]
fn load_skill_advertise_true_opts_into_prompt() {
    let content = "---\nname: shown\ndescription: visible\nadvertise: true\n---\nBody";
    let path = Path::new("/skills/shown/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(skill.add_to_prompt);
}

#[test]
fn load_skill_missing_description() {
    let content = "---\nname: no-desc\n---\n# Body";
    let path = Path::new("/skills/no-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_empty_description() {
    let content = "---\nname: empty\ndescription:\n---\nBody";
    let path = Path::new("/skills/empty/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(diags.iter().any(|d| d.kind == DiagnosticKind::Skipped));
}

#[test]
fn load_skill_truncates_long_description() {
    let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 16);
    let content = format!("---\nname: long-desc\ndescription: {long}\n---\nBody");
    let path = Path::new("/skills/long-desc/SKILL.md");
    let (skill, diags) = load_skill_from_content(&content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.description.len(), MAX_DESCRIPTION_LENGTH);
    assert!(skill.description.ends_with('…'));
    assert!(
        diags
            .iter()
            .any(|d| { d.kind == DiagnosticKind::Warning && d.message.contains("truncating") })
    );
}

#[test]
fn load_skill_advertise_false_or_missing_keeps_default() {
    let content = "---\nname: hidden\ndescription: A hidden skill\nadvertise: false\n---\n";
    let path = Path::new("/skills/hidden/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(!skill.add_to_prompt);
}

#[test]
fn load_skill_name_fallback_to_parent_dir() {
    let content = "---\ndescription: Inferred name\n---\n";
    let path = Path::new("/skills/inferred-name/SKILL.md");
    let (skill, _diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.name, "inferred-name");
}

#[test]
fn load_skill_name_mismatch_warning() {
    let content = "---\nname: wrong-name\ndescription: Mismatch test\n---\n";
    let path = Path::new("/skills/actual-dir/SKILL.md");
    let (_skill, diags) = load_skill_from_content(content, path);
    assert!(diags.iter().any(|d| d.message.contains("does not match")));
}

#[test]
fn load_skill_invalid_name_chars() {
    let content = "---\nname: Bad_Name\ndescription: Invalid chars\n---\n";
    let path = Path::new("/skills/Bad_Name/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "invalid name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("invalid characters"))
    );
}

#[test]
fn load_skill_hyphen_edges_are_skipped() {
    let content = "---\nname: -bad-\ndescription: bad\n---\n";
    let path = Path::new("/skills/-bad-/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("hyphen"))
    );
}

#[test]
fn load_skill_consecutive_hyphens_are_skipped() {
    let content = "---\nname: a--b\ndescription: bad\n---\n";
    let path = Path::new("/skills/a--b/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none());
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("consecutive"))
    );
}

#[test]
fn load_skill_empty_name_is_skipped() {
    // No `name:` field and a parent file_name that won't yield one either
    // (root path has no `file_name()`).
    let content = "---\ndescription: nameless\n---\n";
    let path = Path::new("/");
    let (skill, diags) = load_skill_from_content(content, path);
    assert!(skill.is_none(), "empty name should skip the skill");
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Skipped && d.message.contains("name is empty"))
    );
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

#[test]
fn load_skill_user_invocation_metadata_defaults_and_explicit_values() {
    let content = "---\nname: manual\ndescription: Manual skill\nuser-invocable: false\ndisable-model-invocation: true\nargument-hint: '[topic]'\n---\n";
    let path = Path::new("/skills/manual/SKILL.md");
    let (skill, diags) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert!(skill.user_invocable);
    assert!(skill.user_invocable_explicit);
    assert!(skill.disable_model_invocation);
    assert_eq!(skill.argument_hint.as_deref(), Some("[topic]"));
    assert!(
        diags
            .iter()
            .any(|d| d.kind == DiagnosticKind::Warning
                && d.message.contains("implies user-invocable"))
    );

    let defaults = "---\nname: defaults\ndescription: Default skill\n---\n";
    let (skill, diags) = load_skill_from_content(defaults, Path::new("/skills/defaults/SKILL.md"));
    let skill = skill.expect("should load");
    assert!(skill.user_invocable);
    assert!(!skill.user_invocable_explicit);
    assert!(!skill.disable_model_invocation);
    assert!(skill.argument_hint.is_none());
    assert!(diags.is_empty());
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

#[test]
fn load_skill_truncates_argument_hint() {
    let hint = "x".repeat(MAX_ARGUMENT_HINT_LENGTH + 10);
    let content = format!("---\nname: hint\ndescription: Hint skill\nargument-hint: {hint}\n---\n");
    let path = Path::new("/skills/hint/SKILL.md");
    let (skill, diags) = load_skill_from_content(&content, path);
    let skill = skill.expect("should load");
    assert_eq!(
        skill.argument_hint.expect("hint").len(),
        MAX_ARGUMENT_HINT_LENGTH
    );
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("argument-hint") && d.message.contains("truncating"))
    );
}

#[test]
fn parse_frontmatter_crlf() {
    let content = "---\r\nname: crlf\r\ndescription: Has CRLF\r\n---\r\nBody line";
    let (fm, body) = parse_frontmatter(content);
    assert_eq!(fm.get("name").map(String::as_str), Some("crlf"));
    assert_eq!(fm.get("description").map(String::as_str), Some("Has CRLF"));
    assert_eq!(body, "Body line");
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
fn skill_base_dir_matches_parent() {
    let content = "---\nname: my-skill\ndescription: x\n---\n";
    let path = Path::new("/skills/my-skill/SKILL.md");
    let (skill, _) = load_skill_from_content(content, path);
    let skill = skill.expect("should load");
    assert_eq!(skill.base_dir(), Path::new("/skills/my-skill"));
}

// -- Directory scanning -------------------------------------------------

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
    let paths = discover_skill_paths(Path::new("/nonexistent/path"));
    assert!(paths.is_empty());
}

// -- Multi-directory loading --------------------------------------------

fn set_skill_mtime(path: &Path, seconds_since_epoch: u64) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open skill file");
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds_since_epoch);
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
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

#[test]
fn load_from_empty_dirs() {
    let result = load_skills_from_dirs(&[]);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.is_empty());
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

#[test]
fn built_in_tau_self_knowledge_skills_load_from_embedded_markdown() {
    let skills = built_in_skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "tau-self-knowledge",
            "tau-self-knowledge-architecture",
            "tau-self-knowledge-harness",
            "tau-self-knowledge-config",
            "tau-self-knowledge-cli-ui",
            "tau-self-knowledge-email",
            "tau-self-knowledge-ext-pim",
            "tau-self-knowledge-ext-provider-builtin",
            "tau-self-knowledge-ext-rhai",
            "tau-self-knowledge-ext-shell",
            "tau-self-knowledge-ext-std-notifications",
            "tau-self-knowledge-ext-test-dummy",
            "tau-self-knowledge-ext-websearch",
            "tau-self-knowledge-prompt-templating",
            "tau-self-knowledge-source-code",
            "tau-self-knowledge-community",
            "tau-self-knowledge-debugging",
            "tau-self-knowledge-e2e-testing",
        ]
    );

    let skill = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge")
        .expect("built-in tau self-knowledge skill");
    assert_eq!(
        skill.description,
        "Use this skill when the user asks about the Tau coding agent they are running in, including what Tau is, how it works, built-in self-knowledge, configuration, debugging, source code, community links, or where to find Tau-specific help."
    );
    assert!(skill.add_to_prompt);
    assert!(skill.content.contains("# Tau self-knowledge"));
    assert!(!skill.content.contains(SELF_KNOWLEDGE_VERSION_TOKEN));
    assert!(
        skill
            .content
            .contains(&format!("Tau version `{TAU_VERSION}`"))
    );
    assert!(skill.content.contains("tau-self-knowledge-architecture"));
    assert!(skill.content.contains("tau-self-knowledge-harness"));
    assert!(skill.content.contains("tau-self-knowledge-config"));
    assert!(skill.content.contains("tau-self-knowledge-cli-ui"));
    assert!(skill.content.contains("tau-self-knowledge-email"));
    assert!(skill.content.contains("tau-self-knowledge-ext-pim"));
    assert!(
        skill
            .content
            .contains("tau-self-knowledge-ext-provider-builtin")
    );
    assert!(skill.content.contains("tau-self-knowledge-ext-rhai"));
    assert!(skill.content.contains("tau-self-knowledge-ext-shell"));
    assert!(
        skill
            .content
            .contains("tau-self-knowledge-ext-std-notifications")
    );
    assert!(skill.content.contains("tau-self-knowledge-ext-test-dummy"));
    assert!(skill.content.contains("tau-self-knowledge-ext-websearch"));
    assert!(
        skill
            .content
            .contains("tau-self-knowledge-prompt-templating")
    );
    assert!(skill.content.contains("tau-self-knowledge-source-code"));
    assert!(skill.content.contains("tau-self-knowledge-community"));
    assert!(skill.content.contains("tau-self-knowledge-debugging"));
    assert!(skill.content.contains("tau-self-knowledge-e2e-testing"));

    let architecture = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-architecture")
        .expect("built-in architecture skill");
    assert!(!architecture.add_to_prompt);
    assert!(architecture.content.contains("# Tau architecture overview"));

    let harness = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-harness")
        .expect("built-in harness skill");
    assert!(!harness.add_to_prompt);
    assert!(harness.content.contains("# Tau harness daemon"));
    assert!(harness.content.contains("Socket activation"));

    let config = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-config")
        .expect("built-in config skill");
    assert!(!config.add_to_prompt);
    assert!(config.content.contains("tau provider add"));

    let cli_ui = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-cli-ui")
        .expect("built-in CLI UI skill");
    assert!(!cli_ui.add_to_prompt);
    assert!(cli_ui.content.contains("complete_with_command"));
    assert!(cli_ui.content.contains("slash/action completion"));

    let prompt_templating = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-prompt-templating")
        .expect("built-in prompt templating skill");
    assert!(!prompt_templating.add_to_prompt);
    assert!(prompt_templating.content.contains("prompt_fragments"));
    assert!(prompt_templating.content.contains("starts_with"));

    let email = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-email")
        .expect("built-in email skill");
    assert!(!email.add_to_prompt);
    assert!(email.content.contains("std-email"));
    assert!(email.content.contains("trusted_authserv_ids"));
    assert!(email.content.contains("Authentication-Results"));

    let pim = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-pim")
        .expect("built-in pim extension skill");
    assert!(!pim.add_to_prompt);
    assert!(pim.content.contains("{pim_config}"));
    assert!(pim.content.contains("Google Calendar authorization"));

    let provider_builtin = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-provider-builtin")
        .expect("built-in provider extension skill");
    assert!(!provider_builtin.add_to_prompt);
    assert!(provider_builtin.content.contains("tau provider add"));
    assert!(provider_builtin.content.contains("ChatGPT/Codex"));

    let rhai = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-rhai")
        .expect("built-in rhai extension skill");
    assert!(!rhai.add_to_prompt);
    assert!(rhai.content.contains("std-rhai"));
    assert!(rhai.content.contains("on_intercept"));

    let shell = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-shell")
        .expect("built-in shell extension skill");
    assert!(!shell.add_to_prompt);
    assert!(shell.content.contains("core-shell"));
    assert!(shell.content.contains("dir_lock"));

    let notifications = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-std-notifications")
        .expect("built-in notifications extension skill");
    assert!(!notifications.add_to_prompt);
    assert!(notifications.content.contains("std-notifications"));
    assert!(notifications.content.contains("agent_idle"));

    let test_dummy = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-test-dummy")
        .expect("built-in test dummy extension skill");
    assert!(!test_dummy.add_to_prompt);
    assert!(test_dummy.content.contains("restart_test_dummy"));

    let websearch = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-ext-websearch")
        .expect("built-in websearch extension skill");
    assert!(!websearch.add_to_prompt);
    assert!(websearch.content.contains("std-websearch"));
    assert!(websearch.content.contains("Parallel.ai"));

    let source_code = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-source-code")
        .expect("built-in source code skill");
    assert!(!source_code.add_to_prompt);
    assert!(
        source_code
            .content
            .contains("rad:z3ToHcxKefTYxZEoCoDXmddUkK3a4")
    );

    let community = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-community")
        .expect("built-in community skill");
    assert!(!community.add_to_prompt);
    assert!(community.content.contains("GitHub Discussions"));

    let debugging = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-debugging")
        .expect("built-in debugging skill");
    assert!(!debugging.add_to_prompt);
    assert!(debugging.content.contains("## Important paths"));

    let e2e_testing = skills
        .iter()
        .find(|skill| skill.name == "tau-self-knowledge-e2e-testing")
        .expect("built-in E2E testing skill");
    assert!(!e2e_testing.add_to_prompt);
    assert!(e2e_testing.content.contains("testing_providers"));
    assert!(e2e_testing.content.contains("tau dev tmux"));
}

/// Ensures scoped skill directories still apply their configured default
/// prompt visibility only when skill frontmatter omits an explicit advertise
/// setting, preventing root-precedence metadata from changing prompt behavior.
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
    fs::write(
        nested.join("SKILL.md"),
        "---\nname: real-skill\ndescription: real skill\n---\n",
    )
    .expect("write");

    let link = tmp.path().join("link");
    symlink(&real, &link).expect("symlink");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("link/nested/SKILL.md"));

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "real-skill");
    assert!(!result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning && diagnostic.message.contains("symlink")
    }));
}

/// Ensures direct symlinked Markdown files at a skill root are discovered and
/// loaded instead of producing a warning and being skipped.
#[test]
fn discover_follows_symlinked_files() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("external.md");
    fs::write(
        &target,
        "---\nname: external\ndescription: External skill\n---\n",
    )
    .expect("write");
    symlink(&target, tmp.path().join("external.md")).expect("symlink");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("external.md"));

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "external");
    assert!(!result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning && diagnostic.message.contains("symlink")
    }));
}

/// Ensures a directory-level `SKILL.md` may be a symlink and still causes that
/// directory to be treated as a single Pi-style skill.
#[test]
fn discover_follows_symlinked_skill_md() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let target = outside.path().join("SKILL.md");
    fs::write(
        &target,
        "---\nname: linked-skill-md\ndescription: Linked skill\n---\n",
    )
    .expect("write");
    let skill_dir = tmp.path().join("linked");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    symlink(&target, skill_dir.join("SKILL.md")).expect("symlink");
    fs::create_dir_all(skill_dir.join("nested")).expect("mkdir nested");
    fs::write(
        skill_dir.join("nested").join("SKILL.md"),
        "---\nname: nested\ndescription: Nested skill\n---\n",
    )
    .expect("write nested");

    let paths = discover_skill_paths(tmp.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("linked/SKILL.md"));

    let result = load_skills_from_dirs(&[tmp.path().to_owned()]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "linked-skill-md");
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
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: outside-skill\ndescription: Outside skill\n---\n",
    )
    .expect("write");
    let link = tmp.path().join("skills");
    symlink(&real, &link).expect("symlink");

    let result = load_skills_from_dirs(&[link]);
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "outside-skill");
    assert!(!result.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == DiagnosticKind::Warning && diagnostic.message.contains("symlink")
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

#[test]
fn skill_name_validation_helpers_report_first_error() {
    assert!(is_valid_skill_name("valid-skill-1"));
    assert_eq!(
        skill_name_validation_message("").as_deref(),
        Some("name is empty")
    );
    assert!(
        skill_name_validation_message("Bad_Name")
            .expect("invalid name")
            .contains("invalid characters")
    );
    assert!(
        skill_name_validation_message("-bad")
            .expect("invalid name")
            .contains("hyphen")
    );
}

// -- strip_frontmatter --------------------------------------------------

#[test]
fn strip_frontmatter_returns_body() {
    let content = "---\nname: x\n---\nThe body.";
    assert_eq!(strip_frontmatter(content), "The body.");
}

#[test]
fn strip_frontmatter_no_frontmatter() {
    let content = "Just content.";
    assert_eq!(strip_frontmatter(content), "Just content.");
}

#[test]
fn has_unclosed_frontmatter_detects_missing_closing_fence() {
    assert!(has_unclosed_frontmatter("---\nname: x\n"));
    assert!(has_unclosed_frontmatter("\u{feff}---\r\nname: x\r\n"));
    assert!(!has_unclosed_frontmatter("---\nname: x\n---\nBody"));
    assert!(!has_unclosed_frontmatter("--- not a fence\n"));
    assert!(!has_unclosed_frontmatter("Body only"));
}
