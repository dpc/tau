mod support;

use std::ffi as path_std_ffi;
use std::path::Path;
use std::process::{Command as path_std_process_Command, Output};

use support::isolated_tau_command;
use tempfile::TempDir;

fn preview(home: &TempDir, environment: Option<&str>, args: &[&str]) -> Output {
    preview_at(home.path(), environment, args)
}

fn preview_at(home: &Path, environment: Option<&str>, args: &[&str]) -> Output {
    let work = home.join("work");
    let mut command = isolated_tau_command(env!("CARGO_BIN_EXE_tau"), home);
    command.current_dir(work).args(args);
    if let Some(environment) = environment {
        command.env("TAU_ENABLE_EXTENSIONS", environment);
    }
    command.output().expect("run tau preview")
}

fn persistent_tree_snapshot(home: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(base: &Path, path: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        let relative = path
            .strip_prefix(base)
            .expect("snapshot path below base")
            .to_string_lossy()
            .into_owned();
        if metadata.is_dir() {
            entries.push((
                format!("d:{relative}:{:?}", metadata.permissions()),
                Vec::new(),
            ));
            let mut children = std::fs::read_dir(path)
                .expect("read snapshot directory")
                .map(|entry| entry.expect("snapshot entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(base, &child, entries);
            }
        } else {
            entries.push((
                format!("f:{relative}:{:?}", metadata.permissions()),
                std::fs::read(path).expect("read snapshot file"),
            ));
        }
    }

    let mut entries = Vec::new();
    for root in [".config", ".state", ".cache", ".data", "work"] {
        visit(home, &home.join(root), &mut entries);
    }
    entries
}

fn assert_no_runtime_pairs(home: &Path) {
    let harnesses = home.join(".runtime/tau/harnesses");
    assert_eq!(
        std::fs::read_dir(harnesses)
            .expect("harness runtime directory")
            .count(),
        0,
        "preview must not leave a lifecycle pair"
    );
}

/// All render previews preserve the entire seeded persistent tree on success,
/// handled post-spawn failure, and mixed concurrent execution.
#[test]
fn previews_are_memory_only_across_success_failure_and_concurrency() {
    let home = TempDir::new().expect("temporary home");
    std::fs::create_dir_all(home.path().join(".state/tau/agents/seed")).expect("seed state");
    std::fs::write(
        home.path().join(".state/tau/agents/seed/events.cbor"),
        b"durable sentinel",
    )
    .expect("write sentinel");
    std::fs::create_dir_all(home.path().join(".cache/tau/ext/seed")).expect("seed cache");
    std::fs::write(
        home.path().join(".cache/tau/ext/seed/value"),
        b"cache sentinel",
    )
    .expect("write cache sentinel");
    std::fs::create_dir_all(home.path().join(".config")).expect("config root");
    std::fs::create_dir_all(home.path().join(".data")).expect("data root");
    std::fs::create_dir_all(home.path().join("work")).expect("work root");
    let before = persistent_tree_snapshot(home.path());

    for command in ["print-prompt", "print-tools", "print-system-prompt"] {
        let output = preview(&home, None, &["--role", "engineer", "dev", command]);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(persistent_tree_snapshot(home.path()), before);
        assert_no_runtime_pairs(home.path());
    }

    let failure = preview(
        &home,
        None,
        &["--role", "missing-role", "dev", "print-tools"],
    );
    assert!(!failure.status.success());
    assert_eq!(persistent_tree_snapshot(home.path()), before);
    assert_no_runtime_pairs(home.path());

    let mut threads = Vec::new();
    for index in 0..8 {
        let home = home.path().to_path_buf();
        threads.push(std::thread::spawn(move || {
            let command = if index % 2 == 0 {
                "print-prompt"
            } else {
                "print-tools"
            };
            preview_at(&home, None, &["--role", "engineer", "dev", command])
        }));
    }
    for thread in threads {
        let output = thread.join().expect("preview thread");
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    assert_eq!(persistent_tree_snapshot(home.path()), before);
    assert_no_runtime_pairs(home.path());
}

/// Ensures `print-system-prompt` exposes the built-in delegate-role catalog
/// after harness guidance and before the workdir epilogue.
#[test]
fn print_system_prompt_places_delegate_roles_late() {
    let home = TempDir::new().expect("temporary home");
    std::fs::create_dir_all(home.path().join("work")).expect("work directory");

    let output = preview(
        &home,
        None,
        &["--role", "engineer", "dev", "print-system-prompt"],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let prompt = String::from_utf8(output.stdout).expect("UTF-8 system prompt");
    let catalog_offset = prompt
        .find("## Available sub-task roles")
        .expect("delegate role catalog");
    assert!(
        prompt.find("# Tau harness").expect("harness heading") < catalog_offset,
        "prompt diagnostic must retain late role-catalog placement"
    );
    assert!(
        catalog_offset
            < prompt
                .find("# Agent identity")
                .expect("agent identity heading"),
        "prompt diagnostic must retain the catalog before agent identity"
    );
    assert!(
        catalog_offset
            < prompt
                .find("### Shell workdirs")
                .expect("shell workdir epilogue"),
        "prompt diagnostic must retain the catalog before the priority-900 workdir epilogue"
    );
}

/// Prompt previews omit a conditionally empty shell fragment regardless of how
/// the extension is enabled, while retaining CLI precedence and the
/// deterministic fake id.
#[test]
fn print_prompt_omits_conditionally_empty_extension_fragment() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        "extensions:\n  core-shell:\n    enable: false\n",
    )
    .expect("write harness config");

    let baseline = preview(&home, None, &["--role", "engineer", "dev", "print-prompt"]);
    let from_environment = preview(
        &home,
        Some("core-shell"),
        &["--role", "engineer", "dev", "print-prompt"],
    );
    let cli_disabled = preview(
        &home,
        Some("core-shell"),
        &[
            "--disable-extension",
            "core-shell",
            "--role",
            "engineer",
            "dev",
            "print-prompt",
        ],
    );
    let empty_environment = preview(
        &home,
        Some(" \t"),
        &["--role", "engineer", "dev", "print-prompt"],
    );
    for output in [
        &baseline,
        &from_environment,
        &cli_disabled,
        &empty_environment,
    ] {
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("dev-preview-agent"),
            "fake preview identity must remain deterministic"
        );
    }
    assert_eq!(baseline.stdout, cli_disabled.stdout);
    assert_eq!(baseline.stdout, empty_environment.stdout);
    assert_ne!(baseline.stdout, from_environment.stdout);
    assert!(
        String::from_utf8_lossy(&from_environment.stdout).contains("### Shell workdirs"),
        "{:?}",
        from_environment.stdout
    );
}

/// Ensures prompt previews initialize an ephemeral agent through extension
/// context readiness before strict role templates read its shell workdir.
#[test]
fn print_prompt_supplies_workdir_context_to_strict_role_template() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(config_dir.join("prompts")).expect("create prompts directory");
    std::fs::write(
        config_dir.join("prompts/workdir.hbs"),
        "{{#each agent_context.workdir}}preview workdir: {{value.path}}{{/each}}\n",
    )
    .expect("write strict workdir template");
    std::fs::write(
        config_dir.join("harness.yaml"),
        "agents:\n  role_groups:\n    engineer:\n      roles:\n        engineer:\n          prompt_override: workdir\n",
    )
    .expect("write harness config");

    let output = preview(
        &home,
        Some("core-shell"),
        &["--role", "engineer", "dev", "print-prompt"],
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!(
            "preview workdir: {}",
            home.path().join("work").display()
        )),
        "{:?}",
        output.stdout
    );
}

/// Ensures role-less developer previews use normal startup-role selection while
/// `--role` continues to select an explicit prompt and tool policy.
#[test]
fn previews_use_configured_default_role_unless_overridden() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"agents:
  default_role: preview-default
  role_groups:
    preview:
      roles:
        preview-default:
          prompt_fragments:
            - name: preview.default
              priority: 10
              text: DEFAULT PREVIEW ROLE
          tools: [read]
        preview-explicit:
          prompt_fragments:
            - name: preview.explicit
              priority: 10
              text: EXPLICIT PREVIEW ROLE
          tools: [grep]
"#,
    )
    .expect("write harness config");

    let default_prompt = preview(&home, None, &["dev", "print-prompt"]);
    let explicit_prompt = preview(
        &home,
        None,
        &["--role", "preview-explicit", "dev", "print-prompt"],
    );
    let default_tools = preview(&home, None, &["dev", "print-tools"]);
    let explicit_tools = preview(
        &home,
        None,
        &["--role", "preview-explicit", "dev", "print-tools"],
    );

    for output in [
        &default_prompt,
        &explicit_prompt,
        &default_tools,
        &explicit_tools,
    ] {
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    assert!(String::from_utf8_lossy(&default_prompt.stdout).contains("DEFAULT PREVIEW ROLE"));
    assert!(!String::from_utf8_lossy(&default_prompt.stdout).contains("EXPLICIT PREVIEW ROLE"));
    assert!(String::from_utf8_lossy(&explicit_prompt.stdout).contains("EXPLICIT PREVIEW ROLE"));
    assert!(!String::from_utf8_lossy(&explicit_prompt.stdout).contains("DEFAULT PREVIEW ROLE"));

    let tool_names = |output: &Output| {
        serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
            .expect("tool preview JSON")
            .into_iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .expect("tool definition name")
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(tool_names(&default_tools), ["read"]);
    assert_eq!(tool_names(&explicit_tools), ["grep"]);
}

/// Proves model-aware tool previews use provider-published metadata and
/// preserve explicit role grants for both Codex-style and ordinary shell
/// models.
#[test]
fn print_tools_matches_model_defaults_and_explicit_role_grants() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    let settings_dir = home
        .path()
        .join(".state/tau/provider-settings/provider-builtin");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::create_dir_all(&settings_dir).expect("create provider settings directory");
    std::fs::create_dir_all(home.path().join("work")).expect("create work directory");
    std::fs::write(
        settings_dir.join("chatgpt.json"),
        r#"{
  "kind": "chatgpt",
  "credential": {
    "kind": "oauth",
    "secret_path": "providers/chatgpt/oauth.json"
  }
}"#,
    )
    .expect("write ChatGPT provider settings");
    std::fs::write(
        settings_dir.join("local.json"),
        r#"{
  "kind": "chat_completions",
  "base_url": "http://127.0.0.1:1/v1",
  "models": [{"id": "ordinary", "context_window": 32768}],
  "credential": {
    "kind": "api_key",
    "secret_path": "providers/local/api-key.json"
  }
}"#,
    )
    .expect("write ordinary provider settings");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"agents:
  role_groups:
    preview:
      roles:
        codex-default:
          model: chatgpt/gpt-5.6-luna
        codex-explicit:
          model: chatgpt/gpt-5.6-luna
          tools: [read, grep, ls]
        ordinary-default:
          model: local/ordinary
"#,
    )
    .expect("write harness config");

    let names = |role: &str| {
        let output = preview(&home, None, &["--role", role, "dev", "print-tools"]);
        assert!(output.status.success(), "{:?}", output.stderr);
        let tools = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
            .expect("tool preview JSON");
        assert!(
            tools
                .iter()
                .all(|tool| tool.get("model_visible_name").is_none()),
            "preview must expose only provider-visible names: {tools:?}"
        );
        tools
            .into_iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .expect("tool definition name")
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    let codex_default = names("codex-default");
    assert!(codex_default.contains(&"apply_patch".to_owned()));
    assert!(codex_default.contains(&"shell_command".to_owned()));
    assert!(!codex_default.contains(&"read".to_owned()));
    assert!(!codex_default.contains(&"grep".to_owned()));
    assert!(!codex_default.contains(&"ls".to_owned()));
    assert_eq!(names("codex-explicit"), ["grep", "ls", "read"]);

    let ordinary_default = names("ordinary-default");
    assert!(ordinary_default.contains(&"edit".to_owned()));
    assert!(ordinary_default.contains(&"read".to_owned()));
    assert!(ordinary_default.contains(&"grep".to_owned()));
    assert!(ordinary_default.contains(&"ls".to_owned()));
    assert!(!ordinary_default.contains(&"apply_patch".to_owned()));
    assert!(!ordinary_default.contains(&"shell_command".to_owned()));
}

/// Ensures CLI subprocess fixtures clear inherited Tau profile, transport,
/// secret, home, and XDG inputs before installing their private roots.
#[test]
fn preview_subprocesses_ignore_ambient_tau_environment() {
    let ambient_home = TempDir::new().expect("ambient home");
    let ambient_config_dir = ambient_home.path().join(".config/tau");
    std::fs::create_dir_all(&ambient_config_dir).expect("ambient config directory");
    std::fs::write(
        ambient_config_dir.join("harness.yaml"),
        "agents:\n  default_role: ambient-role\n",
    )
    .expect("ambient harness configuration");

    let output = path_std_process_Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "preview_subprocesses_ignore_ambient_tau_environment_child",
        ])
        .env("TAU_PROFILE", "ambient-profile")
        .env(tau_harness::ROLE_CLI_OVERRIDES_ENV, r#"["ambient-role"]"#)
        .env(
            tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
            r#"["ambient-config"]"#,
        )
        .env(tau_harness::STARTUP_ROLE_ENV, "ambient-role")
        .env("TAU_SECRET_AMBIENT_REGRESSION", "must-not-be-forwarded")
        .env("HOME", ambient_home.path())
        .env("XDG_CONFIG_HOME", ambient_home.path().join(".config"))
        .env("XDG_STATE_HOME", ambient_home.path().join(".state"))
        .env("XDG_CACHE_HOME", ambient_home.path().join(".cache"))
        .env("XDG_DATA_HOME", ambient_home.path().join(".data"))
        .env("XDG_RUNTIME_DIR", ambient_home.path().join(".runtime"))
        .output()
        .expect("run isolated fixture child");
    assert!(
        output.status.success(),
        "fixture child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs the private preview fixture beneath a poisoned parent environment.
#[test]
#[ignore = "run only through the isolated parent regression"]
fn preview_subprocesses_ignore_ambient_tau_environment_child() {
    for (key, value) in [
        ("TAU_PROFILE", "ambient-profile"),
        (tau_harness::ROLE_CLI_OVERRIDES_ENV, r#"["ambient-role"]"#),
        (
            tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
            r#"["ambient-config"]"#,
        ),
        (tau_harness::STARTUP_ROLE_ENV, "ambient-role"),
        ("TAU_SECRET_AMBIENT_REGRESSION", "must-not-be-forwarded"),
    ] {
        assert_eq!(std::env::var(key).as_deref(), Ok(value));
    }

    let home = TempDir::new().expect("private home");
    let clean_environment = isolated_tau_command(
        std::env::current_exe().expect("test executable"),
        home.path(),
    )
    .args([
        "--ignored",
        "--exact",
        "preview_subprocesses_ignore_ambient_tau_environment_clean_child",
    ])
    .output()
    .expect("run clean-environment fixture child");
    assert!(
        clean_environment.status.success(),
        "clean environment child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&clean_environment.stdout),
        String::from_utf8_lossy(&clean_environment.stderr)
    );

    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(&config_dir).expect("private config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
agents:
  default_role: private-role
  role_groups:
    fixture:
      roles:
        private-role:
          prompt_fragments:
            - name: fixture.private
              priority: 10
              text: PRIVATE PREVIEW FIXTURE
"#,
    )
    .expect("private harness configuration");

    let output = preview(&home, None, &["dev", "print-prompt"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(String::from_utf8_lossy(&output.stdout).contains("PRIVATE PREVIEW FIXTURE"));
}

/// Verifies the reusable subprocess helper removes every poisoned startup
/// input.
#[test]
#[ignore = "run only through the isolated parent regression"]
fn preview_subprocesses_ignore_ambient_tau_environment_clean_child() {
    for key in [
        "TAU_PROFILE",
        tau_harness::ROLE_CLI_OVERRIDES_ENV,
        tau_harness::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
        tau_harness::STARTUP_ROLE_ENV,
        "TAU_SECRET_AMBIENT_REGRESSION",
    ] {
        assert!(std::env::var(key).is_err(), "{key} leaked into fixture");
    }
}

/// Proves tool previews expose a disabled-by-default extension from the public
/// environment and apply later CLI disable/re-enable operations in argv order.
#[test]
fn print_tools_composes_extension_environment_and_ordered_cli() {
    let home = TempDir::new().expect("temporary home");
    let env_only = preview(
        &home,
        Some("test-dummy"),
        &["--role", "engineer", "dev", "print-tools"],
    );
    let disabled = preview(
        &home,
        Some("test-dummy"),
        &[
            "--disable-extension",
            "test-dummy",
            "--role",
            "engineer",
            "dev",
            "print-tools",
        ],
    );
    let reenabled = preview(
        &home,
        Some("test-dummy"),
        &[
            "--disable-extension",
            "test-dummy",
            "--enable-extension",
            "test-dummy",
            "--role",
            "engineer",
            "dev",
            "print-tools",
        ],
    );
    let duplicated = preview(
        &home,
        Some("test-dummy,test-dummy"),
        &["--role", "engineer", "dev", "print-tools"],
    );
    for output in [&env_only, &disabled, &reenabled, &duplicated] {
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    let has_dummy = |output: &Output| {
        String::from_utf8_lossy(&output.stdout).contains("\"name\": \"restart_test_dummy\"")
    };
    assert!(has_dummy(&env_only));
    assert!(!has_dummy(&disabled));
    assert!(has_dummy(&reenabled));
    assert_eq!(env_only.stdout, duplicated.stdout);
}

/// Ensures both preview commands fail through the supported public parser for
/// malformed and unknown extension names rather than silently rendering.
#[test]
fn previews_reject_invalid_extension_environment() {
    for command in ["print-prompt", "print-tools"] {
        for value in ["test-dummy,,core-shell", "not-configured"] {
            let home = TempDir::new().expect("temporary home");
            let output = preview(&home, Some(value), &["--role", "engineer", "dev", command]);
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("TAU_ENABLE_EXTENSIONS"));
        }
    }
}

/// Ensures both preview commands reject non-UTF-8 public environment input at
/// the outer supported parser before spawning a render daemon.
#[cfg(unix)]
#[test]
fn previews_reject_non_utf8_extension_environment() {
    use std::os::unix::ffi::OsStringExt as _;

    for command_name in ["print-prompt", "print-tools"] {
        let home = TempDir::new().expect("temporary home");
        let output = isolated_tau_command(env!("CARGO_BIN_EXE_tau"), home.path())
            .env(
                "TAU_ENABLE_EXTENSIONS",
                path_std_ffi::OsString::from_vec(vec![0xff]),
            )
            .args(["--role", "engineer", "dev", command_name])
            .output()
            .expect("run tau preview");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("valid UTF-8"));
    }
}
