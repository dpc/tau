mod support;

use std::ffi as path_std_ffi;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};

use support::{isolated_runtime_dir, isolated_tau_command};
use tempfile::TempDir;

/// A portable-on-Unix configured extension command and its external launch
/// marker.
#[cfg(unix)]
struct PreviewSpawnCanary {
    /// Absolute executable path used in the extension configuration.
    command: std::path::PathBuf,
    /// Marker written as the command's quoted positional argument.
    marker: std::path::PathBuf,
}

#[cfg(unix)]
impl PreviewSpawnCanary {
    /// Configures an enabled external extension whose first action writes a
    /// marker outside Tau's cleanup roots. Unix gating is explicit because its
    /// script uses an absolute `/bin/sh` shebang.
    fn configure(home: &TempDir) -> Self {
        let config_dir = home.path().join(".config/tau");
        let marker = home.path().join("preview-extension-spawned");
        let command_path = home.path().join("preview-spawn-canary");
        std::fs::create_dir_all(&config_dir).expect("create canary config directory");
        std::fs::write(&command_path, "#!/bin/sh\nprintf spawned > \"$1\"\n")
            .expect("write preview spawn canary");
        std::fs::set_permissions(&command_path, Permissions::from_mode(0o700))
            .expect("make preview spawn canary executable");
        let command = serde_json::to_string(&[
            command_path.to_str().expect("UTF-8 canary command path"),
            marker.to_str().expect("UTF-8 canary marker path"),
        ])
        .expect("serialize canary command");
        std::fs::write(
            config_dir.join("harness.yaml"),
            format!("extensions:\n  spawn-canary:\n    command: {command}\n"),
        )
        .expect("write preview spawn canary config");
        Self {
            command: command_path,
            marker,
        }
    }

    /// Proves the exact configured command writes its marker before a rejection
    /// assertion relies on the marker as a launch canary.
    fn run_positive_control(&self) {
        let status = Command::new(&self.command)
            .arg(&self.marker)
            .status()
            .expect("run preview spawn canary");
        assert!(status.success(), "preview spawn canary failed");
        assert!(
            self.marker.is_file(),
            "preview spawn canary did not create its marker"
        );
        std::fs::remove_file(&self.marker).expect("remove positive-control marker");
    }

    /// Confirms preview rejection did not reach configured-extension launch.
    fn assert_not_launched(&self) {
        assert!(
            !self.marker.exists(),
            "rejected preview launched the configured extension"
        );
    }
}

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

/// Ensures a render subprocess forwards comma-separated profiles to its daemon
/// and lets the later profile override the earlier startup role.
#[test]
fn print_prompt_applies_ordered_profile_stack_in_spawned_harness() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(&config_dir).expect("config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
profiles:
  first:
    agents:
      default_role: first-role
  second:
    agents:
      default_role: second-role
agents:
  role_groups:
    preview:
      roles:
        first-role:
          prompt_fragments:
            - name: preview.first
              priority: 10
              text: FIRST PROFILE PREVIEW
        second-role:
          prompt_fragments:
            - name: preview.second
              priority: 10
              text: SECOND PROFILE PREVIEW
"#,
    )
    .expect("write profile fixture");

    let output = preview(
        &home,
        None,
        &["--profile", "first,second", "dev", "print-prompt"],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    let prompt = String::from_utf8(output.stdout).expect("UTF-8 prompt");
    assert!(
        !prompt.contains("FIRST PROFILE PREVIEW"),
        "earlier profile still selected the startup role: {prompt}"
    );
    assert!(
        prompt.contains("SECOND PROFILE PREVIEW"),
        "later profile did not select its startup role: {prompt}"
    );
}

/// Ensures a whitespace-normalized default profile reaches a spawned harness as
/// one profile rather than becoming absent or a differently parsed selection.
#[test]
fn print_prompt_forwards_normalized_default_profile_to_spawned_harness() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    std::fs::create_dir_all(&config_dir).expect("config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
default_profile: " focused "
profiles:
  focused:
    agents:
      prompt_fragments:
        - name: preview.default
          priority: 10
          text: DEFAULT PROFILE PREVIEW
"#,
    )
    .expect("write default profile fixture");

    let output = preview(&home, None, &["--role", "engineer", "dev", "print-prompt"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("DEFAULT PROFILE PREVIEW"),
        "{:?}",
        output.stdout
    );
}

fn assert_no_runtime_pairs(home: &Path) {
    let harnesses = isolated_runtime_dir(home).join("tau/harnesses");
    if !harnesses.exists() {
        return;
    }
    for directory in ["claims", "sockets"] {
        let path = harnesses.join(directory);
        assert!(
            !path.exists()
                || std::fs::read_dir(path)
                    .expect("harness runtime leaf")
                    .next()
                    .is_none(),
            "preview must not leave a runtime claim or socket"
        );
    }
}

fn assert_no_durable_preview_artifacts(home: &Path) {
    assert_eq!(
        std::fs::read(home.join(".state/tau/agents/seed/events.cbor"))
            .expect("read agent sentinel"),
        b"durable sentinel"
    );
    assert_eq!(
        std::fs::read(home.join(".cache/tau/ext/seed/value")).expect("read cache sentinel"),
        b"cache sentinel"
    );
    let mut agent_entries = std::fs::read_dir(home.join(".state/tau/agents"))
        .expect("read agent root")
        .map(|entry| entry.expect("read agent entry").file_name())
        .collect::<Vec<_>>();
    agent_entries.sort();
    assert_eq!(
        agent_entries,
        [std::ffi::OsString::from("seed")],
        "preview must not create another durable agent tree"
    );
    let sessions = home.join(".state/tau/sessions");
    assert!(
        !sessions.exists()
            || std::fs::read_dir(sessions)
                .expect("read sessions directory")
                .next()
                .is_none(),
        "preview must not create a resumable session"
    );
    assert_no_runtime_pairs(home);
}

/// Confirms rejected preview input leaves no daemon-discovery or persistent
/// session and agent artifacts.
fn assert_rejected_preview_has_no_state(home: &Path) {
    for directory in [".state/tau/agents", ".state/tau/sessions"] {
        let path = home.join(directory);
        assert!(
            !path.exists()
                || std::fs::read_dir(&path)
                    .expect("read rejected preview state directory")
                    .next()
                    .is_none(),
            "rejected preview must not create durable state in {directory}"
        );
    }
    assert_no_runtime_pairs(home);
}

#[cfg(unix)]
fn state_tree_contents(root: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(std::path::PathBuf, Option<Vec<u8>>)>) {
        let mut children = std::fs::read_dir(path)
            .expect("read state tree")
            .map(|entry| entry.expect("read state entry").path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let relative = child
                .strip_prefix(root)
                .expect("state entry remains below root")
                .to_path_buf();
            if child.is_dir() {
                entries.push((relative, None));
                visit(root, &child, entries);
            } else {
                entries.push((
                    relative,
                    Some(std::fs::read(&child).expect("read state file")),
                ));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

#[cfg(unix)]
fn make_tree_read_only(path: &Path) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

    if path.is_dir() {
        for entry in std::fs::read_dir(path).expect("read tree for permission change") {
            make_tree_read_only(&entry.expect("read permission entry").path());
        }
        std::fs::set_permissions(path, Permissions::from_mode(0o555))
            .expect("make directory read-only");
    } else {
        std::fs::set_permissions(path, Permissions::from_mode(0o444)).expect("make file read-only");
    }
}

/// Proves one developer render command avoids agent-store writes and preserves
/// output when durable Tau state is recursively read-only.
#[cfg(unix)]
fn assert_preview_runs_unflagged_without_writable_agent_store(command: &str) {
    let home = TempDir::new().expect("temporary home");
    let state = home.path().join(".state");
    for directory in [
        ".config",
        ".cache",
        ".data",
        ".runtime",
        "work",
        ".state/tau/providers",
        ".state/tau/ext/core-shell",
        ".state/tau/secrets/ext/core-shell",
    ] {
        std::fs::create_dir_all(home.path().join(directory)).expect("create preview root");
    }

    let baseline = preview(&home, None, &["--role", "engineer", "dev", command]);
    assert!(
        baseline.status.success(),
        "{command}: {:?}",
        baseline.stderr
    );
    let agents = state.join("tau/agents");
    if agents.exists() {
        assert!(
            std::fs::read_dir(&agents)
                .expect("read baseline agent root")
                .next()
                .is_none(),
            "baseline preview persisted an agent"
        );
        std::fs::remove_dir(&agents).expect("remove empty baseline agent root");
    }
    let state_before = state_tree_contents(&state);
    make_tree_read_only(&state);

    let unflagged = preview(&home, None, &["--role", "engineer", "dev", command]);
    assert!(
        unflagged.status.success(),
        "{command} without --ephemeral: {:?}",
        unflagged.stderr
    );
    assert_eq!(
        unflagged.stdout, baseline.stdout,
        "{command} output changed"
    );

    let explicit = preview(
        &home,
        None,
        &["--ephemeral", "--role", "engineer", "dev", command],
    );
    assert!(
        explicit.status.success(),
        "{command} with --ephemeral: {:?}",
        explicit.stderr
    );
    assert_eq!(
        explicit.stdout, baseline.stdout,
        "{command} --ephemeral changed output"
    );
    assert_no_runtime_pairs(home.path());
    assert_eq!(
        state_tree_contents(&state),
        state_before,
        "diagnostic commands changed durable state"
    );
}

/// `print-prompt` must remain output-identical without writable durable state.
#[cfg(unix)]
#[test]
fn print_prompt_runs_unflagged_without_writable_agent_store() {
    assert_preview_runs_unflagged_without_writable_agent_store("print-prompt");
}

/// `print-tools` must remain output-identical without writable durable state.
#[cfg(unix)]
#[test]
fn print_tools_runs_unflagged_without_writable_agent_store() {
    assert_preview_runs_unflagged_without_writable_agent_store("print-tools");
}

/// `print-system-prompt` must remain output-identical without writable state.
#[cfg(unix)]
#[test]
fn print_system_prompt_runs_unflagged_without_writable_agent_store() {
    assert_preview_runs_unflagged_without_writable_agent_store("print-system-prompt");
}

/// Render previews do not create durable session/agent artifacts or leak
/// runtime discovery pairs while ordinary configured extensions initialize
/// their roots.
#[test]
fn previews_omit_durable_session_artifacts_across_success_failure_and_concurrency() {
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
    for command in ["print-prompt", "print-tools", "print-system-prompt"] {
        let output = preview(&home, None, &["--role", "engineer", "dev", command]);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_no_durable_preview_artifacts(home.path());
    }

    let failure = preview(
        &home,
        None,
        &["--role", "missing-role", "dev", "print-tools"],
    );
    assert!(!failure.status.success());
    assert_no_durable_preview_artifacts(home.path());

    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for command in ["print-prompt", "print-tools"] {
        let home = home.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            preview_at(&home, None, &["--role", "engineer", "dev", command])
        }));
    }
    barrier.wait();
    for thread in threads {
        let output = thread.join().expect("preview thread");
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    assert_no_durable_preview_artifacts(home.path());
}

/// Required std-pim must complete ordinary Configure storage writes during an
/// ephemeral diagnostic without creating a resumable session directory.
#[test]
fn print_tools_allows_std_pim_extension_data_without_durable_session() {
    let home = TempDir::new().expect("temporary home");
    std::fs::create_dir_all(home.path().join("work")).expect("work directory");

    let output = preview(
        &home,
        Some("std-pim"),
        &["--role", "engineer", "dev", "print-tools"],
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        home.path()
            .join(".state/tau/ext/std-pim/state-v0.json")
            .is_file(),
        "std-pim Configure must receive ordinary writable User storage"
    );
    let sessions = home.path().join(".state/tau/sessions");
    assert!(
        !sessions.exists()
            || std::fs::read_dir(sessions)
                .expect("read sessions directory")
                .next()
                .is_none(),
        "ephemeral diagnostics must not create resumable sessions"
    );
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
        .find("## Available agent roles for `agent_start`")
        .expect("delegate role catalog");
    assert!(
        prompt.find("# Tau harness").expect("harness heading") < catalog_offset,
        "prompt diagnostic must retain late role-catalog placement"
    );
    assert!(
        !prompt.contains("# Agent identity"),
        "built-in prompt diagnostic must omit per-agent identity"
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
/// built-in identity omission.
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
            !String::from_utf8_lossy(&output.stdout).contains("dev-preview-agent"),
            "built-in preview must omit fake agent identity"
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

/// Proves previews without Secret authority do not infer Codex tools from an
/// unusable configured route, while preserving explicit role grants.
#[test]
fn print_tools_requires_usable_model_route_or_explicit_role_grants() {
    let home = TempDir::new().expect("temporary home");
    let config_dir = home.path().join(".config/tau");
    let settings_dir = home.path().join(".state/tau/providers/provider-builtin");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    std::fs::create_dir_all(&settings_dir).expect("create provider settings directory");
    std::fs::create_dir_all(home.path().join("work")).expect("create work directory");
    std::fs::write(
        settings_dir.join("chatgpt.json"),
        r#"{
  "kind": "chatgpt",
  "credential": {
    "kind": "oauth",
    "identity": "0123456789abcdef0123456789abcdef"
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
    "identity": "fedcba9876543210fedcba9876543210"
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
    assert!(!codex_default.contains(&"apply_patch".to_owned()));
    assert!(!codex_default.contains(&"shell_command".to_owned()));
    assert!(codex_default.contains(&"edit".to_owned()));
    assert!(codex_default.contains(&"read".to_owned()));
    assert_eq!(names("codex-explicit"), ["grep", "ls", "read"]);

    let ordinary_default = names("ordinary-default");
    assert!(ordinary_default.contains(&"edit".to_owned()));
    assert!(ordinary_default.contains(&"read".to_owned()));
    assert!(ordinary_default.contains(&"grep".to_owned()));
    assert!(ordinary_default.contains(&"ls".to_owned()));
    assert!(!ordinary_default.contains(&"apply_patch".to_owned()));
    assert!(!ordinary_default.contains(&"shell_command".to_owned()));
}

/// Returns whether one successful tool preview exposes the test-dummy tool.
fn preview_has_test_dummy(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stdout).contains("\"name\": \"restart_test_dummy\"")
}

/// The public extension environment must enable test-dummy idempotently when
/// the same extension name appears more than once.
#[test]
fn print_tools_extension_environment_is_idempotent() {
    let home = TempDir::new().expect("temporary home");
    let env_only = preview(
        &home,
        Some("test-dummy"),
        &["--role", "engineer", "dev", "print-tools"],
    );
    let duplicated = preview(
        &home,
        Some("test-dummy,test-dummy"),
        &["--role", "engineer", "dev", "print-tools"],
    );
    for output in [&env_only, &duplicated] {
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    assert!(preview_has_test_dummy(&env_only));
    assert_eq!(env_only.stdout, duplicated.stdout);
}

/// A later CLI disable must override test-dummy enabled by the public
/// extension environment.
#[test]
fn print_tools_cli_disable_overrides_extension_environment() {
    let home = TempDir::new().expect("temporary home");
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

    assert!(disabled.status.success(), "{:?}", disabled.stderr);
    assert!(!preview_has_test_dummy(&disabled));
}

/// A later CLI enable must override an earlier CLI disable in argv order.
#[test]
fn print_tools_cli_reenable_follows_ordered_disable() {
    let home = TempDir::new().expect("temporary home");
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

    assert!(reenabled.status.success(), "{:?}", reenabled.stderr);
    assert!(preview_has_test_dummy(&reenabled));
}

/// Rejects malformed and unknown public extension input before configured
/// extension launch, leaving no discovery or durable-state artifacts.
#[cfg(unix)]
#[test]
fn previews_reject_invalid_extension_environment_before_startup() {
    for (value, diagnostic) in [
        ("test-dummy,,core-shell", "item 2 is empty"),
        ("not-configured", "unknown extension"),
    ] {
        let home = TempDir::new().expect("temporary home");
        let spawn_canary = PreviewSpawnCanary::configure(&home);
        spawn_canary.run_positive_control();
        let output = preview(
            &home,
            Some(value),
            &["--role", "engineer", "dev", "print-tools"],
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty(), "rejected preview wrote stdout");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("TAU_ENABLE_EXTENSIONS") && stderr.contains(diagnostic),
            "expected {diagnostic:?} extension-environment rejection: {stderr}"
        );
        spawn_canary.assert_not_launched();
        assert_rejected_preview_has_no_state(home.path());
    }
}

/// Rejects non-UTF-8 public extension input before configured extension launch,
/// leaving no discovery or durable-state artifacts.
#[cfg(unix)]
#[test]
fn previews_reject_non_utf8_extension_environment() {
    use std::os::unix::ffi::OsStringExt as _;

    let home = TempDir::new().expect("temporary home");
    let spawn_canary = PreviewSpawnCanary::configure(&home);
    spawn_canary.run_positive_control();
    let output = isolated_tau_command(env!("CARGO_BIN_EXE_tau"), home.path())
        .env(
            "TAU_ENABLE_EXTENSIONS",
            path_std_ffi::OsString::from_vec(vec![0xff]),
        )
        .args(["--role", "engineer", "dev", "print-tools"])
        .output()
        .expect("run tau preview");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "rejected preview wrote stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TAU_ENABLE_EXTENSIONS") && stderr.contains("valid UTF-8"),
        "expected UTF-8 extension-environment rejection: {stderr}"
    );
    spawn_canary.assert_not_launched();
    assert_rejected_preview_has_no_state(home.path());
}
