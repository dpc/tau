use tempfile::TempDir;

use super::*;

/// Ensures omission preserves unrestricted behavior without requiring the
/// supplied cwd to exist.
#[test]
fn absent_allowlist_does_not_touch_or_restrict_the_workdir() {
    let config = ShellConfig::default();
    assert_eq!(
        config
            .authorize("anything", Path::new("/definitely/missing/tau-cwd"))
            .expect("absent allowlist is unrestricted"),
        None
    );
}

/// Ensures omitted command enforcement preserves the existing shell prompt
/// without an allowlist declaration.
#[test]
fn absent_allowlist_omits_the_prompt_fragment() {
    assert_eq!(ShellConfig::default().allowlist_prompt_fragment(), None);
}

/// Ensures an enabled allowlist renders its typed command selectors and paired
/// canonical-workdir selectors rather than claiming they are literal commands.
#[test]
fn allowlist_prompt_lists_typed_selector_pairs() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [
            {
                "workdir": "/srv/project/**",
                "command": "cargo *"
            },
            {
                "workdir": "/srv/project",
                "command_regex": "jj (?:log|show)"
            }
        ]
    }))
    .expect("parse allowlist");

    assert_eq!(
        config.allowlist_prompt_fragment(),
        Some(
            "\n\n### Shell command allowlist\n\n\
             Shell command enforcement is enabled. A raw shell command and its \
             canonical effective workdir must both match one selector pair:\n\
             - command_glob: \"cargo *\"; workdir: \"/srv/project/**\"\n\
             - command_regex: \"jj (?:log|show)\"; workdir: \"/srv/project\""
                .to_owned()
        )
    );
}

/// Ensures an explicit empty allowlist states the total command denial instead
/// of making an enabled guardrail appear unrestricted.
#[test]
fn empty_allowlist_prompt_states_that_all_commands_are_denied() {
    let config: ShellConfig =
        serde_json::from_value(serde_json::json!({ "allowlist": [] })).expect("parse allowlist");

    assert_eq!(
        config.allowlist_prompt_fragment(),
        Some(
            "\n\n### Shell command allowlist\n\n\
             Shell command enforcement is enabled. A raw shell command and its \
             canonical effective workdir must both match one selector pair:\n\
             - none (all shell commands are denied)"
                .to_owned()
        )
    );
}

/// Ensures prompt presentation has stable set ordering while execution retains
/// the configured authored-rule order and duplicates.
#[test]
fn allowlist_prompt_sorts_and_deduplicates_selector_pairs() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [
            {
                "workdir": "/srv/z",
                "command_regex": "z"
            },
            {
                "workdir": "/srv/a",
                "command": "a"
            },
            {
                "workdir": "/srv/z",
                "command_regex": "z"
            }
        ]
    }))
    .expect("parse allowlist");

    let prompt = config
        .allowlist_prompt_fragment()
        .expect("enabled allowlist has a prompt fragment");
    assert_eq!(prompt.matches("command_regex: \"z\"").count(), 1);
    assert!(
        prompt.find("command_glob: \"a\"").expect("glob selector")
            < prompt.find("command_regex: \"z\"").expect("regex selector")
    );
}

/// Ensures valid glob braces remain the exact JSON-decoded selector while
/// escaping Handlebars delimiters before the selector reaches prompt assembly.
#[test]
fn allowlist_prompt_escapes_handlebars_delimiters_in_glob_selectors() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": "/srv/project",
            "command": "{{cargo,jj}} *"
        }]
    }))
    .expect("parse nested-brace glob");

    let prompt = config
        .allowlist_prompt_fragment()
        .expect("enabled allowlist has a prompt fragment");
    let selector = r#""\u007b\u007bcargo,jj\u007d\u007d *""#;
    assert!(prompt.contains(&format!("command_glob: {selector}")));
    assert_eq!(
        serde_json::from_str::<String>(selector).expect("selector JSON"),
        "{{cargo,jj}} *"
    );
    assert!(
        !prompt.contains("{{"),
        "selectors must stay literal when appended to a Handlebars template"
    );
}

/// Ensures command and workdir matchers bind as one conjunctive rule rather
/// than combining halves from different entries.
#[test]
fn allowlist_rules_bind_command_and_workdir_as_pairs() {
    let temp = TempDir::new().expect("tempdir");
    let other = TempDir::new().expect("other tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [
            {
                "workdir": temp.path().display().to_string(),
                "command": "cargo *"
            },
            {
                "workdir": other.path().display().to_string(),
                "command": "jj status"
            }
        ]
    }))
    .expect("parse allowlist");

    assert!(
        config
            .authorize("cargo test", temp.path())
            .expect("matching pair")
            .is_some()
    );
    let error = config
        .authorize("jj status", temp.path())
        .expect_err("split pair must not authorize");
    assert!(error.contains("allowed command/workdir rule pairs:"));
    assert!(error.contains(r#"command_glob: "cargo *""#));
    assert!(error.contains(&format!(r#"workdir: "{}""#, temp.path().display())));
}

/// Ensures the documented `jj` regular expression accepts only the intended
/// raw command language at each length and shell-syntax boundary.
#[test]
fn command_regex_matches_only_the_documented_jj_language() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": temp.path().display().to_string(),
            "command_regex": "jj (?:log|show [a-z]{6,32})"
        }]
    }))
    .expect("parse regex allowlist");

    for command in [
        "jj log",
        "jj show abcdef",
        "jj show abcdefghijklmnopqrstuvwxyzabcdef",
    ] {
        assert!(
            config
                .authorize(command, temp.path())
                .expect("permitted command")
                .is_some(),
            "{command:?} must match"
        );
    }
    for command in [
        "jj show abcde",
        "jj show abcdefghijklmnopqrstuvwxyzabcdefg",
        "jj show ABCDEF",
        "jj show abcde1",
        "jj show abcdéf",
        " jj log",
        "jj  log",
        "jj log ",
        "jj log\n",
        "jj log --no-pager",
        "jj show abcdef extra",
        "jj log | cat",
        "jj log >output",
        "jj log $(true)",
        "jj log `true`",
        "jj log; true",
        "jj log && true",
        "jj log || true",
    ] {
        assert!(
            config.authorize(command, temp.path()).is_err(),
            "{command:?} must be denied"
        );
    }
}

/// Ensures regex rules implicitly use absolute anchors even when multiline mode
/// would make ordinary line anchors accept a substring.
#[test]
fn command_regex_uses_implicit_absolute_whole_string_anchors() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": temp.path().display().to_string(),
            "command_regex": "(?m)^printf allowed$"
        }]
    }))
    .expect("parse regex allowlist");

    assert!(
        config
            .authorize("printf allowed", temp.path())
            .expect("whole command matches")
            .is_some()
    );
    assert!(
        config
            .authorize("printf allowed\nprintf denied", temp.path())
            .is_err(),
        "multiline mode must not weaken implicit absolute anchors"
    );
}

/// Ensures regex configuration stays case-sensitive even if an authored inline
/// flag attempts to weaken that stated allowlist invariant.
#[test]
fn command_regex_rejects_case_insensitive_inline_flags() {
    let error = serde_json::from_value::<ShellConfig>(serde_json::json!({
        "allowlist": [{
            "workdir": "/tmp/**",
            "command_regex": "(?i)jj log"
        }]
    }))
    .expect_err("case-insensitive regex must fail configuration");
    assert!(
        error
            .to_string()
            .contains("shell allowlist command regex must remain case-sensitive")
    );

    for pattern in [r"(?-i:jj log)", "(?x)# (?i)\njj\\x20log"] {
        let config: ShellConfig = serde_json::from_value(serde_json::json!({
            "allowlist": [{
                "workdir": "/tmp",
                "command_regex": pattern
            }]
        }))
        .expect("syntax-aware validation must accept case-sensitive regex");
        assert!(
            config
                .authorize("jj log", Path::new("/tmp"))
                .expect("accepted case-sensitive regex")
                .is_some()
        );
    }
}

/// Ensures command matching is anchored over the raw string while treating
/// separators and newlines as ordinary characters.
#[test]
fn command_globs_match_the_whole_raw_multiline_string() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": temp.path().display().to_string(),
            "command": "printf *"
        }]
    }))
    .expect("parse allowlist");

    assert!(
        config
            .authorize("printf a/b\nprintf second", temp.path())
            .expect("star spans separators and newline")
            .is_some()
    );
    assert!(
        config
            .authorize("x printf value", temp.path())
            .expect_err("glob is whole-string anchored")
            .contains("denied by configured allowlist")
    );
    assert!(
        config
            .authorize("Printf value", temp.path())
            .expect_err("matching is case-sensitive")
            .contains("denied by configured allowlist")
    );
}

/// Ensures workdir stars remain component-aware while double stars cross
/// multiple canonical path components.
#[test]
fn workdir_globs_distinguish_single_and_recursive_stars() {
    let temp = TempDir::new().expect("tempdir");
    let nested = temp.path().join("one/two");
    std::fs::create_dir_all(&nested).expect("create nested cwd");
    let single: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": format!("{}/*", temp.path().display()),
            "command": "*"
        }]
    }))
    .expect("single-star config");
    let recursive: ShellConfig = serde_json::from_value(serde_json::json!({
        "allowlist": [{
            "workdir": format!("{}/**", temp.path().display()),
            "command": "*"
        }]
    }))
    .expect("recursive config");

    assert!(
        single
            .authorize("true", &nested)
            .expect_err("single star cannot cross components")
            .contains("denied")
    );
    assert!(
        recursive
            .authorize("true", &nested)
            .expect("double star crosses components")
            .is_some()
    );
}

/// Ensures an explicitly empty allowlist denies every command and discloses
/// that no paired rules are configured.
#[test]
fn empty_allowlist_denies_all_with_stable_diagnostic() {
    let temp = TempDir::new().expect("tempdir");
    let config: ShellConfig =
        serde_json::from_value(serde_json::json!({ "allowlist": [] })).expect("parse allowlist");
    let error = config
        .authorize("echo denied", temp.path())
        .expect_err("empty allowlist denies all");
    assert_eq!(
        error,
        format!(
            "shell command denied by configured allowlist: no rule matched workdir {} and command\nallowed command/workdir rule pairs:\n- none",
            temp.path().canonicalize().expect("canonical").display()
        )
    );
}

/// Ensures malformed, incomplete, and relative allowlist rules fail during
/// configuration instead of silently widening execution.
#[test]
fn malformed_allowlist_rules_fail_configuration() {
    for (value, expected) in [
        (serde_json::json!({ "allowlist": null }), "sequence"),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**" }] }),
            "command",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "relative/**", "command": "*" }] }),
            "must be absolute",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/[", "command": "*" }] }),
            "invalid shell allowlist workdir glob",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**", "command": "[" }] }),
            "invalid shell allowlist command glob",
        ),
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**", "command": "*", "extra": true }] }),
            "unknown field",
        ),
    ] {
        let error = serde_json::from_value::<ShellConfig>(value).expect_err("malformed rule");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

/// Ensures exactly one command matcher is required, invalid regular expressions
/// fail closed, and resource diagnostics remain stable at configuration time.
#[test]
fn command_matcher_choice_and_resource_bounds_fail_configuration() {
    let at_rule_limit = (0..MAX_SHELL_ALLOWLIST_RULES)
        .map(|_| serde_json::json!({ "workdir": "/tmp/**", "command": "*" }))
        .collect::<Vec<_>>();
    serde_json::from_value::<ShellConfig>(serde_json::json!({ "allowlist": at_rule_limit }))
        .expect("exact rule-count limit must be accepted");

    let mut too_many_rules = (0..MAX_SHELL_ALLOWLIST_RULES)
        .map(|_| serde_json::json!({ "workdir": "/tmp/**", "command": "*" }))
        .collect::<Vec<_>>();
    too_many_rules.push(serde_json::json!({
        "workdir": "/tmp/**",
        "command_regex": format!("a{{{MAX_SHELL_ALLOWLIST_COMPILE_BYTES}}}")
    }));

    let exact_workdir_pattern = format!("/{}", "x".repeat(MAX_SHELL_ALLOWLIST_PATTERN_BYTES - 1));
    for (field, pattern) in [
        ("workdir", exact_workdir_pattern),
        ("command", "x".repeat(MAX_SHELL_ALLOWLIST_PATTERN_BYTES)),
        (
            "command_regex",
            "x".repeat(MAX_SHELL_ALLOWLIST_PATTERN_BYTES),
        ),
    ] {
        let mut rule = serde_json::Map::new();
        rule.insert("workdir".to_owned(), serde_json::json!("/tmp/**"));
        rule.insert(field.to_owned(), serde_json::Value::String(pattern));
        if field == "workdir" {
            rule.insert("command".to_owned(), serde_json::json!("*"));
        }
        serde_json::from_value::<ShellConfig>(serde_json::json!({
            "allowlist": [rule]
        }))
        .expect("exact authored-pattern limit must be accepted");
    }

    let oversized_pattern = "x".repeat(MAX_SHELL_ALLOWLIST_PATTERN_BYTES + 1);
    let compiled_too_large = format!("a{{{}}}", MAX_SHELL_ALLOWLIST_COMPILE_BYTES);
    for (value, expected) in [
        (
            serde_json::json!({ "allowlist": [{ "workdir": "/tmp/**" }] }),
            "shell allowlist rule requires exactly one of `command` or `command_regex`",
        ),
        (
            serde_json::json!({
                "allowlist": [{
                    "workdir": "/tmp/**",
                    "command": "*",
                    "command_regex": ".*"
                }]
            }),
            "shell allowlist rule requires exactly one of `command` or `command_regex`",
        ),
        (
            serde_json::json!({
                "allowlist": [{ "workdir": "/tmp/**", "command_regex": "(" }]
            }),
            "invalid shell allowlist command regex",
        ),
        (
            serde_json::json!({
                "allowlist": [{ "workdir": "/tmp/**", "command": oversized_pattern }]
            }),
            "shell allowlist `command` must not exceed 2048 authored UTF-8 bytes",
        ),
        (
            serde_json::json!({
                "allowlist": [{
                    "workdir": format!("/{}", "x".repeat(MAX_SHELL_ALLOWLIST_PATTERN_BYTES)),
                    "command": "*"
                }]
            }),
            "shell allowlist `workdir` must not exceed 2048 authored UTF-8 bytes",
        ),
        (
            serde_json::json!({
                "allowlist": [{
                    "workdir": "/tmp/**",
                    "command_regex": oversized_pattern
                }]
            }),
            "shell allowlist `command_regex` must not exceed 2048 authored UTF-8 bytes",
        ),
        (
            serde_json::json!({
                "allowlist": [{
                    "workdir": "/tmp/**",
                    "command_regex": compiled_too_large
                }]
            }),
            "shell allowlist command regex compilation must not exceed 262144 bytes",
        ),
        (
            serde_json::json!({ "allowlist": too_many_rules }),
            "shell allowlist permits at most 32 rules",
        ),
    ] {
        let error = serde_json::from_value::<ShellConfig>(value).expect_err("invalid allowlist");
        assert_eq!(error.to_string(), expected);
    }
}

/// Ensures YAML's recommended single-quoted scalar preserves regular-expression
/// escapes while strict command matcher selection remains active.
#[test]
fn command_regex_accepts_yaml_single_quoted_patterns() {
    let config: ShellConfig = serde_yaml_ng::from_str(
        r#"
allowlist:
  - workdir: /tmp
    command_regex: 'jj show [a-z]{6,32}\.json'
"#,
    )
    .expect("parse YAML regex allowlist");
    assert!(
        config
            .authorize("jj show abcdef.json", Path::new("/tmp"))
            .expect("matching YAML regex")
            .is_some()
    );
}

/// Ensures empty extra_env values implement the documented clear-variable
/// semantics instead of passing an empty string through to the child.
#[test]
fn empty_extra_env_removes_child_variable() {
    let mut extra_env = BTreeMap::new();
    extra_env.insert("HOME".to_owned(), String::new());
    let config = ShellConfig {
        extra_env,
        ..Default::default()
    };

    let output = config
        .command_for("printf \"${HOME+set}\"")
        .env_remove("HOME")
        .output()
        .expect("spawn shell");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");

    let output = config
        .spawn_isolated("printf \"${HOME+set}\"", None, false, false)
        .expect("spawn isolated shell")
        .child
        .wait_with_output()
        .expect("wait shell");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

/// Ensures the protected overlay wins over both inherited and ordinary
/// configured values while preserving TERM and unrelated pager variables.
#[test]
fn non_interactive_pager_overlay_has_final_precedence_and_narrow_scope() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "extra_env": {
            "PAGER": "configured-pager",
            "GIT_PAGER": "configured-git-pager",
            "GH_PAGER": "configured-gh-pager",
            "SYSTEMD_PAGER": "configured-systemd-pager",
            "TERM": "tau-term",
            "JJ_PAGER": "configured-jj-pager",
            "MANPAGER": "configured-man-pager",
            "BAT_PAGER": "configured-bat-pager"
        }
    }))
    .expect("parse shell config");
    let mut command = config.command_for(
        "printf '%s\\n' \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \
         \"$SYSTEMD_PAGER\" \"$TERM\" \"$JJ_PAGER\" \"$MANPAGER\" \"$BAT_PAGER\"",
    );
    command
        .env("PAGER", "inherited-pager")
        .env("GIT_PAGER", "inherited-git-pager");
    config.apply_environment(&mut command);

    let output = command.output().expect("run environment probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "cat\ncat\ncat\ncat\ntau-term\ncat\nconfigured-man-pager\nconfigured-bat-pager\n"
    );
}

/// Ensures the full shared preparation sequence preserves an inherited TERM
/// even when ordinary shell configuration does not mention TERM.
#[test]
fn shell_isolation_preserves_inherited_term_by_default() {
    let config = ShellConfig::default();
    let mut command = config.command_for("printf '%s' \"$TERM\"");
    command.env("TERM", "inherited-test-term");
    apply_command_isolation(&mut command);
    config.apply_environment(&mut command);

    let output = command.output().expect("run inherited TERM probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "inherited-test-term"
    );
}

/// Ensures the documented opt-out is explicit and leaves ordinary
/// `extra_env` pager and TERM choices intact.
#[test]
fn non_interactive_pager_opt_out_preserves_configured_environment() {
    let config: ShellConfig = serde_json::from_value(serde_json::json!({
        "non_interactive_pager": false,
        "extra_env": {
            "PAGER": "custom-pager",
            "GIT_PAGER": "custom-git-pager",
            "TERM": "custom-term"
        }
    }))
    .expect("parse shell config");
    let mut command = config.command_for("printf '%s\\n' \"$PAGER\" \"$GIT_PAGER\" \"$TERM\"");
    config.apply_environment(&mut command);

    let output = command.output().expect("run opt-out probe");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "custom-pager\ncustom-git-pager\ncustom-term\n"
    );
}

/// Ensures directory-lock backend config keeps memory as the default while
/// accepting the opt-in filesystem backend and state directory.
#[test]
fn dir_lock_backend_config_defaults_memory_and_parses_filesystem() {
    assert_eq!(
        ExtConfig::default().dir_lock.backend,
        DirLockBackendConfig::Memory
    );

    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "dir_lock": {
            "enable": true,
            "backend": "filesystem",
            "state_dir": "/tmp/tau-dir-locks"
        }
    }))
    .expect("parse dir_lock backend config");

    assert_eq!(config.dir_lock.backend, DirLockBackendConfig::Filesystem);
    assert_eq!(
        config.dir_lock.state_dir,
        Some(PathBuf::from("/tmp/tau-dir-locks"))
    );
}
