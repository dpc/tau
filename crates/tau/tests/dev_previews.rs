use std::ffi as path_std_ffi;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn preview(home: &TempDir, environment: Option<&str>, args: &[&str]) -> Output {
    preview_at(home.path(), environment, args)
}

fn preview_at(home: &Path, environment: Option<&str>, args: &[&str]) -> Output {
    let work = home.join("work");
    let runtime = home.join(".runtime");
    std::fs::create_dir_all(&work).expect("create preview cwd");
    std::fs::create_dir_all(&runtime).expect("create preview runtime");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tau"));
    command
        .current_dir(work)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_DATA_HOME", home.join(".data"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("TAU_ENABLE_EXTENSIONS")
        .args(args);
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
    assert_eq!(baseline.stdout, from_environment.stdout);
    assert_eq!(baseline.stdout, cli_disabled.stdout);
    assert_eq!(baseline.stdout, empty_environment.stdout);
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
        let work = home.path().join("work");
        std::fs::create_dir_all(&work).expect("create preview cwd");
        let output = Command::new(env!("CARGO_BIN_EXE_tau"))
            .current_dir(work)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_STATE_HOME", home.path().join(".state"))
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
