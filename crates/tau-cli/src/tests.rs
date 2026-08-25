use std::collections::HashSet;
use std::io::BufReader;
use std::os::unix as path_std_os_unix;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{ffi as path_std_ffi, fs as path_std_fs, sync as path_std_sync, time as path_std_time};

use clap::{CommandFactory as _, Parser};
use tau_cli_term::TermHandle;
use tau_cli_term_raw::{Color, Term};
use tau_config::settings as path_tau_config_settings;
use tau_proto::{
    AgentCompacted, AgentCompactionTriggered, AgentManualCompactionRequested, AgentPromptCreated,
    AgentPromptFailed, AgentPromptQueued, AgentPromptRejected, AgentPromptSteered,
    AgentPromptSubmitted, AgentPromptTerminated, AgentPromptTerminationReason,
    AgentStandaloneCompactionFailed, AgentStandaloneCompactionStarted, CborValue, ContentPart,
    ContextItem, ContextRole, Effort, Event, ExtensionReady, HarnessContextUsageChanged,
    HarnessRoleInfo, HarnessRoleSelected, HarnessRolesAvailable, HarnessSessionDir, MessageItem,
    OpaqueProviderItem, ProviderResponseFinished, ProviderResponseUpdated, ProviderStopReason,
    ServiceTier, SessionDirStatus, SessionStartReason, SessionStarted, ThinkingSummary,
    ToolBackgroundResult, ToolCallItem, ToolCancelled, ToolError, ToolResult, UiPromptSubmitted,
    UiRoleUpdateAction, Verbosity,
};

use super::agent_navigation::AgentNavigationState;
use super::chat::cold_attach_stager::ShellStartPresentation;
use super::chat::{
    DraftSlot, UiIoMeter, UiWriter, custom_prompt_replacement, debounce_loop_with_wait,
    invalidate_pending_draft, is_known_static_command, leading_command_token,
    next_agent_cycle_selection, queue_prompt_draft_snapshot, redacted_command_echo_line,
    redacted_prompt_history_line, retarget_prompt_draft_snapshot, role_cycling_enabled,
    send_draft_snapshot_with_before_writer, should_send_draft_snapshot,
    terminal_options_from_settings,
};
use super::cli::{Command as CliCommand, DevCommand};
use super::event_renderer::{EventRenderer, WatchedAgentActivity, watched_agent_tool_display};
use super::tool_render::format_context_token_count;
use super::{CliError, cli as path_super_cli, transcript_markers};

/// Foreground-ownership fail-stop must bypass generic stderr reporting because
/// the process cannot confirm that it owns the terminal.
#[test]
fn foreground_ownership_failure_suppresses_top_level_terminal_report() {
    let error = CliError::ForegroundOwnershipUnconfirmed("restore failed".to_owned());

    assert!(!error.should_report_to_terminal());
}

/// Ensures only commands that launch a harness or render from harness settings
/// validate a fresh snapshot. Attach and local/IPC inspection must remain
/// usable after an already-running daemon's source configuration becomes
/// invalid.
#[test]
fn harness_settings_validation_is_limited_to_config_consumers() {
    let run_args = || path_super_cli::RunArgs {
        config: None,
        prompt_stdin: false,
        ephemeral: false,
    };
    assert!(!super::consumes_harness_settings(
        &super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::StartupMode::Attach(None),
        }
    ));
    assert!(super::consumes_harness_settings(
        &super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::StartupMode::New,
        }
    ));
    assert!(super::consumes_harness_settings(
        &super::DispatchCommand::Startup {
            args: run_args(),
            mode: super::StartupMode::Resume(None),
        }
    ));
    assert!(!super::consumes_harness_settings(
        &super::DispatchCommand::Other(path_super_cli::Command::Session {
            command: path_super_cli::SessionCommand::List(Default::default()),
        })
    ));
    assert!(!super::consumes_harness_settings(
        &super::DispatchCommand::Other(path_super_cli::Command::Dev {
            command: path_super_cli::DevCommand::Send {
                session_id: "s1".to_owned(),
                line: vec!["hello".to_owned()],
            },
        })
    ));
    assert!(super::consumes_harness_settings(
        &super::DispatchCommand::Other(path_super_cli::Command::Dev {
            command: path_super_cli::DevCommand::PrintSystemPrompt,
        })
    ));
}

/// Static CLI mouse configuration must reach the terminal layer unchanged so
/// the raw terminal can select its ownership-safe capture behavior once.
#[test]
fn static_mouse_setting_propagates_to_terminal_options() {
    let mut settings = path_tau_config_settings::CliSettings::built_in();
    settings.mouse = false;

    assert_eq!(
        terminal_options_from_settings(&settings),
        tau_cli_term::TerminalOptions {
            cursor_shape: tau_cli_term::CursorShape::Bar,
            mouse: false,
        }
    );
}

/// Dynamic action IDs use the bounded ASCII short-ID producer shape accepted by
/// the protocol type.
#[test]
fn action_short_id_producer_stays_within_invocation_id_grammar() {
    for _ in 0..100 {
        let raw = super::mint_short_id("action");
        let id = tau_proto::ActionInvocationId::parse(raw.clone())
            .expect("generated action invocation id");
        assert_eq!(id.as_str(), raw);
        assert_eq!(id.as_str().len(), 13);
        assert!(id.as_str().starts_with("action-"));
        assert!(
            id.as_str()[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        );
    }
}

/// Context-size magnitudes retain useful precision at small scales, round
/// half-up at each width boundary, and promote instead of growing past three
/// numeric columns.
#[test]
fn context_size_formatter_rounds_and_promotes_within_three_numeric_columns() {
    for (tokens, expected) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1.0k"),
        (4_600, "4.6k"),
        (9_949, "9.9k"),
        (9_950, "10k"),
        (99_499, "99k"),
        (99_500, "100k"),
        (353_400, "353k"),
        (999_499, "999k"),
        (999_500, "1.0m"),
    ] {
        assert_eq!(format_context_token_count(tokens), expected);
    }

    for (unit, suffix, next_suffix) in [
        (1_000_000, "m", "b"),
        (1_000_000_000, "b", "t"),
        (1_000_000_000_000, "t", "q"),
    ] {
        let small_boundary = 10 * unit - unit / 20;
        let medium_boundary = 100 * unit - unit / 2;
        let promotion_boundary = 1_000 * unit - unit / 2;
        assert_eq!(
            format_context_token_count(small_boundary),
            format!("10{suffix}")
        );
        assert_eq!(
            format_context_token_count(medium_boundary),
            format!("100{suffix}")
        );
        assert_eq!(
            format_context_token_count(promotion_boundary),
            format!("1.0{next_suffix}")
        );
    }

    for tokens in [
        1_000,
        9_950,
        99_500,
        999_500,
        9_950_000,
        99_500_000,
        999_500_000,
        u64::MAX,
    ] {
        let rendered = format_context_token_count(tokens);
        let numeric = rendered.trim_end_matches(|character: char| character.is_ascii_alphabetic());
        assert!(numeric.chars().count() <= 3, "{rendered}");
    }
}

/// Returns the stable inline theme shared by CLI renderer tests.
pub(crate) fn cli_test_theme() -> tau_themes::Theme {
    tau_themes::Theme::parse(
        r##"
        {
            styles: {
                "tool.mode": { fg: "yellow" },
                "watching.name": { fg: "dark_yellow" },
                "tool.status.success": { fg: "green" },
                "tool.status.error": { fg: "red" },
                "tool.status.info": { fg: "dark_cyan" },
                "system.info": { fg: "blue" },
                "system.internal_notice": { fg: "blue", italic: true },
                "system.info.important": { fg: "red" },
                "status.agents": { fg: "cyan" },
                "diff.added": { fg: "dark_green" },
                "diff.removed": { fg: "dark_red" },
                "diff.added.inline": { fg: "green", bold: true },
                "diff.removed.inline": { fg: "red", bold: true },
                "action.label": { fg: "dark_grey" },
                "action.id": { fg: "yellow", bold: true },
                "action.error": { fg: "red" },
                "agent.message.identity": { bold: true },
                "token.stats": { fg: "dark_grey" },
                "token.stats.symbol.delta": { bold: true },
                "token.stats.symbol.sigma": { bold: true },
                "token.stats.metric.cache_warn": { fg: "dark_yellow" },
                "token.stats.metric.cache_miss": { fg: "red" },
                "markdown.strong": { fg: "red", bold: true },
                "markdown.code": { fg: "green" },
            }
        }
        "##,
    )
    .expect("CLI test theme parses")
}

/// Build a renderer with the built-in lifecycle markers rather than the terse
/// legacy test markers used by [`EventRenderer::new`].
fn marker_test_renderer(handle: TermHandle) -> EventRenderer {
    EventRenderer::new_with_state(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
        path_tau_config_settings::CliState::default(),
        path_tau_config_settings::TauDirs::default(),
        "◯".to_owned(),
        "⬤".to_owned(),
    )
}

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("valid test agent id")
}

fn test_session_id(value: impl Into<String>) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("test session id")
}

fn test_agent_prompt_id(value: impl Into<String>) -> tau_proto::AgentPromptId {
    tau_proto::AgentPromptId::parse(value).expect("test agent prompt id")
}

/// Returns the adaptive header cells selected for a tool block at `width`.
fn priority_header_cells(
    block: &tau_cli_term::StyledBlock,
    width: usize,
) -> Vec<tau_cli_term::Cell> {
    block
        .priority_line_content()
        .expect("priority header")
        .layout(width)
}

/// Returns the plain adaptive header selected for a tool block at `width`.
fn priority_header_text(block: &tau_cli_term::StyledBlock, width: usize) -> String {
    priority_header_cells(block, width)
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

use super::tool_render::{
    CompactionStatus, ToolLineElement, ToolStatus, build_delegate_completion_display,
    format_turn_stats_line, render_action_error_block, render_action_output_block,
    render_compaction_block, render_diff_tool_block, render_harness_notice,
    render_multi_diff_tool_block, render_shell_block, render_tool_block, render_tool_use_state,
    render_turn_stats_block, streaming_block, synthesize_fallback_display,
};

/// Ensures `--profile` keeps its root-owned ordered selection syntax through
/// command parsing before startup forwards it to the harness.
#[test]
fn profile_flag_parses_as_root_harness_selection() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--profile",
        "focused,review",
        "--role",
        "engineer",
        "dev",
        "print-prompt",
    ]);
    assert_eq!(cli.harness.profile.as_deref(), Some("focused,review"));
    assert_eq!(cli.harness.role.as_deref(), Some("engineer"));
}

/// Ensures the root run command accepts `--ephemeral` as an explicit
/// session-persistence mode without affecting the separate attach/resume flags.
#[test]
fn run_parses_ephemeral_flag() {
    let cli = path_super_cli::Cli::parse_from(["tau", "--ephemeral"]);
    assert!(cli.run.ephemeral);
    assert!(cli.command.is_none());
}

/// Prevents `--ephemeral` from becoming a misleading modifier for an already
/// running or persisted session, where the new process cannot guarantee a clean
/// session-persistence boundary.
#[test]
fn run_rejects_ephemeral_with_attach_or_resume() {
    assert!(super::reject_ephemeral_incompatible(true, &super::StartupMode::Attach(None)).is_err());
    assert!(super::reject_ephemeral_incompatible(true, &super::StartupMode::Resume(None)).is_err());
    assert!(super::reject_ephemeral_incompatible(true, &super::StartupMode::New).is_ok());
}

#[test]
fn dev_print_prompt_accepts_agents_md_toggle() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "print-prompt",
        "--enable-agents-md",
        "false",
    ]);
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintPrompt {
                enable_agents_md: false,
            },
        })
    ));
}

/// Ensures the hidden tmux helper parses isolated scratch/workdir startup
/// options, because future manual E2E sessions must not accidentally inherit
/// the user's real Tau state.
#[test]
fn dev_tmux_start_parses_isolated_startup_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "start",
        "--scratch-root",
        "/tmp/tau-e2e-test",
        "--session",
        "manual",
        "--tau-bin",
        "target/debug/tau",
        "--workdir",
        "/tmp/tau-e2e-test/work",
        "--width",
        "100",
        "--height",
        "30",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::Tmux {
                command: super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
            && args.common.session == "manual"
            && args.tau_bin == Some(std::path::PathBuf::from("target/debug/tau"))
            && args.workdir == Some(std::path::PathBuf::from("/tmp/tau-e2e-test/work"))
            && args.width == 100
            && args.height == 30
    ));
}

/// Ensures `start` can omit its scratch root so the helper can generate a fresh
/// temporary root, and accepts the shorter `--root` spelling when a caller
/// needs to select the root explicitly.
#[test]
fn dev_tmux_start_accepts_generated_root_and_root_alias() {
    let generated = path_super_cli::Cli::parse_from(["tau", "dev", "tmux", "start"]);
    assert!(matches!(
        generated.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::Tmux {
                command: super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root.is_none()
    ));

    let explicit = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "start",
        "--root",
        "/tmp/tau-e2e-test",
    ]);
    assert!(matches!(
        explicit.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::Tmux {
                command: super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
    ));
}

/// Ensures `send` keeps prompt text as a trailing argument vector and exposes a
/// no-enter mode, protecting the manual workflow's ability to paste slash
/// commands or partial prompts before submitting them.
#[test]
fn dev_tmux_send_parses_literal_text_and_enter_toggle() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "send",
        "--scratch-root",
        "/tmp/tau-e2e-test",
        "--no-enter",
        "--",
        ":version",
        "with spaces",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::Tmux {
                command: super::cli::DevTmuxCommand::Send(args),
            },
        }) if args.target.common.scratch_root == Some(std::path::PathBuf::from("/tmp/tau-e2e-test"))
            && args.no_enter
            && args.text == vec![":version".to_owned(), "with spaces".to_owned()]
    ));
}

/// Public agent-list flags parse additively around one required session id.
#[test]
fn list_agents_command_parses_filters() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "list",
        "session-1",
        "--include-suspended",
        "--include-unloaded",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Agent {
            command: super::cli::AgentCommand::List(args),
        })
            if args.session_id == "session-1"
                && args.include_suspended
                && args.include_unloaded
                && !args.include_unavailable
                && !args.all
    ));
}

/// Agent trace defaults select the compact TOON lite overview and the ordinary
/// state-directory journal root.
#[test]
fn agent_trace_command_parses_defaults() {
    let cli = path_super_cli::Cli::parse_from(["tau", "agent", "trace", "agent-root"]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Agent {
            command: super::cli::AgentCommand::Trace(args),
        })
            if args.agent_id.as_str() == "agent-root"
                && !args.include_descendants
                && args.format == super::cli::AgentTraceFormat::AgentToolsToon
                && args.mode == super::cli::AgentTraceMode::Lite
                && args.agents_dir == tau_session_inspect::default_agents_dir()
    ));
}

/// Agent-trace help must advertise the compact-overview semantics and both
/// default values so generated help cannot drift from parser behavior.
#[test]
fn agent_trace_help_shows_compact_toon_lite_defaults() {
    let command = path_super_cli::Cli::command();
    let mut trace = command
        .find_subcommand("agent")
        .and_then(|agent| agent.find_subcommand("trace"))
        .expect("agent trace command")
        .clone();
    let help = trace.render_long_help().to_string();

    assert!(help.contains("Project a validated durable agent snapshot"));
    assert!(help.contains("[default: agent-tools-toon]"));
    assert!(help.contains("[default: lite]"));
    assert!(help.contains("at most 4 KiB of each text/output item"));
    assert!(help.contains("complete semantic text/normalized output"));
}

/// Agent trace accepts the lossy OTLP adapter, descendant workflow inclusion,
/// and an explicit offline journal root together.
#[test]
fn agent_trace_command_parses_all_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--include-descendants",
        "--format",
        "otlp-json",
        "--mode",
        "lite",
        "--agents-dir",
        "/tmp/agents",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Agent {
            command: super::cli::AgentCommand::Trace(args),
        })
            if args.include_descendants
                && args.format == super::cli::AgentTraceFormat::OtlpJson
                && args.mode == super::cli::AgentTraceMode::Lite
                && args.agents_dir == std::path::Path::new("/tmp/agents")
    ));
}

/// Agent trace accepts both compact encodings, defaults their detail to lite,
/// and parses explicit full detail independently from encoding.
#[test]
fn agent_trace_command_parses_compact_formats_and_modes() {
    for (name, expected) in [
        (
            "agent-tools-jsonl",
            path_super_cli::AgentTraceFormat::AgentToolsJsonl,
        ),
        (
            "agent-tools-toon",
            path_super_cli::AgentTraceFormat::AgentToolsToon,
        ),
    ] {
        let cli = path_super_cli::Cli::parse_from([
            "tau",
            "agent",
            "trace",
            "agent-root",
            "--format",
            name,
        ]);

        assert!(matches!(
            cli.command,
            Some(super::cli::Command::Agent {
                command: super::cli::AgentCommand::Trace(args),
            }) if args.format == expected && args.mode == super::cli::AgentTraceMode::Lite
        ));
    }

    let full = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--format",
        "agent-tools-toon",
        "--mode",
        "full",
    ]);
    assert!(matches!(
        full.command,
        Some(super::cli::Command::Agent {
            command: super::cli::AgentCommand::Trace(args),
        }) if args.format == super::cli::AgentTraceFormat::AgentToolsToon
            && args.mode == super::cli::AgentTraceMode::Full
    ));
}

/// Dispatch validation rejects full detail for every noncompact encoding with
/// the tailored diagnostic while accepting both compact encodings.
#[test]
fn agent_trace_mode_validation_matches_format_semantics() {
    for format in [
        path_super_cli::AgentTraceFormat::TauJsonl,
        path_super_cli::AgentTraceFormat::OtlpJson,
        path_super_cli::AgentTraceFormat::AgentPerformanceJsonl,
    ] {
        let error = super::validate_agent_trace_mode(format, path_super_cli::AgentTraceMode::Full)
            .expect_err("full noncompact trace");
        assert_eq!(
            error.to_string(),
            "participant error: `agent trace --mode full` requires `--format \
             agent-tools-jsonl` or `--format agent-tools-toon`"
        );
    }
    for format in [
        path_super_cli::AgentTraceFormat::AgentToolsJsonl,
        path_super_cli::AgentTraceFormat::AgentToolsToon,
    ] {
        super::validate_agent_trace_mode(format, path_super_cli::AgentTraceMode::Full)
            .expect("full compact trace");
    }
}

/// Agent trace exposes the content-free performance JSONL projection without a
/// payload-detail mode.
#[test]
fn agent_trace_command_parses_performance_format() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "agent",
        "trace",
        "agent-root",
        "--format",
        "agent-performance-jsonl",
    ]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Agent {
            command: super::cli::AgentCommand::Trace(args),
        }) if args.format == super::cli::AgentTraceFormat::AgentPerformanceJsonl
            && args.mode == super::cli::AgentTraceMode::Lite
    ));
}

/// Session inspection operations share the same noun-first nested command shape
/// as agent inspection.
#[test]
fn session_commands_parse_nested_operations() {
    let list = path_super_cli::Cli::parse_from(["tau", "session", "list"]);
    assert!(matches!(
        list.command,
        Some(super::cli::Command::Session {
            command: super::cli::SessionCommand::List(args),
        })
            if args.dir.is_none() && !args.json
    ));

    let show = path_super_cli::Cli::parse_from(["tau", "session", "show", "--session-id", "s1"]);
    assert!(matches!(
        show.command,
        Some(super::cli::Command::Session {
            command: super::cli::SessionCommand::Show { session_id, .. },
        }) if session_id.as_str() == "s1"
    ));
}

/// Session inspection rejects controlled identifiers during argument parsing,
/// before either command can inspect or create a missing sessions root.
#[test]
fn session_show_and_stats_reject_invalid_ids_with_missing_roots() {
    for command in ["show", "stats"] {
        let id_flag = if command == "show" {
            "--session-id"
        } else {
            "--session"
        };
        let error = match path_super_cli::Cli::try_parse_from([
            "tau",
            "session",
            command,
            id_flag,
            "bad.id",
            "--sessions-dir",
            "/definitely/missing/tau-sessions",
        ]) {
            Ok(_) => panic!("invalid session id must be rejected"),
            Err(error) => error,
        };
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("bad.id"));
        assert!(diagnostic.contains("session id contains invalid byte"));
    }
}

/// Session-list filtering canonicalizes symlink aliases during parsing and
/// composes independently with structured output.
#[test]
fn session_list_parses_canonical_directory_and_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&project).expect("project directory");
    path_std_os_unix::fs::symlink(&project, &alias).expect("project symlink");

    let cli = path_super_cli::Cli::try_parse_from([
        path_std_ffi::OsStr::new("tau"),
        path_std_ffi::OsStr::new("session"),
        path_std_ffi::OsStr::new("list"),
        path_std_ffi::OsStr::new("--dir"),
        alias.as_os_str(),
        path_std_ffi::OsStr::new("--json"),
    ])
    .expect("session list arguments");

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Session {
            command: super::cli::SessionCommand::List(args),
        }) if args.dir.as_deref() == Some(project.as_path()) && args.json
    ));
}

/// Missing paths and non-directory paths fail in clap's exit-2 value-validation
/// path instead of becoming successful empty list filters.
#[test]
fn session_list_rejects_invalid_directories_during_parsing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("file");
    std::fs::write(&file, b"not a directory").expect("test file");
    for invalid in [temp.path().join("missing"), file] {
        let error = match path_super_cli::Cli::try_parse_from([
            path_std_ffi::OsStr::new("tau"),
            path_std_ffi::OsStr::new("session"),
            path_std_ffi::OsStr::new("list"),
            path_std_ffi::OsStr::new("--dir"),
            invalid.as_os_str(),
        ]) {
            Ok(_) => panic!("invalid directory should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
    }
}

/// An inaccessible directory is an invalid filter rather than an apparent
/// successful absence result.
#[test]
fn session_list_rejects_inaccessible_directory_during_parsing() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let private = temp.path().join("private");
    let directory = private.join("project");
    std::fs::create_dir_all(&directory).expect("private project");
    std::fs::set_permissions(&private, path_std_fs::Permissions::from_mode(0o000))
        .expect("remove directory access");

    let result = path_super_cli::Cli::try_parse_from([
        path_std_ffi::OsStr::new("tau"),
        path_std_ffi::OsStr::new("session"),
        path_std_ffi::OsStr::new("list"),
        path_std_ffi::OsStr::new("--dir"),
        directory.as_os_str(),
    ]);

    std::fs::set_permissions(&private, path_std_fs::Permissions::from_mode(0o700))
        .expect("restore directory access");
    let error = match result {
        Ok(_) => panic!("inaccessible directory should be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    assert_eq!(error.exit_code(), 2);
}

/// Tool starts carry the owning agent id, so hidden-agent tools must be routed
/// away from the visible transcript even before provider output maps the call.
#[test]
fn renderer_learns_agent_from_tool_started_event() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let event = Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "hidden-tool".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: CborValue::Null,
        agent_id: agent_id("agent-b"),
        originator: tau_proto::PromptOriginator::User,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&event).as_deref(),
        Some("agent-b")
    );

    renderer.handle(&event);

    assert_eq!(
        renderer.tool_agent_for_test("hidden-tool").as_deref(),
        Some("agent-b")
    );
    assert!(
        renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .contains(&"agent-b".to_owned())
    );
}

#[test]
fn prompt_stdin_flag_is_parsed_for_default_run() {
    // `--prompt-stdin` keeps the normal harness/session args but replaces the
    // terminal UI with the one-shot stdin client.
    let cli = path_super_cli::Cli::parse_from(["tau", "--role", "manager", "--prompt-stdin"]);

    assert!(cli.run.prompt_stdin);
    assert_eq!(cli.harness.role.as_deref(), Some("manager"));
}

#[test]
fn harness_config_flags_parse_repeated_and_global() {
    let overrides = super::parse_harness_config_cli_overrides([
        "tau",
        "--harness-config=extensions.core-shell.config.working_directory=/foo",
        "dev",
        "print-prompt",
        "--harness-config=session_retention_days=3",
    ])
    .expect("parse overrides");

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::HarnessConfigCliOverride {
                key: "extensions.core-shell.config.working_directory".to_owned(),
                raw_value: "/foo".to_owned(),
            },
            tau_config::settings::HarnessConfigCliOverride {
                key: "session_retention_days".to_owned(),
                raw_value: "3".to_owned(),
            },
        ]
    );
}
#[test]
fn harness_config_overrides_reject_attach_only_paths() {
    let overrides = [tau_config::settings::HarnessConfigCliOverride {
        key: "session_retention_days".to_owned(),
        raw_value: "3".to_owned(),
    }];

    let err = super::reject_harness_config_overrides(&overrides, "attach")
        .expect_err("attach cannot apply overrides");
    assert!(err.to_string().contains("starting a new harness instance"));
}

/// Alias rejection preserves the exact dedicated flag or environment source
/// instead of misdiagnosing it as a generic harness-config override.
#[test]
fn alias_inputs_reject_non_startup_commands_with_source_identity() {
    let cases = [
        ((true, false, false, false), "--provider-alias"),
        ((false, true, false, false), "--model-alias"),
        (
            (false, false, true, false),
            tau_config::settings::TAU_PROVIDER_ALIASES_ENV,
        ),
        (
            (false, false, false, true),
            tau_config::settings::TAU_MODEL_ALIASES_ENV,
        ),
    ];
    for ((provider_flag, model_flag, provider_env, model_env), expected) in cases {
        let error = super::reject_model_reference_alias_inputs(
            super::ModelReferenceAliasInputPresence {
                provider_flag,
                model_flag,
                provider_environment: provider_env,
                model_environment: model_env,
            },
            "attach",
        )
        .expect_err("non-startup command must reject alias input");
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("--harness-config"), "{error}");
    }
}

/// The dump-initial-prompt dispatch is an internal validation path, not a
/// startup consumer, so dedicated aliases must be rejected before generic
/// override folding can erase their source identity.
#[test]
fn dump_initial_prompt_dispatch_rejects_alias_inputs() {
    let command = super::DispatchCommand::Other(CliCommand::Dev {
        command: DevCommand::DumpInitialPrompt {
            out: "prompt.txt".into(),
            message: "hello".to_owned(),
        },
    });
    assert!(super::rejects_model_reference_alias_inputs(&command));
    let error = super::reject_model_reference_alias_inputs(
        super::ModelReferenceAliasInputPresence {
            model_flag: true,
            ..Default::default()
        },
        "dev dump-initial-prompt",
    )
    .expect_err("dump path must reject aliases");
    assert!(error.to_string().contains("--model-alias"), "{error}");
}

/// The legacy `--config` flag must not be silently ignored because that makes
/// harness startup appear to use a config file that was never loaded.
#[test]
fn legacy_config_path_is_rejected() {
    let cli = path_super_cli::Cli::parse_from(["tau", "--config", "legacy.json"]);
    let err = super::reject_legacy_config_path(cli.run.config.as_deref())
        .expect_err("legacy config path should fail");

    assert!(err.to_string().contains("--config is no longer supported"));

    let non_run_cli =
        path_super_cli::Cli::parse_from(["tau", "--config", "legacy.json", "session", "list"]);
    let non_run_err = super::reject_legacy_config_path(non_run_cli.run.config.as_deref())
        .expect_err("legacy config path should fail before non-run dispatch");
    assert!(
        non_run_err
            .to_string()
            .contains("--config is no longer supported")
    );

    let explicit_run_cli =
        path_super_cli::Cli::parse_from(["tau", "run", "--config", "legacy.json"]);
    let Some(path_super_cli::Command::Run(explicit_run)) = explicit_run_cli.command else {
        panic!("expected explicit run command");
    };
    let explicit_run_err = super::reject_legacy_config_path(explicit_run.config.as_deref())
        .expect_err("legacy config path should fail before explicit run dispatch");
    assert!(
        explicit_run_err
            .to_string()
            .contains("--config is no longer supported")
    );
}

/// Attach mode connects to an existing daemon, so explicit startup
/// role/extension/profile overrides must fail instead of pretending to
/// reconfigure that daemon.
#[test]
fn attach_rejects_startup_overrides_that_existing_daemon_cannot_apply() {
    let role_overrides = [path_tau_config_settings::RoleCliOverride::Enable(
        "manager".to_owned(),
    )];
    let extension_overrides = [path_tau_config_settings::ExtensionCliOverride::Disable(
        "core-shell".to_owned(),
    )];

    let role_err = super::reject_attach_startup_overrides(false, false, Some("manager"), &[], &[])
        .expect_err("interactive attach role should fail");
    assert!(role_err.to_string().contains("cannot apply --role"));

    let role_override_err =
        super::reject_attach_startup_overrides(false, false, None, &role_overrides, &[])
            .expect_err("attach role overrides should fail");
    assert!(
        role_override_err
            .to_string()
            .contains("role enable/disable")
    );

    let extension_override_err =
        super::reject_attach_startup_overrides(false, false, None, &[], &extension_overrides)
            .expect_err("attach extension overrides should fail");
    assert!(
        extension_override_err
            .to_string()
            .contains("extension enable/disable")
    );

    super::reject_attach_startup_overrides(true, false, Some("manager"), &[], &[])
        .expect("prompt-stdin uses --role for the submitted prompt");

    let profile_error = super::reject_attach_startup_overrides(false, true, None, &[], &[])
        .expect_err("attach cannot apply explicitly selected profile");
    assert!(profile_error.to_string().contains("cannot apply --profile"));
}

#[test]
fn harness_config_flag_requires_key_value() {
    let err = match path_super_cli::Cli::try_parse_from(["tau", "--harness-config=missing-equals"])
    {
        Ok(_) => panic!("missing KEY=VALUE must fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("expected KEY=VALUE"));
}

#[test]
fn global_harness_flags_parse_before_dev_print_prompt() {
    // Hidden diagnostic commands use the same global harness args as normal
    // startup, including flags placed before the `dev` subcommand.
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--disable-roles-all",
        "--role",
        "manager",
        "dev",
        "print-prompt",
    ]);

    assert_eq!(cli.harness.role_overrides.disable_roles_all, 1);
    assert_eq!(cli.harness.role.as_deref(), Some("manager"));
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintPrompt {
                enable_agents_md: true
            },
        })
    ));
}

#[test]
fn role_cli_flags_accept_repeated_and_mixed_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--disable-roles-all",
        "--enable-role",
        "manager",
        "--disable-role",
        "engineer",
        "--disable-roles-all",
    ]);

    assert_eq!(cli.harness.role_overrides.disable_roles_all, 2);
    assert_eq!(cli.harness.role_overrides.enable_role, vec!["manager"]);
    assert_eq!(cli.harness.role_overrides.disable_role, vec!["engineer"]);
}

#[test]
fn extension_cli_flags_accept_repeated_and_mixed_options() {
    let cli = path_super_cli::Cli::parse_from([
        "tau",
        "--enable-extensions-all",
        "--disable-extension",
        "core-shell",
        "--enable-extension",
        "std-websearch",
        "--disable-extensions-all",
    ]);

    assert_eq!(cli.harness.extension_overrides.enable_extensions_all, 1);
    assert_eq!(cli.harness.extension_overrides.disable_extensions_all, 1);
    assert_eq!(
        cli.harness.extension_overrides.enable_extension,
        vec!["std-websearch"]
    );
    assert_eq!(
        cli.harness.extension_overrides.disable_extension,
        vec!["core-shell"]
    );
}

#[test]
fn role_cli_overrides_preserve_argument_order() {
    let overrides = super::parse_role_cli_overrides([
        "tau",
        "--disable-role",
        "manager",
        "--disable-roles-all",
        "--enable-role=manager",
        "--enable-role",
        "engineer",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::RoleCliOverride::Disable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::DisableAll,
            tau_config::settings::RoleCliOverride::Enable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::Enable("engineer".to_owned()),
        ]
    );
}

#[test]
fn extension_cli_overrides_preserve_argument_order() {
    let overrides = super::parse_extension_cli_overrides([
        "tau",
        "--disable-extension",
        "core-shell",
        "--enable-extensions-all",
        "--disable-extensions-all",
        "--enable-extension=std-websearch",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::ExtensionCliOverride::Disable("core-shell".to_owned()),
            tau_config::settings::ExtensionCliOverride::EnableAll,
            tau_config::settings::ExtensionCliOverride::DisableAll,
            tau_config::settings::ExtensionCliOverride::Enable("std-websearch".to_owned()),
        ]
    );
}

/// Ensures the supported environment list is documented in long help together
/// with its grammar and precedence relative to CLI overrides.
#[test]
fn long_help_documents_extension_environment() {
    use clap::CommandFactory;
    let mut output = Vec::new();
    path_super_cli::Cli::command()
        .write_long_help(&mut output)
        .expect("render long help");
    let help = String::from_utf8(output).expect("help is UTF-8");
    assert!(help.contains("TAU_ENABLE_EXTENSIONS=NAME[,NAME...]"));
    assert!(help.contains("CLI enable/disable flags win"));
}

/// Proves the outer `tau dev tmux` dispatcher refuses startup overrides that
/// would require normal harness configuration validation before the helper has
/// switched into its scratch HOME/XDG environment.
#[test]
fn dev_tmux_rejects_startup_overrides_before_harness_validation() {
    let role_error = super::reject_dev_tmux_startup_overrides(None, Some("manager"), &[], &[], &[])
        .expect_err("--role refused");
    assert!(role_error.to_string().contains("cannot use --role"));

    let extension_error = super::reject_dev_tmux_startup_overrides(
        None,
        None,
        &[],
        &[path_tau_config_settings::ExtensionCliOverride::DisableAll],
        &[],
    )
    .expect_err("extension override refused");
    assert!(
        extension_error
            .to_string()
            .contains("cannot use extension enable/disable overrides")
    );
}

/// Ensures `:prompt <id>` resolves a configured template to editable prompt
/// text rather than submitting it immediately.
#[test]
fn custom_prompt_command_returns_configured_prompt_text() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let replacement = custom_prompt_replacement(":prompt review", &prompts)
        .expect("prompt command")
        .expect("known prompt");

    assert_eq!(replacement, "Review this patch carefully");
}

/// Ensures unknown `:prompt` ids produce a clear local error and list
/// configured ids so users can recover without accidentally submitting the
/// command text.
#[test]
fn custom_prompt_command_reports_unknown_id() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let error = custom_prompt_replacement(":prompt missing", &prompts)
        .expect("prompt command")
        .expect_err("unknown prompt should fail");

    assert!(error.contains("unknown custom prompt `missing`"));
    assert!(error.contains("available: review"));
}

/// Ensures the CLI uses the running harness announcement as the custom-prompt
/// source of truth, which keeps reattached UIs aligned with daemon startup
/// overrides instead of re-reading local config files.
#[test]
fn renderer_tracks_custom_prompts_from_harness_event() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let prompt = tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    };

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: Vec::new(),
        groups: Vec::new(),
        custom_prompts: vec![prompt.clone()],
    }));

    let prompts = renderer.custom_prompts().lock().expect("prompts").clone();
    assert_eq!(prompts, vec![prompt]);
}

/// Ensures `:prompt` remains a local command for command echo/history
/// routing and does not fall through as a normal user prompt.
#[test]
fn prompt_command_is_known_static_command() {
    assert!(is_known_static_command(":prompt review"));
}

/// Protects the final command ownership fallback recorded by
/// `SPEC-tau-cli-command-mode`: a likely mistyped leading command
/// must not become a normal prompt, while non-leading slashes remain prompt
/// text.
#[test]
fn leading_command_tokens_are_identified_before_prompt_submission() {
    assert_eq!(leading_command_token(":typo"), Some(":typo"));
    assert_eq!(leading_command_token("  :typo arg"), Some(":typo"));
    assert_eq!(
        leading_command_token(":skill:jujutsu args"),
        Some(":skill:jujutsu")
    );
    assert_eq!(leading_command_token("hello /typo"), None);
    assert_eq!(leading_command_token("please inspect /tmp/file"), None);
    assert_eq!(leading_command_token("./relative/path"), None);
}

/// Classifies only the static commands rendered locally by the interactive CLI.
#[test]
fn known_static_commands_are_identified_for_history_rendering() {
    assert!(is_known_static_command(":model engineer"));
    assert!(is_known_static_command(":set show-tools compact"));
    assert!(is_known_static_command(":theme dpc"));
    assert!(is_known_static_command(":session-stats"));
    assert!(is_known_static_command(":debug-show-ui-event-stats"));
    assert!(is_known_static_command(":debug-show-event-stats std-shell"));
    assert!(is_known_static_command(":quit"));
    assert!(is_known_static_command(":agent"));
    assert!(is_known_static_command(":agent switch worker-1"));
    assert!(is_known_static_command(":agent suspend"));
    assert!(is_known_static_command(":agent resume worker-1"));
    assert!(is_known_static_command(":agent new"));
    assert!(is_known_static_command(":new"));
    assert!(is_known_static_command(":name Current worker"));
    assert!(is_known_static_command(":suspend"));
    assert!(is_known_static_command(":resume"));
    assert!(is_known_static_command(":new now"));
    assert!(is_known_static_command(":session new"));
    assert!(is_known_static_command(":version"));
    assert!(is_known_static_command(":version now"));
    assert!(is_known_static_command(":skill jujutsu"));
    assert!(is_known_static_command(":skill:jujutsu args"));
    assert!(!is_known_static_command("/skillx jujutsu"));
    assert!(!is_known_static_command("hello :model engineer"));
}

/// Gmail OAuth finish input must use one fixed presentation across direct and
/// literal-escaped command spellings so code/state never enters echo or
/// history. The adjacent startup-profile assertion keeps the original refusal
/// regression covered after sharing this command-classification test location.
#[test]
fn gmail_oauth_finish_redirect_url_is_redacted_from_echo_and_prompt_history() {
    let line = ":email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    let redacted = ":email auth google finish <redacted>";
    assert_eq!(redacted_command_echo_line(line), redacted);
    assert_eq!(redacted_prompt_history_line(line, line), redacted);
    assert!(!redacted_command_echo_line(line).contains("auth-code-secret"));
    let missing_account = ":email auth google finish http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    assert_eq!(redacted_command_echo_line(missing_account), redacted);
    assert!(
        !redacted_prompt_history_line(missing_account, missing_account)
            .contains("auth-code-secret")
    );
    assert_eq!(
        redacted_command_echo_line(":email auth google start work"),
        ":email auth google start work"
    );
    let escaped = tau_cli_term::canonical_literal_colon_prompt(
        "::email auth google finish work http://localhost/?code=auth-code-secret",
    )
    .expect("literal escape canonicalizes");
    assert_eq!(
        redacted_prompt_history_line(&escaped, escaped.trim()),
        redacted
    );

    let profile_error =
        super::reject_dev_tmux_startup_overrides(Some("focused"), None, &[], &[], &[])
            .expect_err("profile refused");
    assert!(profile_error.to_string().contains("configuration profile"));
}

/// Content-enabled draft publication must preserve ordinary text while
/// replacing every recognizable Gmail OAuth finish buffer before serialization.
#[test]
fn contentful_prompt_drafts_redact_gmail_oauth_finish_buffers() {
    const CODE: &str = "CODE_SENTINEL_46";
    const STATE: &str = "STATE_SENTINEL_46";
    const REDACTED: &str = ":email auth google finish <redacted>";
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let sensitive =
        format!(":email auth google finish work http://127.0.0.1:54321/?code={CODE}&state={STATE}");

    queue_prompt_draft_snapshot(&handle, test_session_id("s1"), None, sensitive);
    let encoded = {
        let (mtx, _cv) = &handle;
        let slot = super::locked(mtx);
        let (_, draft) = slot.pending.as_ref().expect("pending sensitive draft");
        serde_json::to_vec(draft).expect("serialize sensitive draft")
    };
    assert!(
        !encoded
            .windows(CODE.len())
            .any(|window| window == CODE.as_bytes())
    );
    assert!(
        !encoded
            .windows(STATE.len())
            .any(|window| window == STATE.as_bytes())
    );
    assert!(String::from_utf8(encoded).expect("JSON").contains(REDACTED));

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        format!(
            "::email auth google finish work http://127.0.0.1:54321/?code={CODE}&state={STATE}"
        ),
    );
    let escaped_encoded = {
        let (mtx, _cv) = &handle;
        let slot = super::locked(mtx);
        let (_, draft) = slot.pending.as_ref().expect("pending escaped draft");
        serde_json::to_vec(draft).expect("serialize escaped draft")
    };
    assert!(
        !escaped_encoded
            .windows(CODE.len())
            .any(|window| window == CODE.as_bytes())
    );
    assert!(
        !escaped_encoded
            .windows(STATE.len())
            .any(|window| window == STATE.as_bytes())
    );
    assert!(
        String::from_utf8(escaped_encoded)
            .expect("JSON")
            .contains(REDACTED)
    );

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        "  ordinary draft  ".to_owned(),
    );
    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (_, draft) = slot.pending.as_ref().expect("pending ordinary draft");
    assert_eq!(draft.text.as_deref(), Some("  ordinary draft  "));
}

/// Content-free prompt drafts must stay content-free even when the active
/// editor contains a recognizable Gmail OAuth finish buffer.
#[test]
fn content_free_prompt_drafts_do_not_add_gmail_redaction_text() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        ":email auth google finish account http://localhost/?code=secret".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (_, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(draft.text, None);
}

/// The shared version label must use the exact build metadata string in both
/// command modes, rather than accepting arbitrary text between its parentheses.
#[test]
fn runtime_version_label_matches_cli_version_shape() {
    let label = super::version_label();
    let expected = match super::build_last_modified() {
        Some(date) => format!(
            "tau {} ({}, {date})",
            env!("CARGO_PKG_VERSION"),
            super::build_revision()
        ),
        None => format!(
            "tau {} ({})",
            env!("CARGO_PKG_VERSION"),
            super::build_revision()
        ),
    };
    assert_eq!(label, expected);

    let (_term, handle, vt) = setup(100, 24);
    handle.print_output(
        "system-info",
        tau_cli_term::resolve::themed_block(
            &cli_test_theme(),
            tau_themes::names::SYSTEM_INFO,
            format!("{}{}", transcript_markers::NOTICE, label),
        ),
    );
    sync(&handle);
    assert_eq!(
        vt.screen_text(100)
            .join("\n")
            .matches(label.as_str())
            .count(),
        1,
        ":version command feedback must render the shared label exactly once"
    );
}

/// Writer that feeds bytes directly into a VT parser and records a screen
/// snapshot at each redraw-thread flush boundary.
#[derive(Clone)]
struct VtWriter {
    /// Parser containing the latest virtual-terminal screen.
    parser: Arc<Mutex<vt100::Parser>>,
    /// Completed flush-delimited frames and their wait notification.
    frames: Arc<(Mutex<Vec<Vec<String>>>, std::sync::Condvar)>,
}

impl VtWriter {
    fn new(parser: vt100::Parser) -> Self {
        Self {
            parser: Arc::new(Mutex::new(parser)),
            frames: Arc::new((Mutex::new(Vec::new()), path_std_sync::Condvar::new())),
        }
    }

    fn screen_text(&self, w: u16) -> Vec<String> {
        self.parser
            .lock()
            .expect("vt")
            .screen()
            .rows(0, w)
            .collect()
    }

    fn screen_contains(&self, w: u16, needle: &str) -> bool {
        self.screen_text(w).iter().any(|r| r.contains(needle))
    }

    fn cell_style(&self, row: u16, col: u16) -> (vt100::Color, vt100::Color, bool) {
        let parser = self.parser.lock().expect("vt");
        let cell = parser
            .screen()
            .cell(row, col)
            .expect("visible terminal cell");
        (cell.fgcolor(), cell.bgcolor(), cell.bold())
    }

    fn frame_generation(&self) -> usize {
        self.frames.0.lock().expect("frames").len()
    }

    fn wait_for_frame_after(&self, generation: usize) -> Vec<String> {
        self.wait_for_frame_after_until(
            generation,
            Instant::now() + Duration::from_secs(2),
            "next frame",
        )
    }

    fn wait_for_frame_after_until(
        &self,
        generation: usize,
        deadline: Instant,
        context: &str,
    ) -> Vec<String> {
        let (frames, ready) = self.frames.as_ref();
        let mut frames = frames.lock().expect("frames");
        while frames.len() <= generation {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, timeout) = ready.wait_timeout(frames, remaining).expect("frames");
            frames = next;
            assert!(
                !timeout.timed_out() || frames.len() > generation,
                "timed out waiting for {context} after frame generation {generation}; captured frames: {frames:?}"
            );
        }
        frames[generation].clone()
    }

    fn wait_for_frame_containing_after(&self, mut generation: usize, needle: &str) -> usize {
        let starting_generation = generation;
        let deadline = Instant::now() + Duration::from_secs(2);
        let context =
            format!("frame containing {needle:?} (starting generation {starting_generation})");
        loop {
            let frame = self.wait_for_frame_after_until(generation, deadline, &context);
            generation += 1;
            if frame.iter().any(|row| row.contains(needle)) {
                return generation;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {context} after generation {}; last frame: {frame:?}",
                generation - 1
            );
        }
    }
}

impl std::io::Write for VtWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Process bytes directly into the parser. The mutex
        // ensures the test thread sees a consistent state.
        self.parser.lock().expect("vt").process(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let parser = self.parser.lock().expect("vt");
        let width = parser.screen().size().1;
        let frame = parser.screen().rows(0, width).collect();
        drop(parser);
        let (frames, ready) = self.frames.as_ref();
        frames.lock().expect("frames").push(frame);
        ready.notify_all();
        Ok(())
    }
}

fn setup(w: u16, h: u16) -> (Term, TermHandle, VtWriter) {
    let vt = VtWriter::new(vt100::Parser::new(h, w, 100));
    let (term, handle, _input) = Term::new_virtual(
        w as usize,
        h as usize,
        "> ",
        Box::new(vt.clone()),
        tau_cli_term::CursorShape::Bar,
    );
    (term, handle, vt)
}

/// Ensures ANSI emission preserves user/assistant base styling through
/// structural Markdown, including wrapping and the process-wide no-color mode.
#[test]
fn virtual_terminal_markdown_structure_inherits_transcript_colors() {
    let theme = tau_themes::Theme::parse(
        r##"{
            styles: {
                "user.prompt": { fg: "#f0f0f0", bg: "#101010" },
                "agent.response": { fg: "#00d0d0", bg: "#101010" },
                "markdown.heading": { bold: true },
                "markdown.list.marker": { bold: true },
            }
        }"##,
    )
    .expect("valid VT Markdown theme");
    let (_term, handle, vt) = setup(12, 8);
    handle.print_output(
        "markdown-user",
        super::markdown_render::markdown_block(&theme, tau_themes::names::USER_PROMPT, "# User\n"),
    );
    handle.print_output(
        "markdown-assistant",
        super::markdown_render::markdown_block(
            &theme,
            tau_themes::names::AGENT_RESPONSE,
            "12. assistant text wraps\n",
        ),
    );
    sync(&handle);

    let rows = vt.screen_text(12);
    let user_row = rows
        .iter()
        .position(|row| row.contains("# User"))
        .expect("user heading row") as u16;
    let user_offset = rows[user_row as usize]
        .find("# User")
        .expect("user heading column");
    let user_col = rows[user_row as usize][..user_offset].chars().count() as u16;
    let assistant_row = rows
        .iter()
        .position(|row| row.contains("12."))
        .expect("assistant list row") as u16;
    let assistant_offset = rows[assistant_row as usize]
        .find("12.")
        .expect("assistant list column");
    let assistant_col = rows[assistant_row as usize][..assistant_offset]
        .chars()
        .count() as u16;
    let continuation_row = rows
        .iter()
        .position(|row| row.contains("t text wraps"))
        .expect("wrapped assistant continuation") as u16;
    let continuation_offset = rows[continuation_row as usize]
        .find("t text wraps")
        .expect("wrapped assistant continuation column");
    let continuation_col = rows[continuation_row as usize][..continuation_offset]
        .chars()
        .count() as u16;
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    assert_eq!(
        vt.cell_style(user_row, user_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, true)
        } else {
            (
                vt100::Color::Rgb(0xf0, 0xf0, 0xf0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                true,
            )
        },
        "rows={rows:?}, user row={user_row}, col={user_col}"
    );
    assert_eq!(
        vt.cell_style(assistant_row, assistant_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, true)
        } else {
            (
                vt100::Color::Rgb(0x00, 0xd0, 0xd0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                true,
            )
        }
    );
    assert_eq!(
        vt.cell_style(continuation_row, continuation_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, false)
        } else {
            (
                vt100::Color::Rgb(0x00, 0xd0, 0xd0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                false,
            )
        }
    );
}

fn sync(handle: &TermHandle) {
    handle.redraw_sync();
}

fn assert_rendered_ansi_foreground(
    vt: &VtWriter,
    width: u16,
    text: &str,
    index: u8,
) -> vt100::Color {
    let rows = vt.screen_text(width);
    let (row, column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|column| (row as u16, column as u16)))
        .unwrap_or_else(|| panic!("missing submitted prompt {text:?}: {rows:?}"));
    let foreground = vt.cell_style(row, column).0;
    let expected = if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        vt100::Color::Default
    } else {
        vt100::Color::Idx(index)
    };
    assert_eq!(foreground, expected, "submitted prompt {text:?}");
    foreground
}

fn assert_rendered_bright_white(vt: &VtWriter, width: u16, text: &str) {
    let foreground = assert_rendered_ansi_foreground(vt, width, text, 15);
    if foreground == vt100::Color::Idx(15) {
        assert_ne!(
            foreground,
            vt100::Color::Idx(7),
            "submitted prompt {text:?} must not use ordinary white"
        );
    }
}

fn rendered_cell_attributes(
    vt: &VtWriter,
    width: u16,
    text: &str,
) -> (vt100::Color, vt100::Color, bool, bool, bool) {
    let rows = vt.screen_text(width);
    let (row, byte_column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|column| (row as u16, column)))
        .unwrap_or_else(|| panic!("missing submitted prompt text {text:?}: {rows:?}"));
    let column = rows[row as usize][..byte_column].chars().count() as u16;
    let parser = vt.parser.lock().expect("vt");
    let cell = parser
        .screen()
        .cell(row, column)
        .expect("visible terminal cell");
    (
        cell.fgcolor(),
        cell.bgcolor(),
        cell.bold(),
        cell.italic(),
        cell.underline(),
    )
}

fn expected_rendered_color(color: vt100::Color) -> vt100::Color {
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        vt100::Color::Default
    } else {
        color
    }
}

/// Ensures live UI submissions and durable replay render the same Markdown
/// attributes while retaining the exact raw prompt text for routing and
/// history.
#[test]
fn submitted_prompt_markdown_styles_match_live_and_replay_without_mutating_raw_text() {
    let theme = tau_themes::Theme::parse(
        r##"{
            styles: {
                "user.prompt": { fg: "#f0f0f0", bg: "#101010" },
                "markdown.strong": { bold: true },
                "markdown.emphasis": { italic: true },
                "markdown.code": { fg: "#00d000" },
                "markdown.link": { fg: "#d00000", bold: true },
            }
        }"##,
    )
    .expect("valid submitted-prompt Markdown theme");
    let source = "**strong** _emphasis_ `code` [link](https://example.test/docs)".to_owned();

    for replayed in [false, true] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            theme.clone(),
        );
        let event = if replayed {
            Event::AgentPromptSubmitted(AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id("main"),
                text: source.clone(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                display_name: None,
                ctx_id: None,
            })
        } else {
            Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: source.clone(),
                agent_id: agent_id("main"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            })
        };
        renderer.handle(&event);
        sync(&handle);

        assert_eq!(
            renderer.last_submitted_user_prompt_text_for_test(),
            Some(source.as_str()),
            "the {} projection must retain exact raw prompt bytes",
            if replayed { "replayed" } else { "live" }
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "**strong**"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xf0, 0xf0, 0xf0)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                true,
                false,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "_emphasis_"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xf0, 0xf0, 0xf0)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                false,
                true,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "`code`"),
            (
                expected_rendered_color(vt100::Color::Rgb(0x00, 0xd0, 0x00)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                false,
                false,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "link"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xd0, 0x00, 0x00)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                true,
                false,
                false,
            )
        );
        assert!(
            !vt.screen_contains(100, "https://example.test/docs"),
            "the display-only OSC 8 link projection must not replace retained raw text"
        );
    }
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse(format!("msg-{sender_id}-{recipient}"))
            .expect("test message id must satisfy the identifier grammar"),
        sender_id: agent_id(sender_id),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: agent_id(recipient),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn external_agent_message(
    sender_id: &str,
    session_id: &str,
    recipient: &str,
    message: &str,
) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse(format!(
            "msg-{sender_id}-{session_id}-{recipient}"
        ))
        .expect("test message id must satisfy the identifier grammar"),
        sender_id: agent_id(sender_id),
        recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
            session_id: test_session_id(session_id),
            agent_id: agent_id(recipient),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn visible_lines(vt: &VtWriter, w: u16) -> Vec<String> {
    vt.screen_text(w)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn eventually_screen_contains(vt: &VtWriter, w: u16, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if vt.screen_contains(w, needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn eventually_screen_lacks(vt: &VtWriter, w: u16, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if !vt.screen_contains(w, needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn assistant_message_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn agent_prompt_created(agent_prompt_id: &str, session_id: &str) -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: test_session_id(session_id),
        system_prompt: String::new(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

fn agent_prompt_started(agent_prompt_id: &str, session_id: &str) -> tau_proto::AgentPromptStarted {
    tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: test_session_id(session_id),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }
}

fn standalone_compaction_started(
    transaction_id: &str,
    agent_prompt_id: &str,
) -> AgentStandaloneCompactionStarted {
    AgentStandaloneCompactionStarted {
        agent_id: agent_id("main"),
        transaction_id: tau_proto::CompactionTransactionId::parse(transaction_id)
            .expect("known-safe compaction transaction id"),
        compact_prompt_id: test_agent_prompt_id(agent_prompt_id),
        cut: tau_proto::AgentHead::Root,
        resume_through: None,
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
    }
}

fn standalone_compaction_prompt_started(agent_prompt_id: &str) -> tau_proto::AgentPromptStarted {
    tau_proto::AgentPromptStarted {
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        ..agent_prompt_started(agent_prompt_id, "s1")
    }
}

fn self_compaction_requested(request_id: &str, call_id: &str) -> AgentManualCompactionRequested {
    AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse(request_id)
            .expect("known-safe request id"),
        caller_agent_id: agent_id("main"),
        target_agent_id: agent_id("main"),
        initiating_agent_prompt_id: test_agent_prompt_id("ap-main-request"),
        initiating_tool_call_id: call_id.into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::Compact,
        visible_tool_name: tau_proto::ToolName::new("compact"),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: 0,
        model: "test/model".parse().expect("model id"),
        resume_inference: true,
    }
}

fn self_compaction_started(
    request_id: &str,
    call_id: &str,
    transaction_id: &str,
    agent_prompt_id: &str,
) -> AgentStandaloneCompactionStarted {
    AgentStandaloneCompactionStarted {
        trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: tau_proto::CompactionRequestId::parse(request_id)
                .expect("known-safe request id"),
            caller_agent_id: agent_id("main"),
            initiating_tool_call_id: call_id.into(),
        },
        ..standalone_compaction_started(transaction_id, agent_prompt_id)
    }
}

/// Builds a fresh bound danger snapshot for selected-agent quota wiring tests.
fn danger_quota_event(model: &tau_proto::ModelId) -> Event {
    let now = super::event_renderer::unix_time_millis();
    let remaining = 604_800_u64 / 2;
    Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: model.provider.clone(),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-danger").expect("quota epoch"),
        sequence: 1,
        windows: vec![tau_proto::ProviderQuotaWindow {
            key: tau_proto::ProviderQuotaWindowKey {
                limit_id: tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool"),
                window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                    .expect("quota window"),
            },
            used_basis_points: 9_400,
            usage_observed_at_unix_ms: now,
            window_seconds: 604_800,
            reset_at_unix_seconds: Some(now / 1_000 + remaining),
            remaining_seconds_at_timing_anchor: Some(remaining as i64),
            timing_anchor_observed_at_unix_ms: Some(now),
            server_offset_ms: Some(0),
            server_offset_observed_at_unix_ms: Some(now),
        }],
        route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
            model: model.clone(),
            limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool")],
            observed_at_unix_ms: now,
            provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
        }],
    })
}

fn provider_response_stats_update(
    agent_prompt_id: &str,
    agent_id: tau_proto::AgentId,
    current_bytes: u64,
    previous_bytes: u64,
    current_elapsed_micros: u64,
    previous_elapsed_micros: u64,
) -> ProviderResponseUpdated {
    ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id,
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: Some(tau_proto::ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: current_bytes,
                elapsed_micros: current_elapsed_micros,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: previous_bytes,
                elapsed_micros: previous_elapsed_micros,
            },
            first_semantic_output_elapsed_micros: None,
        }),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn main_provider_response_stats_update(
    agent_prompt_id: &str,
    current_bytes: u64,
    previous_bytes: u64,
) -> ProviderResponseUpdated {
    provider_response_stats_update(
        agent_prompt_id,
        tau_proto::AgentId::parse("main").expect("agent id"),
        current_bytes,
        previous_bytes,
        2_000_000,
        1_000_000,
    )
}

#[test]
fn renderer_starts_without_selected_or_default_agent() {
    // Regression: the UI opens in the start-new-agent state instead of
    // preselecting a synthetic `main` agent.
    let (_term, handle, _vt) = setup(80, 24);
    let renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );
    assert!(
        renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .is_empty()
    );
    assert!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .active_agents()
            .is_empty()
    );
}

/// Complete agent stats must redraw the selected-agent status row with escaped
/// work metadata, retaining the phase when narrow width drops the task title.
#[test]
fn selected_agent_status_row_renders_phase_and_adapts_task_title() {
    let title = "review \u{202e}fix";
    let escaped_title = tau_proto::visible_escape_metadata(title);
    let render_at_width = |width| {
        let (_term, handle, vt) = setup(width, 8);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: test_session_id("s1"),
            reason: tau_proto::SessionStartReason::Initial,
        }));
        renderer.switch_agent("worker".to_owned());
        renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker"),
            work_status: tau_proto::SessionAgentWorkStatus::new(
                tau_proto::AgentWorkStatusPhase::Blocked,
                Some(title.to_owned()),
            )
            .expect("valid work status"),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state: tau_proto::AgentRuntimeState::Idle,
            turn_activity: tau_proto::AgentTurnActivity::Manipulating,
            tools: Default::default(),
            context: Default::default(),
            estimated_api_cost: Default::default(),
            creator_subtree_estimated_api_cost: Default::default(),
        }));
        sync(&handle);
        vt.screen_text(width)
    };

    let wide = render_at_width(80);
    assert!(
        wide.iter()
            .any(|row| row.contains(&format!("⛔️🔨 @worker {escaped_title}"))),
        "wide selected-agent status row should contain phase and escaped title: {wide:?}"
    );
    assert!(
        wide.iter().all(|row| !row.contains(title)),
        "raw structural metadata must not reach the terminal: {wide:?}"
    );

    let narrow = render_at_width(18);
    assert!(
        narrow.iter().any(|row| row.contains("⛔️🔨 @worker")),
        "work phase should survive narrow-width fitting: {narrow:?}"
    );
    assert!(
        narrow.iter().all(|row| !row.contains(&escaped_title)),
        "lower-priority task title should yield before phase: {narrow:?}"
    );
}

/// Increasing `:set redraw-history-size` should restore more scrollback
/// immediately by forcing a full redraw, while decreasing it should only affect
/// the next otherwise-needed full redraw.
#[test]
fn redraw_history_size_only_redraws_immediately_when_increased() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    sync(&handle);
    let initial_full_renders = handle.full_render_count();

    renderer.apply_setting("redraw-history-size", "100");
    sync(&handle);
    assert_eq!(handle.redraw_history_size(), 100);
    assert_eq!(handle.full_render_count(), initial_full_renders);

    renderer.apply_setting("redraw-history-size", "101");
    sync(&handle);
    assert_eq!(handle.redraw_history_size(), 101);
    assert_eq!(handle.full_render_count(), initial_full_renders + 1);
}

#[test]
fn first_agent_prompt_created_selects_new_agent_and_new_session_clears_it() {
    // Regression: the first prompt created for the default conversation carries
    // the new agent id; seeing it from the empty state selects that agent. A
    // later `:session new` returns to the empty start-new-agent state.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_abc12345"),
        ..agent_prompt_created("sp1", "s1")
    }));
    sync(&handle);
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("engineer_abc12345")
    );
    assert!(vt.screen_contains(80, "@engineer_abc12345"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );
}

#[test]
fn delayed_prompt_started_does_not_duplicate_live_response_block() {
    // Regression: provider updates can arrive before a delayed
    // `agent.prompt_started` if an interceptor parks that lifecycle event. The
    // delayed start must not create a second live response block alongside the
    // provider-update fallback block.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            " world",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    let lines = visible_lines(&vt, 80);
    let response_lines = lines
        .iter()
        .filter(|line| line.contains("hello"))
        .collect::<Vec<_>>();
    assert_eq!(
        response_lines.len(),
        1,
        "delayed prompt_started must not leave duplicate live response blocks: {lines:?}"
    );
    assert!(
        response_lines[0].contains("hello world"),
        "response should keep accumulating in the single live block: {lines:?}"
    );
}

#[test]
fn initial_session_started_omits_session_status_and_role_placeholder() {
    // Regression: startup may announce SessionStarted before role selection.
    // The status bar must not duplicate the prompt-context session id or add a
    // misleading no-role placeholder.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("tau-agent-test"),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "&tau-agent-test"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

/// Ensures the resolved profile stack follows the session-directory status
/// while absent selection does not add a synthetic startup line.
#[test]
fn session_directory_status_reports_only_selected_startup_profile_stacks() {
    let session_dir = HarnessSessionDir {
        session_id: test_session_id("tau-agent-test"),
        path: "/tmp/tau-agent-test".into(),
        status: SessionDirStatus::New,
    };

    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.set_startup_profile_selection(Some(
        path_tau_config_settings::ProfileSelection::parse("focused,review")
            .expect("profile selection"),
    ));
    renderer.handle(&Event::HarnessSessionDir(session_dir.clone()));
    sync(&handle);
    let named_lines = visible_lines(&vt, 100);
    let session_line = named_lines
        .iter()
        .position(|line| line.contains("▤ session dir:"))
        .expect("session directory status");
    assert_eq!(
        named_lines
            .get(session_line + 1)
            .map(|line| line.trim_end()),
        Some("▤ config profile stack: focused,review")
    );

    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::HarnessSessionDir(session_dir));
    sync(&handle);
    assert!(!vt.screen_contains(100, "config profile stack:"));
}

/// A theme refresh between an optimistic session switch and its authoritative
/// echo must preserve routing, drafts, and the new prompt-context session.
#[test]
fn theme_refresh_preserves_optimistic_session_context() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.set_right_prompt_paths("/tmp/project".into(), None);
    let draft_handle = Arc::new((
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    ));
    let active_session = Arc::new(Mutex::new(
        tau_proto::SessionId::parse("old-session").expect("session id"),
    ));
    renderer.set_draft_retargeter(draft_handle.clone(), active_session.clone());

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("old-session"),
        reason: SessionStartReason::Initial,
    }));
    {
        let mut draft = draft_handle.0.lock().expect("draft");
        draft.epoch = 7;
        draft.pending = Some((
            7,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("new-session"),
                target_agent_id: None,
                text: Some("draft".to_owned()),
            },
        ));
    }
    *active_session.lock().expect("active session") =
        tau_proto::SessionId::parse("new-session").expect("session id");
    let themed =
        tau_themes::Theme::parse(r##"{ styles: { "prompt.cwd": { fg: "red", bold: true } } }"##)
            .expect("theme parses");
    renderer.apply_theme(themed);
    sync(&handle);

    assert_eq!(
        active_session.lock().expect("active session").as_str(),
        "new-session"
    );
    let draft = draft_handle.0.lock().expect("draft");
    assert_eq!(draft.epoch, 7);
    assert!(draft.pending.is_some());
    assert!(vt.screen_contains(80, "/tmp/project &new-session"));
    assert!(!vt.screen_contains(80, "&old-session"));
}

#[test]
fn extension_prompt_with_target_does_not_select_from_empty_state() {
    // Regression: extension side prompts now carry target_agent_id for routing,
    // but `:agent none`/startup must stay on the no-agent screen until the user
    // explicitly selects a transcript.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..agent_prompt_created("worker-sp", "s1")
    }));

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response("worker-sp", vec![assistant_message_item("worker answer")])
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker answer"));
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("worker-1")
    );
}

#[test]
fn replayed_durable_first_user_prompt_selects_live_agent() {
    // Regression: cold replay skips transient AgentPromptCreated events. The
    // durable agent-owned prompt fact must still render the user message and
    // select a live agent so the next Enter press sends a targeted follow-up
    // instead of being rejected as "not live".
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "hello".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("engineer_abc12345")
    );
    assert!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .is_live("engineer_abc12345")
    );
    assert!(vt.screen_contains(80, "hello"));
}

fn render_submitted_prompt_projections(theme: tau_themes::Theme) -> VtWriter {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer =
        EventRenderer::new(handle.clone(), tau_cli_term::CompletionData::new(), theme);
    let ui_prompt = |text: &str| {
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: text.to_owned(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })
    };

    renderer.handle(&ui_prompt("immediate submitted prompt"));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "promoted queued prompt".to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&ui_prompt("promoted queued prompt"));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: Default::default(),
        agent_id: agent_id("main"),
        text: "steered submitted prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("main"),
        text: "replayed submitted prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.switch_agent("other".to_owned());
    renderer.switch_agent("main".to_owned());
    sync(&handle);

    vt
}

/// Ensures every submitted-user-prompt projection renders ANSI bright white in
/// the default theme rather than ordinary-white index 7.
#[test]
fn submitted_prompt_projections_render_default_bright_white() {
    let vt = render_submitted_prompt_projections(tau_themes::Theme::builtin());

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_bright_white(&vt, 100, text);
    }
}

/// Ensures the active `tau-dpc` theme renders every submitted-prompt
/// projection bright white rather than inheriting the terminal-default color.
#[test]
fn submitted_prompt_projections_render_dpc_bright_white() {
    let vt = render_submitted_prompt_projections(tau_themes::Theme::builtin_dpc());

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_bright_white(&vt, 100, text);
    }
}

/// Ensures each submitted-prompt projection preserves an explicit custom
/// `user.prompt` foreground instead of restoring the default bright white.
#[test]
fn custom_submitted_prompt_foreground_overrides_default_bright_white() {
    let theme = tau_themes::Theme::parse(r#"{ styles: { "user.prompt": { fg: "grey" } } }"#)
        .expect("custom prompt theme parses");
    let vt = render_submitted_prompt_projections(theme);

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_ansi_foreground(&vt, 100, text, 7);
    }
}

/// Timer-created internal prompt submissions render a visible wakeup marker so
/// the following response is attributable to the timer, not an invisible
/// prompt.
#[test]
fn timer_wakeup_prompt_submitted_renders_visible_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: Some("timer:wake:1".to_owned()),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "Timer `wake` woke this agent: stand up"));
    assert!(!vt.screen_contains(100, "woke this agent: Timer `wake` fired"));
}

/// Timer wakeups that were queued during a busy turn render the same marker
/// when folded as steered prompts.
#[test]
fn timer_wakeup_prompt_steered_renders_visible_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("timer:wake:2".to_owned()),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "Timer `wake` woke this agent: stand up"));
    assert!(!vt.screen_contains(100, "woke this agent: Timer `wake` fired"));
}

/// An extension-originated steered prompt without a queued user projection
/// renders as a message rather than a user prompt.
#[test]
fn extension_prompt_steered_uses_message_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::Extension {
            name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
        },
        agent_id: agent_id("engineer_abc12345"),
        text: "extension-steered prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    for (text, submission_source) in [
        (
            "legacy internal payload",
            tau_proto::PromptSubmissionSource::Legacy,
        ),
        (
            "human internal payload",
            tau_proto::PromptSubmissionSource::HumanUi,
        ),
    ] {
        renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source,
            display_name: None,
            ctx_id: None,
        }));
    }
    sync(&handle);

    assert!(vt.screen_contains(100, "■ External `fixture` message:"));
    assert!(vt.screen_contains(100, "extension-steered prompt"));
    assert!(!vt.screen_contains(100, "⬤ extension-steered prompt"));
}

/// Internal prompt facts use authenticated source rather than payload class:
/// extensions are always attributed messages, while typed harness prompts
/// reproject in place through the default-off diagnostic toggle.
#[test]
fn source_aware_internal_prompt_projection_and_toggle_are_exactly_once() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let extension = tau_proto::PromptSubmissionSource::Extension {
        name: tau_proto::ExtensionName::parse("std-swarm").expect("valid extension name"),
    };
    let harness = tau_proto::PromptSubmissionSource::HarnessInternal;

    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "extension submitted payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: extension.clone(),
        display_name: None,
        ctx_id: Some("swarm-command-1".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: extension,
        agent_id: agent_id("engineer_abc12345"),
        text: "extension steered payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("swarm-command-2".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness submitted payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: harness.clone(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: harness,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness steered payload".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "External `std-swarm` message:"));
    assert!(vt.screen_contains(100, "extension submitted payload"));
    assert!(vt.screen_contains(100, "extension steered payload"));
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    assert!(!vt.screen_contains(100, "legacy internal payload"));
    assert!(!vt.screen_contains(100, "human internal payload"));

    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    let enabled = visible_lines(&vt, 100).join("\n");
    assert_eq!(enabled.matches("harness submitted payload").count(), 1);
    assert_eq!(enabled.matches("harness steered payload").count(), 1);
    assert_eq!(enabled.matches("extension submitted payload").count(), 1);
    assert_eq!(enabled.matches("extension steered payload").count(), 1);
    assert!(!enabled.contains("legacy internal payload"));
    assert!(!enabled.contains("human internal payload"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "harness submitted payload"));
    assert!(vt.screen_contains(100, "harness steered payload"));

    renderer.apply_setting("show-internal-prompts", "off");
    sync(&handle);
    assert!(!vt.screen_contains(100, "harness submitted payload"));
    assert!(!vt.screen_contains(100, "harness steered payload"));
    assert!(vt.screen_contains(100, "extension submitted payload"));
    assert!(vt.screen_contains(100, "extension steered payload"));
}

/// The real untyped `AgentPromptSteered` carrier used by work-status reminders
/// remains model-visible while compact mode suppresses only its human
/// projection.
#[test]
fn compact_mode_dominates_status_reminder_internal_prompt_subfilter() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "Set your status to `working` before continuing substantive tool work.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));

    renderer.apply_setting("notice-level", "warning");
    sync(&handle);
    assert!(!vt.screen_contains(120, "Set your status to `working`"));
    renderer.apply_setting("notice-level", "info");
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(120, "Set your status to `working`"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));
}

/// A new session must discard hidden prompt slots before block identifiers are
/// reused, so enabling diagnostics cannot disclose prior-session prompt text.
#[test]
fn internal_prompt_toggle_does_not_reproject_previous_session_history() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let internal = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            display_name: None,
            ctx_id: None,
        })
    };

    renderer.handle(&internal("session one hidden prompt"));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&internal("session two hidden prompt"));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    assert!(!vt.screen_contains(100, "session one hidden prompt"));
    assert!(vt.screen_contains(100, "session two hidden prompt"));
}

/// Timer and context-alert presentation own their canonical prompt facts before
/// the diagnostic toggle, so enabling it cannot append generic notice blocks.
#[test]
fn internal_prompt_toggle_preserves_timer_and_context_alert_special_presentation() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `special` fired: exact once".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: Some("timer:special:1".to_owned()),
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "context alert exact once".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        ctx_id: None,
    }));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    let lines = visible_lines(&vt, 100).join("\n");
    assert_eq!(
        lines
            .matches("Timer `special` woke this agent: exact once")
            .count(),
        1
    );
    assert_eq!(lines.matches("context alert exact once").count(), 1);
    assert!(!lines.contains("[tau-internal]: Timer `special` fired: exact once"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "Timer `special` woke this agent"));
    assert!(!vt.screen_contains(100, "context alert exact once"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "Timer `special` woke this agent"));
    assert!(vt.screen_contains(100, "context alert exact once"));
}

/// Replayed Submitted and Steered facts retain their per-agent source-aware
/// slots across snapshot switches, so repeated toggles restore each once.
#[test]
fn replayed_source_aware_prompt_slots_survive_agent_snapshot_switches() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.switch_agent("replayed-agent".to_owned());
    let submitted = Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("replayed-agent"),
        text: "replayed submitted internal".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    let steered = Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("replayed-agent"),
        text: "replayed steered internal".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    });
    renderer.handle(&submitted);
    renderer.handle(&steered);
    renderer.switch_agent("other-agent".to_owned());
    renderer.switch_agent("replayed-agent".to_owned());
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);

    let enabled = visible_lines(&vt, 100).join("\n");
    assert_eq!(enabled.matches("replayed submitted internal").count(), 1);
    assert_eq!(enabled.matches("replayed steered internal").count(), 1);
    assert!(
        enabled.find("replayed submitted internal") < enabled.find("replayed steered internal")
    );

    renderer.apply_setting("show-internal-prompts", "off");
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    let retoggled = visible_lines(&vt, 100).join("\n");
    assert_eq!(retoggled.matches("replayed submitted internal").count(), 1);
    assert_eq!(retoggled.matches("replayed steered internal").count(), 1);
}

/// A typed WatchProviderStatus strips only the canonical production envelope in
/// live, reconstructed, and replayed transcripts; delimiter-like untyped text
/// remains an ordinary message.
#[test]
fn authenticated_internal_notices_are_consistent_live_and_replayed() {
    let agent = agent_id("internal-agent");
    let provider_snapshot = Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("provider-snapshot")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("watched-agent"),
        sender_session_id: None,
        recipient_id: agent.clone(),
        kind: tau_proto::AgentMessageKind::WatchProviderStatus,
        watch_provider_status: Some(tau_proto::AgentWatchProviderStatusNotification {
            session_id: test_session_id("s1"),
            subscription_id: "subscription-1".to_owned(),
            turn_generation: 1,
            agent_prompt_id: test_agent_prompt_id("prompt-1"),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Unknown,
                attempt: 1,
                next_retry_delay_secs: 11,
            },
            initial: true,
        }),
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: format!(
            "{}watch{}",
            tau_proto::TAU_INTERNAL_OPEN,
            tau_proto::TAU_INTERNAL_CLOSE,
        ),
    });
    let mut wrong_kind_provider = provider_snapshot.clone();
    let Event::AgentMessageReceived(message) = &mut wrong_kind_provider else {
        unreachable!("cloned provider event retains its variant");
    };
    message.kind = tau_proto::AgentMessageKind::Message;
    message.message = format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    let mut missing_typed_provider = provider_snapshot.clone();
    let Event::AgentMessageReceived(message) = &mut missing_typed_provider else {
        unreachable!("cloned provider event retains its variant");
    };
    message.watch_provider_status = None;
    message.message = format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    let submitted = Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent.clone(),
        text: "Your `status` is set to `working` on \"Fix Slack mandatory terminal delivery\". Set it to `done`, `waiting`, or `blocked` to finish or call `wait` when waiting for external events.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    let steered = Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent.clone(),
        text: "replayed internal body".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    });

    let (_live_term, live_handle, live_vt) = setup(100, 24);
    let mut live = marker_test_renderer(live_handle.clone());
    live.switch_agent(agent.as_str().to_owned());
    live.handle(&provider_snapshot);
    live.handle(&wrong_kind_provider);
    live.handle(&missing_typed_provider);
    live.handle(&submitted);
    live.apply_setting("show-internal-prompts", "on");
    sync(&live_handle);
    let live_text = visible_lines(&live_vt, 100).join("\n");
    let provider_frame = format!(
        "{}watch{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    assert!(live_text.contains("□ watch"));
    assert!(live_text.contains("□ Your `status` is set to `working` on \"Fix Slack mandatory"));
    assert!(!live_text.contains("[tau-internal"));
    assert!(!live_text.contains(&provider_frame));
    assert!(live_text.contains("■ Message from @watched-agent:"));
    assert!(live_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(live_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(!live_text.contains("[tau-internal]: wrong-kind provider body"));
    assert!(!live_text.contains("[tau-internal]: missing-typed provider body"));
    live.switch_agent("other-agent".to_owned());
    live.switch_agent(agent.as_str().to_owned());
    sync(&live_handle);
    let reconstructed_live_text = visible_lines(&live_vt, 100).join("\n");
    assert_eq!(reconstructed_live_text.matches("□ watch").count(), 1);
    assert!(!reconstructed_live_text.contains(&provider_frame));
    assert!(reconstructed_live_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(reconstructed_live_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));

    let (_cold_term, cold_handle, cold_vt) = setup(100, 24);
    let mut cold = marker_test_renderer(cold_handle.clone());
    cold.switch_agent(agent.as_str().to_owned());
    cold.handle_recorded_at(&provider_snapshot, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&wrong_kind_provider, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&missing_typed_provider, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&submitted, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&steered, tau_proto::UnixMicros::new(2));
    cold.apply_setting("show-internal-prompts", "on");
    sync(&cold_handle);
    let cold_text = visible_lines(&cold_vt, 100).join("\n");
    assert!(cold_text.contains("□ watch"));
    assert!(cold_text.contains("□ Your `status` is set to `working` on \"Fix Slack mandatory"));
    assert!(cold_text.contains("□ replayed internal body"));
    assert!(!cold_text.contains("[tau-internal"));
    assert!(!cold_text.contains(&provider_frame));
    assert!(cold_text.contains("■ Message from @watched-agent:"));
    assert!(cold_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(cold_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(!cold_text.contains("[tau-internal]: wrong-kind provider body"));
    assert!(!cold_text.contains("[tau-internal]: missing-typed provider body"));
}

/// Semantic row markers keep messages and notices visually distinct while
/// preserving the structured-status marker between them.
#[test]
fn semantic_row_markers_match_their_categories() {
    assert_eq!(transcript_markers::MESSAGE, "■ ");
    assert_eq!(transcript_markers::STATUS_UPDATE, "▤ ");
    assert_eq!(transcript_markers::NOTICE, "□ ");
}

/// Typed harness provenance remains hidden by default even for a user-class
/// legacy-shaped fact, then the diagnostic toggle reveals it as a notice.
#[test]
fn unqueued_harness_prompt_steered_uses_toggle_controlled_notice() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "harness-steered message".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "harness-steered message"));
    renderer.apply_setting("show-internal-prompts", "on");
    sync(&handle);
    assert!(vt.screen_contains(100, "□ harness-steered message"));
    assert!(!vt.screen_contains(100, "⬤ harness-steered message"));
}

/// A tagged fresh-turn context-size alert renders exactly once in journal
/// order, while an otherwise identical untagged internal prompt stays hidden.
#[test]
fn context_size_alert_prompt_submitted_renders_internal_history_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    let visible_prompt = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        })
    };
    renderer.handle(&visible_prompt("before submitted alert"));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("engineer_abc12345"),
        text: "untagged internal prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("engineer_abc12345"),
        text: "Use the `compact` tool after finishing your current task.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&visible_prompt("after submitted alert"));
    sync(&handle);

    let relevant_lines = visible_lines(&vt, 100)
        .into_iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| line.contains("submitted alert") || line.contains("Use the `compact`"))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_lines,
        [
            "> before submitted alert",
            "□ Use the `compact` tool after finishing your current task.",
            "> after submitted alert",
        ]
    );
    assert!(!vt.screen_contains(100, "untagged internal prompt"));
}

/// A context-size alert folded after tools uses the same exact, ordered history
/// presentation as a fresh-turn delivery.
#[test]
fn context_size_alert_prompt_steered_renders_internal_history_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    let visible_prompt = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        })
    };
    renderer.handle(&visible_prompt("before steered alert"));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: true,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "compact after tools".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        ctx_id: None,
    }));
    renderer.handle(&visible_prompt("after steered alert"));
    sync(&handle);

    let relevant_lines = visible_lines(&vt, 100)
        .into_iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| line.contains("steered alert") || line.contains("compact after tools"))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_lines,
        [
            "> before steered alert",
            "□ compact after tools",
            "> after steered alert",
        ]
    );
}

/// Successful compaction lifecycle notices retain their info style and gain the
/// semantic notice marker.
#[test]
fn compaction_lifecycle_notice_uses_info_style() {
    let theme = cli_test_theme();
    let lifecycle = render_harness_notice(
        &theme,
        &tau_proto::HarnessNotice::diagnostic(
            tau_proto::notice_kind::HARNESS_NOTICE,
            "Starting compaction request cr-35-0 for reviewer-sOqj (ct-35)",
            tau_proto::NoticeLevel::Info,
        ),
    );

    assert_eq!(lifecycle.content.spans()[0].style.fg, Some(Color::Blue));
    assert_eq!(
        lifecycle
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "□ Starting compaction request cr-35-0 for reviewer-sOqj (ct-35)"
    );
}

/// Manual-compaction lifecycle status belongs only to the target transcript,
/// including when the target is a descendant of the currently selected agent.
#[test]
fn manual_compaction_lifecycle_status_follows_target_agent_selection() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentManualCompactionRequested(
        AgentManualCompactionRequested {
            request_id: tau_proto::CompactionRequestId::parse("cr-48-0").expect("request id"),
            caller_agent_id: agent_id("manager"),
            target_agent_id: agent_id("reviewer-KH50"),
            initiating_agent_prompt_id: test_agent_prompt_id("ap-manager-48"),
            initiating_tool_call_id: "call-48".into(),
            initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
            visible_tool_name: tau_proto::ToolName::new("agent_compact"),
            requested_target_head: tau_proto::AgentHead::Root,
            target_generation: 0,
            model: "test/model".parse().expect("model id"),
            resume_inference: false,
        },
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        AgentStandaloneCompactionStarted {
            agent_id: agent_id("reviewer-KH50"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-48")
                .expect("transaction id"),
            compact_prompt_id: test_agent_prompt_id("ap-reviewer-KH50-48"),
            cut: tau_proto::AgentHead::Root,
            resume_through: None,
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                request_id: tau_proto::CompactionRequestId::parse("cr-48-0").expect("request id"),
                caller_agent_id: agent_id("manager"),
                initiating_tool_call_id: "call-48".into(),
            },
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "compaction request cr-48-0"));

    renderer.switch_agent("unrelated".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "compaction request cr-48-0"));

    renderer.switch_agent("reviewer-KH50".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(
        100,
        "Agent manager accepted compaction request for reviewer-KH50 (cr-48-0)"
    ));
    assert!(!vt.screen_contains(100, "Starting compaction request"));
}

#[test]
fn role_cycling_only_enabled_without_selected_agent() {
    // Regression: role cycling changes the role used for the next new agent,
    // so once an agent is selected it must stop mutating the live agent's role.
    let current_agent_state = Arc::new(Mutex::new(None));
    assert!(role_cycling_enabled(&current_agent_state));

    *current_agent_state.lock().expect("current agent") = Some("engineer_abc12345".to_owned());
    assert!(!role_cycling_enabled(&current_agent_state));

    *current_agent_state.lock().expect("current agent") = None;
    assert!(role_cycling_enabled(&current_agent_state));
}

/// Ctrl-K/Ctrl-J cycle through active agents and the overview while skipping
/// suspended agents that would refuse user prompts.
#[test]
fn agent_switching_cycles_active_agents_and_skips_suspended() {
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned(), "charlie".to_owned()];
    let active_agents = HashSet::from(["alpha".to_owned(), "charlie".to_owned()]);

    assert_eq!(
        next_agent_cycle_selection(Some("alpha"), &known_agents, &active_agents, 1).as_deref(),
        Some("charlie")
    );
    assert_eq!(
        next_agent_cycle_selection(Some("alpha"), &known_agents, &active_agents, -1),
        None
    );
    assert_eq!(
        next_agent_cycle_selection(Some("charlie"), &known_agents, &active_agents, 1),
        None
    );
}

/// Cycling from the overview enters the active-agent ring from the edge
/// implied by the shortcut direction.
#[test]
fn agent_switching_without_selection_starts_at_edge_for_direction() {
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned()];
    let active_agents = HashSet::from(["alpha".to_owned(), "bravo".to_owned()]);

    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, 1).as_deref(),
        Some("alpha")
    );
    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, -1).as_deref(),
        Some("bravo")
    );
}

/// The overview remains the sole cycle state when no agents are active.
#[test]
fn agent_switching_without_active_agents_stays_on_overview() {
    let known_agents = vec!["suspended".to_owned()];
    let active_agents = HashSet::new();

    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, 1),
        None
    );
    assert_eq!(
        next_agent_cycle_selection(None, &known_agents, &active_agents, -1),
        None
    );
}

fn tool_started(call_id: &str, tool_name: &str, arguments: CborValue) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn initial_tool_progress(call_id: &str, tool_name: &str, args: &str, mode: &str) -> Event {
    Event::ToolProgress(tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: args.to_owned(),
            mode: mode.to_owned(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
    })
}
fn provider_response_delta_update(
    agent_prompt_id: impl Into<tau_proto::AgentPromptId>,
    text: impl Into<String>,
    thinking: Option<String>,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseUpdated {
    let text = text.into();
    let mut deltas = Vec::new();
    if let Some(thinking) = thinking.filter(|thinking| !thinking.is_empty()) {
        deltas.push(tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 0,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: thinking,
        });
    }
    if !text.is_empty() {
        deltas.push(tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text,
            phase: None,
        });
    }
    ProviderResponseUpdated {
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas,
        compaction: None,
        status: None,
        response_stats: None,
        originator,
    }
}

fn finished_response(
    agent_prompt_id: &str,
    output_items: Vec<ContextItem>,
) -> ProviderResponseFinished {
    let stop_reason = if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        ProviderStopReason::ToolCalls
    } else {
        ProviderStopReason::EndTurn
    };
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items,
        stop_reason,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Output-limit warnings must distinguish the one authorized continuation from
/// each terminal truncation shape without presenting an incomplete tool call as
/// executable.
#[test]
fn renderer_output_length_diagnostics_match_disposition_and_visible_output() {
    let cases = [
        (
            vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "retained private reasoning".to_owned(),
            })],
            tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&test_agent_prompt_id(
                    "length-planned",
                )),
                successor_agent_prompt_id: test_agent_prompt_id("length-successor"),
                ordinal: 1,
                limit: 1,
            },
            "Output limit reached; continuing once from retained reasoning.",
            None,
        ),
        (
            vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "terminal private reasoning".to_owned(),
            })],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit before completing the turn. No assistant answer or executable tool call was produced.",
            None,
        ),
        (
            vec![assistant_message_item("visible partial answer")],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit before completing the turn. The displayed response may be incomplete.",
            Some("visible partial answer"),
        ),
        (
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "truncated-call".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some("{".to_owned()),
                responses_envelope: None,
            })],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit while producing a tool call. The incomplete call was not executed.",
            None,
        ),
    ];

    for (index, (output_items, disposition, warning, visible_output)) in
        cases.into_iter().enumerate()
    {
        let (_term, handle, vt) = setup(160, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.apply_setting("show-thinking", "false");
        let mut finished = finished_response(&format!("length-{index}"), output_items);
        finished.stop_reason = ProviderStopReason::Length;
        finished.output_length_disposition = disposition;
        renderer.handle(&Event::ProviderResponseFinished(finished));
        sync(&handle);

        assert!(
            eventually_screen_contains(&vt, 160, warning),
            "missing output-limit warning in case {index}: {:?}",
            vt.screen_text(160)
        );
        if let Some(output) = visible_output {
            assert!(vt.screen_contains(160, output));
        }
        assert!(
            !vt.screen_contains(160, "retained private reasoning")
                && !vt.screen_contains(160, "terminal private reasoning")
        );
        assert_eq!(renderer.test_active_tool_count(), 0);
    }
}

/// Streaming response updates append text deltas rather than replacing a full
/// accumulated snapshot, so two chunks should render as one growing response.
#[test]
fn response_delta_updates_append_live_text() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hel",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "lo",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "HelHel"));
}

/// Queued prompts remain pending user input and must therefore use the same
/// hollow marker as the currently composed prompt, not the submitted marker.
#[test]
fn queued_prompt_uses_composing_marker() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued marker check".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "◯ queued marker check (queued)"));
    assert!(!vt.screen_contains(80, "⬤ queued marker check (queued)"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "queued marker check".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "⬤ queued marker check"));
    assert!(!vt.screen_contains(80, "◯ queued marker check"));
}

/// A terminal queue rejection must remove the stale queued marker and render
/// the actionable provider configuration failure.
#[test]
fn rejected_prompt_replaces_queued_marker_with_actionable_failure() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "prompt without providers".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptRejected(AgentPromptRejected {
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        message: "No provider models are available. Run `tau provider list`.".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "prompt without providers (queued)"));
    assert!(vt.screen_contains(100, "No provider models are available"));
    assert!(vt.screen_contains(100, "tau provider list"));
}

/// FIFO prompt terminals remove the corresponding oldest queued marker, so a
/// create-agent failure followed by an ordinary rejection cannot cross-remove
/// adjacent prompts.
#[test]
fn prompt_failure_and_rejection_remove_queued_markers_in_fifo_order() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-a".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    for text in ["initial A", "ordinary B"] {
        renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
            text: text.into(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
        }));
    }

    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-a".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-a".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed".into(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "initial A (queued)"));
    assert!(vt.screen_contains(100, "ordinary B (queued)"));

    renderer.handle(&Event::AgentPromptRejected(AgentPromptRejected {
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        message: "No provider models are available.".into(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "ordinary B (queued)"));
}

/// A failure routed to a hidden agent must not consume the visible agent's
/// queued marker.
#[test]
fn hidden_agent_prompt_failure_preserves_visible_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "visible queued".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "hidden-create".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("hidden"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hidden queued".into(),
        agent_id: agent_id("hidden"),
        message_class: tau_proto::PromptMessageClass::User,
    }));

    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "hidden-create".into(),
        agent_id: agent_id("hidden"),
        ctx_id: "hidden-ctx".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "hidden failed".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "visible queued (queued)"));
    assert!(!vt.screen_contains(100, "hidden queued (queued)"));
}

/// A non-requesting attached UI receives only the broadcast queued/failure
/// lifecycle and must still remove the failed initial marker.
#[test]
fn broadcast_only_initial_prompt_failure_removes_queue_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "initial from another UI".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "other-ui-create".into(),
        agent_id: agent_id("main"),
        ctx_id: "other-ui-ctx".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, "initial from another UI (queued)"));
    assert!(vt.screen_contains(100, "initial failed"));
}

/// A late failure for an initial prompt already promoted into submitted history
/// must not consume a newer ordinary queued marker.
#[test]
fn submitted_initial_prompt_failure_preserves_newer_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-a".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "initial A".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "initial A".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: Some("ctx-a".into()),
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-a".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-a".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "initial failed late".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// Queue ownership follows broadcast FIFO/class rather than mutable text, so
/// skill expansion or interception rewrites cannot strand an initial marker.
#[test]
fn rewritten_submitted_initial_failure_preserves_newer_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: ":skill expand-me".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "expanded skill body".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: Some("ctx-rewritten".into()),
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-rewritten".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-rewritten".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "rewritten initial failed".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, ":skill expand-me (queued)"));
    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// An internal initial prompt owns no visible queue block, so its terminal
/// cannot consume a later visible user marker.
#[test]
fn internal_initial_prompt_failure_preserves_visible_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::UiCreateAgentResult(
        tau_proto::UiCreateAgentResult {
            request_id: "create-internal".into(),
            session_id: test_session_id("s1"),
            outcome: tau_proto::UiCreateAgentOutcome::Created {
                agent_id: agent_id("main"),
                initial_prompt: tau_proto::UiCreateAgentInitialPrompt::Queued,
            },
        },
    ));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "internal A".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::Internal,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-internal".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-internal".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "internal failed".into(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// A multiline queued prompt occupies two rows while steering still promotes
/// the complete original text rather than the bounded presentation windows.
#[test]
fn queued_prompt_elides_at_layout_without_changing_authoritative_text() {
    let (_term, handle, vt) = setup(32, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let text =
        "First line with discarded tail\nmiddle line retained\nforgotten start end of last line.";

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: text.into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    let queued = vt.screen_text(32).join("\n");
    assert!(queued.contains("◯ First line with discarded"));
    assert!(queued.contains("┄"));
    assert!(queued.contains("last line. (queued)"));
    assert!(!queued.contains("middle line retained"));

    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        text: text.into(),
        trusted_internal_spans: Vec::new(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(32, "middle line retained"));
}

/// An assistant response must visibly transition from the hollow streaming
/// marker to the solid completed marker when its final event replaces the live
/// block.
#[test]
fn agent_response_marker_tracks_streaming_and_completed_states() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-marker",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-marker"),
            "marker answer",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "◇ marker answer"));
    assert!(!vt.screen_contains(80, "◆ marker answer"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-marker",
        vec![assistant_message_item("marker answer")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "◆ marker answer"));
    assert!(!vt.screen_contains(80, "◇ marker answer"));
}

/// Ensures streaming Markdown styles are applied as each line completes, so a
/// later blank-line seal does not restyle already-hidden scrollback and force a
/// full redraw.
#[test]
fn live_markdown_blank_line_seal_does_not_full_redraw_scrollback() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-md", "s1",
    )));
    for index in 0..24 {
        renderer.handle(&Event::ProviderResponseUpdated(
            provider_response_delta_update(
                test_agent_prompt_id("sp-md"),
                format!("*line {index}*\n"),
                None,
                tau_proto::PromptOriginator::User,
            ),
        ));
    }
    sync(&handle);
    assert!(vt.screen_contains(80, "*line 23*"));
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-md"),
            "\n",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

/// A UI that missed the prompt-created event can still route later deltas by
/// agent id and marks the live text with an ellipsis until the final response.
#[test]
fn late_response_delta_update_uses_ellipsis_prefix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-late"),
            "world",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "…world"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-late",
        vec![assistant_message_item("hello world")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "hello world"));
    assert!(!vt.screen_contains(80, "…world"));
}

/// Normal streaming after the prompt lifecycle was observed must not reuse the
/// late-subscription ellipsis prefix, which otherwise appears before the first
/// streamed assistant text.
#[test]
fn observed_response_delta_update_does_not_use_ellipsis_prefix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-observed",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-observed"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "hello"));
    assert!(!vt.screen_contains(80, "…hello"));
}

/// A provider retry/status reset hides reasoning from the failed attempt so
/// stale thinking does not remain visible while the replacement attempt runs.
#[test]
fn status_clear_response_removes_live_thinking_block() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "",
            Some("failed attempt thinking".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed attempt thinking"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
            retry: None,
        }),
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "retrying"));
    assert!(!vt.screen_contains(80, "failed attempt thinking"));
}

fn finished_response_with_usage(
    agent_prompt_id: &str,
    agent_id_value: &str,
    prompt_sent_tokens: u64,
    prompt_cached_tokens: u64,
    response_received_tokens: u64,
    text: &str,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_id: agent_id(agent_id_value),
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens,
            prompt_cached_tokens,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens,
            stats: tau_proto::TokenUsageStats {
                total: tau_proto::TokenUsageCounts {
                    sent_tokens: prompt_sent_tokens,
                    cached_tokens: prompt_cached_tokens,
                    received_tokens: response_received_tokens,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
        ..finished_response(agent_prompt_id, vec![assistant_message_item(text)])
    }
}

#[test]
fn first_agent_event_does_not_force_full_redraw() {
    // Regression: starting from the initial start-new-agent screen only changes
    // the input target. The already-visible empty transcript becomes the new
    // agent transcript in-place instead of replacing the whole output snapshot.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer_abc12345"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_abc12345"),
        ..agent_prompt_created("sp1", "s1")
    }));
    sync(&handle);
    assert_eq!(handle.full_render_count(), 0);
}

#[test]
fn new_agent_after_new_session_does_not_force_full_redraw() {
    // `:session new` intentionally moves to the start-new-agent screen and clears
    // the old transcript. Starting the next agent from that already-visible
    // empty screen should only update target/status metadata, not redraw
    // scrollback.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "first".into(),
        agent_id: tau_proto::AgentId::parse("engineer_one").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s2"),
        text: "second".into(),
        agent_id: tau_proto::AgentId::parse("engineer_two").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn new_session_initial_history_appends_to_first_agent() {
    // `:session new` can be reached after an explicit no-agent state, but the new
    // session's start screen is a fresh initial screen. Visible startup history
    // there should be adopted by the first agent instead of preserved as an
    // explicit no-agent snapshot.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("previous-agent".to_owned());
    renderer.clear_selected_agent();
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 88.into(),
        extension_name: tau_proto::ExtensionName::parse("std-session")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-session starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-session starting"));
    assert_eq!(handle.full_render_count(), full_render_count);

    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 88.into(),
        extension_name: tau_proto::ExtensionName::parse("std-session")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-session starting"));
    assert!(vt.screen_contains(80, "extension std-session ready"));
}

#[test]
fn delayed_clear_after_new_session_keeps_initial_history_adoptable() {
    // The input thread also queues a local ClearSelectedAgent when starting a
    // new session. If the remote SessionStarted(New) wins the race, that delayed
    // clear must not convert the fresh initial screen into an explicit protected
    // no-agent snapshot.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("previous-agent".to_owned());
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.clear_selected_agent();
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 89.into(),
        extension_name: tau_proto::ExtensionName::parse("std-race")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-race starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-race starting"));
    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn selecting_same_agent_does_not_force_full_redraw() {
    // Regression: selecting the already-displayed target agent is a pure no-op
    // for transcript rendering.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn switching_between_displayed_agents_restores_transcripts() {
    // The no-redraw fast path must not hide real transcript switches: moving
    // between two agents still swaps the output snapshot and restores each
    // agent's durable scrollback.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker one transcript".into(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.switch_agent("worker-2".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker two transcript".into(),
        agent_id: agent_id("worker-2"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "worker two transcript"));
    assert!(!vt.screen_contains(80, "worker one transcript"));
    let full_render_count = handle.full_render_count();

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "worker one transcript"));
    assert!(!vt.screen_contains(80, "worker two transcript"));
    assert!(handle.full_render_count() > full_render_count);
}

/// Ensures the redraw caused by an agent switch cannot combine the destination
/// transcript with the previously selected agent's input placeholder.
#[test]
fn agent_switch_first_frame_has_matching_transcript_and_placeholder() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let generation = vt.frame_generation();
    handle.with_redraw_suppressed(|| {
        renderer.switch_agent("worker-1".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "worker one transcript".into(),
            agent_id: agent_id("worker-1"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
        renderer.switch_agent("worker-2".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "worker two transcript".into(),
            agent_id: agent_id("worker-2"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    });
    let generation = vt.wait_for_frame_containing_after(generation, "worker two transcript");
    renderer.switch_agent_after_display_update_for_test("worker-1".to_owned(), || {
        handle.redraw_sync();
    });
    let frame = vt.wait_for_frame_after(generation);

    assert!(
        frame
            .iter()
            .any(|row| row.contains("worker one transcript")),
        "{frame:?}"
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("Write a message to worker-1"))
    );
    assert!(
        !frame
            .iter()
            .any(|row| row.contains("Write a message to worker-2"))
    );
}

/// Ensures clearing an agent selection paints the no-agent transcript boundary
/// and new-agent placeholder together in the clear operation's first frame.
#[test]
fn clear_selection_first_frame_has_new_agent_placeholder() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let generation = vt.frame_generation();
    handle.with_redraw_suppressed(|| {
        renderer.switch_agent("worker-1".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "selected agent transcript".into(),
            agent_id: agent_id("worker-1"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    });
    let generation = vt.wait_for_frame_containing_after(generation, "selected agent transcript");
    renderer.clear_selected_agent_after_display_update_for_test(|| handle.redraw_sync());
    let frame = vt.wait_for_frame_after(generation);

    assert!(
        !frame
            .iter()
            .any(|row| row.contains("selected agent transcript")),
        "{frame:?}"
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("Write a message to start a new agent"))
    );
    assert!(
        !frame
            .iter()
            .any(|row| row.contains("Write a message to worker-1"))
    );
}

/// Ensures the external prompt editor trailer is seeded from the visible
/// agent's response history, not from the most recent hidden agent response
/// processed by the renderer. It also preserves prompt-local editor fields that
/// are shared with the active input draft rather than with hidden transcripts.
#[test]
fn hidden_agent_response_does_not_replace_visible_editor_context() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "worker one response",
        ),
    ));
    {
        let editor_context = renderer.editor_context();
        let mut editor_context = editor_context.lock().expect("editor context");
        editor_context.previous_prompt = Some("visible previous prompt".to_owned());
        editor_context.edited_trailer_recovery = Some("visible recovery".to_owned());
    }

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-0",
            "worker-2",
            20_000,
            0,
            0,
            "worker two response",
        ),
    ));

    let visible_context = renderer.editor_context();
    let visible_context = visible_context.lock().expect("editor context").clone();
    assert_eq!(
        visible_context.last_response.as_deref(),
        Some("worker one response")
    );
    assert_eq!(visible_context.current_response, None);
    assert_eq!(
        visible_context.previous_prompt.as_deref(),
        Some("visible previous prompt")
    );
    assert_eq!(
        visible_context.edited_trailer_recovery.as_deref(),
        Some("visible recovery")
    );

    renderer.switch_agent("worker-2".to_owned());

    let worker_two_context = renderer.editor_context();
    let worker_two_context = worker_two_context.lock().expect("editor context").clone();
    assert_eq!(
        worker_two_context.last_response.as_deref(),
        Some("worker two response")
    );
    assert_eq!(
        worker_two_context.previous_prompt.as_deref(),
        Some("visible previous prompt")
    );
    assert_eq!(
        worker_two_context.edited_trailer_recovery.as_deref(),
        Some("visible recovery")
    );
}

/// Ensures the no-agent editor prompt context is not seeded with the last
/// selected agent's response and remains isolated from later hidden responses
/// owned by that old agent.
#[test]
fn clearing_selected_agent_clears_response_editor_context() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "worker one response",
        ),
    ));

    renderer.clear_selected_agent();
    let no_agent_context = renderer.editor_context();
    let no_agent_context = no_agent_context.lock().expect("editor context").clone();
    assert_eq!(no_agent_context.current_response, None);
    assert_eq!(no_agent_context.last_response, None);

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-1",
            "worker-1",
            20_000,
            0,
            0,
            "later hidden worker response",
        ),
    ));

    let no_agent_context = renderer.editor_context();
    let no_agent_context = no_agent_context.lock().expect("editor context").clone();
    assert_eq!(no_agent_context.current_response, None);
    assert_eq!(no_agent_context.last_response, None);
}

/// Ensures transcript re-rendering retains the bounded cache estimate after
/// switching away from and back to the agent that produced the response.
#[test]
fn switching_agents_preserves_estimated_cache_efficiency() {
    // Switching away and back must retain the bounded reusable-prefix estimate
    // when durable provider usage has no exact cache-read ceiling.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent("worker-1".to_owned());

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-0",
            "worker-1",
            20_000,
            0,
            0,
            "first worker response",
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-1-sp-1",
            "worker-1",
            20_100,
            19_000,
            0,
            "second worker response",
        ),
    ));
    renderer.switch_agent("worker-2".to_owned());
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "Δ95%? 19k/20k?"));
}

/// Ensures a hidden agent's cached response retains the same bounded estimate
/// when a later selection reconstructs that agent's transcript.
#[test]
fn switching_to_hidden_agent_preserves_estimated_cache_efficiency() {
    // Regression: hidden side-agent responses are recorded in that agent's UI
    // state and later replayed by a full transcript re-render when selected, so
    // they must retain the bounded reusable-prefix estimate.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent("worker-1".to_owned());

    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-0",
            "worker-2",
            20_000,
            0,
            0,
            "hidden first response",
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "worker-2-sp-1",
            "worker-2",
            20_100,
            19_000,
            0,
            "hidden second response",
        ),
    ));
    renderer.switch_agent("worker-2".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "Δ95%? 19k/20k?"));
}

/// Keeps turn-stat Σ totals with the response-owning agent while retaining a
/// separate flat total that cold-attach replay can reconstruct from every
/// durable provider terminal.
#[test]
fn turn_stats_and_session_stats_keep_token_scopes_separate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent("worker-1".to_owned());

    let mut first = finished_response_with_usage(
        "worker-1-sp-0",
        "worker-1",
        100,
        50,
        10,
        "first worker response",
    );
    first.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 100,
        cached_tokens: 50,
        received_tokens: 10,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(first));

    let mut other_agent = finished_response_with_usage(
        "worker-2-sp-0",
        "worker-2",
        700,
        600,
        80,
        "other worker response",
    );
    other_agent.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 800,
        cached_tokens: 650,
        received_tokens: 90,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(other_agent));

    let mut second = finished_response_with_usage(
        "worker-1-sp-1",
        "worker-1",
        50,
        20,
        5,
        "second worker response",
    );
    second.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 850,
        cached_tokens: 670,
        received_tokens: 95,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(second));
    sync(&handle);

    assert!(vt.screen_contains(80, "Σ↑70/150 ↓15"));
    assert!(!vt.screen_contains(80, "Σ↑670/850 ↓95"));
    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑670/850 ↓95"
    );
    renderer.show_session_token_stats();
    sync(&handle);
    assert!(vt.screen_contains(80, "session token totals: ↑670/850 ↓95"));
}

/// Replayed terminals after an existing-session switch must replace, rather
/// than add to, the flat totals from the former session.
#[test]
fn session_stats_reset_before_resumed_session_replay() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-a"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("agent-a-sp-0", "agent-a", 100, 90, 10, "session A response"),
    ));
    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑90/100 ↓10"
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-b"),
        reason: SessionStartReason::Resume,
    }));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("agent-b-sp-0", "agent-b", 7, 3, 2, "session B replay"),
    ));

    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑3/7 ↓2"
    );
}

#[test]
fn extension_context_ready_routes_to_agent_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "debug");
    renderer.handle(&Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("s1"),
            agent_id: agent_id("worker-1"),
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(80, "context ready"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "agent @worker-1 context ready"));
}

/// The initialization UI must use real line breaks, expose only advertised
/// skill names, aggregate other skills, and put each bootstrap path on its own
/// concise line with prompt-context size statistics but without leaking skill
/// descriptions.
#[test]
fn agent_context_initialization_summary_is_concise_and_literal() {
    let advertised = tau_proto::DiscoveryEffectiveSkill {
        name: "advertised".into(),
        description: "description must stay hidden".to_owned(),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    };
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: vec![advertised],
        agents_files: vec![
            tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/home/dpc/.config/agents/AGENTS.md".into(),
                lines: 10,
                bytes: 100,
            },
            tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/repo/AGENTS.md".into(),
                lines: 20,
                bytes: 200,
            },
        ],
    };

    let block =
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, 2);
    let spans = block.content.spans();
    let text = spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        "▤ initialized agent-1\nskills:\n  advertised 1L, 28B\n  2 other session skills available\nAGENTS.md:\n  /home/dpc/.config/agents/AGENTS.md 10L, 100B\n  /repo/AGENTS.md 20L, 200B"
    );
    assert!(!text.contains("\\n"));
    assert!(!text.contains("description must stay hidden"));
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::DarkCyan))
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        [" 1L, 28B", " 10L, 100B", " 20L, 200B"]
    );
}

/// Skill statistics describe only the advertised description injected into the
/// initial context, including UTF-8 bytes and multiple description lines.
#[test]
fn agent_context_initialization_skill_stats_measure_prompt_description() {
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: vec![
            tau_proto::DiscoveryEffectiveSkill {
                name: "focused".into(),
                description: "one\né".to_owned(),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            },
            tau_proto::DiscoveryEffectiveSkill {
                name: "empty".into(),
                description: String::new(),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            },
        ],
        agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
            file_path: "/empty/AGENTS.md".into(),
            lines: 0,
            bytes: 0,
        }],
    };

    let text =
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, 0)
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();

    assert_eq!(
        text,
        "▤ initialized agent-1\nskills:\n  focused 2L, 6B\n  empty 0L, 0B\nAGENTS.md:\n  /empty/AGENTS.md 0L, 0B"
    );
}

/// Empty sections stay omitted, while a singular aggregate remains grammatical.
#[test]
fn agent_context_initialization_summary_omits_empty_sections() {
    let initialized = tau_proto::HarnessAgentContextInitialized {
        session_id: test_session_id("session-1"),
        agent_id: agent_id("agent-1"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
            .expect("test identifier must be valid"),
        listed_skills: Vec::new(),
        agents_files: Vec::new(),
    };
    let text = |count| {
        crate::tool_render::agent_context_initialized_block(&cli_test_theme(), &initialized, count)
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>()
    };

    assert_eq!(text(0), "▤ initialized agent-1");
    assert_eq!(
        text(1),
        "▤ initialized agent-1\nskills:\n  1 other session skill available"
    );
}

/// The event renderer must combine the canonical session snapshot with the
/// agent-specific prompt projection in the observable terminal summary.
#[test]
fn agent_context_initialization_event_aggregates_session_skills() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let skill = |name: &str| tau_proto::DiscoveryEffectiveSkill {
        name: name.into(),
        description: format!("{name} description"),
        source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    };
    renderer.handle(&Event::HarnessSessionSkillsAvailable(
        tau_proto::HarnessSessionSkillsAvailable {
            session_id: test_session_id("session-1"),
            skills: vec![skill("advertised"), skill("other")],
        },
    ));
    renderer.switch_agent("agent-1".to_owned());
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id("agent-1"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init-1")
                .expect("test identifier must be valid"),
            listed_skills: vec![skill("advertised")],
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/repo/AGENTS.md".into(),
                lines: 20,
                bytes: 200,
            }],
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "initialized agent-1"));
    assert!(vt.screen_contains(80, "advertised"));
    assert!(vt.screen_contains(80, "1 other session skill available"));
    assert!(vt.screen_contains(80, "/repo/AGENTS.md"));
    assert!(vt.screen_contains(80, "advertised 1L, 22B"));
    assert!(vt.screen_contains(80, "/repo/AGENTS.md 20L, 200B"));
    assert!(!vt.screen_contains(80, "other description"));
}

/// Agent-specific discovery must remain in its owning transcript across
/// background delivery and repeated selection switches.
#[test]
fn agent_context_initialization_is_visible_only_in_selected_agent_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let initialized = |agent: &str, skill: &str, path: &str| {
        Event::HarnessAgentContextInitialized(tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id(agent),
            agent_initialization_id: tau_proto::AgentInitializationId::parse(format!(
                "{agent}-init"
            ))
            .expect("test identifier must be valid"),
            listed_skills: vec![tau_proto::DiscoveryEffectiveSkill {
                name: skill.into(),
                description: format!("{skill} description"),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            }],
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: path.into(),
                lines: 6,
                bytes: 550,
            }],
        })
    };

    renderer.switch_agent("agent-1".to_owned());
    renderer.handle(&initialized(
        "agent-1",
        "foreground-skill",
        "/one/AGENTS.md",
    ));
    renderer.handle(&initialized(
        "agent-2",
        "background-skill",
        "/two/AGENTS.md",
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "foreground-skill 1L, 28B"));
    assert!(vt.screen_contains(80, "/one/AGENTS.md 6L, 550B"));
    assert!(!vt.screen_contains(80, "background-skill"));
    assert!(!vt.screen_contains(80, "/two/AGENTS.md"));

    renderer.switch_agent("agent-2".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "background-skill 1L, 28B"));
    assert!(vt.screen_contains(80, "/two/AGENTS.md 6L, 550B"));
    assert!(!vt.screen_contains(80, "foreground-skill"));
    assert!(!vt.screen_contains(80, "/one/AGENTS.md"));

    renderer.switch_agent("agent-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "foreground-skill 1L, 28B"));
    assert!(!vt.screen_contains(80, "background-skill"));
}

/// Current-state discovery catch-up received before any selection must remain
/// hidden until the user selects its owning agent, as on restore or reattach.
#[test]
fn catch_up_agent_context_initialization_waits_for_agent_selection() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "debug");
    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "startup".into(),
        message: "startup output adopted by first agent".into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id("restored"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("restored-init")
                .expect("test identifier must be valid"),
            listed_skills: Vec::new(),
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: "/restored/AGENTS.md".into(),
                lines: 6,
                bytes: 550,
            }],
        },
    ));
    renderer.handle(&Event::ExtensionContextReady(
        tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("restored-init")
                .expect("test identifier must be valid"),
            session_id: test_session_id("session-1"),
            agent_id: agent_id("restored"),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(!vt.screen_contains(80, "/restored/AGENTS.md"));
    assert!(!vt.screen_contains(80, "context ready"));

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("restored"),
        ..agent_prompt_created("restored-prompt", "session-1")
    }));
    sync(&handle);
    assert_eq!(
        renderer
            .current_agent_state()
            .lock()
            .expect("current agent")
            .as_deref(),
        Some("restored")
    );
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(vt.screen_contains(80, "/restored/AGENTS.md 6L, 550B"));
    assert!(vt.screen_contains(80, "context ready"));
    let restored = vt.screen_text(80).join("\n");
    assert!(
        restored.find("/restored/AGENTS.md").expect("discovery row")
            < restored
                .find("context ready")
                .expect("later context-ready row")
    );

    renderer.switch_agent("background".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(!vt.screen_contains(80, "/restored/AGENTS.md"));

    renderer.switch_agent("restored".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "startup output adopted by first agent"));
    assert!(vt.screen_contains(80, "/restored/AGENTS.md 6L, 550B"));
}

/// Extension lifecycle completion must update the same snapshot that received
/// the starting block. Otherwise switching the viewed agent between
/// `extension.starting` and `extension.ready` leaves a stale starting line in
/// the old transcript and prints the ready line in an unrelated one.
#[test]
fn extension_lifecycle_completion_routes_to_starting_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent-a".to_owned());
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 7.into(),
        extension_name: tau_proto::ExtensionName::parse("std-test")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-test starting"));

    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 7.into(),
        extension_name: tau_proto::ExtensionName::parse("std-test")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-test ready"));
    assert!(!vt.screen_contains(80, "extension std-test starting"));

    renderer.switch_agent("agent-a".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-test ready"));
    assert!(!vt.screen_contains(80, "extension std-test starting"));
}

/// Extension lifecycle blocks that start on the initial no-agent screen are
/// part of the first agent conversation and must not be cleared by selecting
/// that first agent. This protects the startup/agent-selection flow from
/// redrawing away visible history before the conversation has really begun.
#[test]
fn initial_no_agent_extension_lifecycle_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));
    assert_eq!(handle.full_render_count(), full_render_count);

    renderer.handle(&Event::ExtensionExited(tau_proto::ExtensionExited {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
        exit_code: Some(1),
        signal: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global starting"));
    assert!(vt.screen_contains(80, "extension std-global exited"));
}

/// Extension lifecycle blocks on an explicitly cleared no-agent screen must
/// stay owned by that global snapshot. That state is different from process
/// startup: the user intentionally left an existing agent transcript, so fresh
/// agents should not inherit global no-agent output.
#[test]
fn explicit_no_agent_extension_lifecycle_routes_to_no_agent_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("previous-agent".to_owned());
    renderer.clear_selected_agent();
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));

    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global starting"));

    renderer.handle(&Event::ExtensionExited(tau_proto::ExtensionExited {
        instance_id: 8.into(),
        extension_name: tau_proto::ExtensionName::parse("std-global")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
        exit_code: Some(1),
        signal: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global exited"));
    assert!(!vt.screen_contains(80, "extension std-global starting"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global exited"));
    assert!(!vt.screen_contains(80, "extension std-global starting"));
}

/// Message facts for unavailable or invalid targets stay in the no-agent
/// snapshot, while facts for loaded targets belong to that target transcript.
#[test]
fn message_facts_route_to_owned_ui_snapshots_end_to_end() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let message_fact = |target: &str, message_id: &str, text: &str| {
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            tau_proto::MessageAgentTarget::new(target),
            tau_proto::MessageFactId::new(message_id),
            tau_proto::MessageParty {
                stable_id: "sender-1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            text,
        ))
    };

    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "unavailable-agent",
        "unavailable-message",
        "unavailable body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "unavailable body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "unavailable body"));
    assert!(vt.screen_contains(100, "for Tau target unavailable-agent"));
    renderer.switch_agent("fresh-after-global".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "unavailable body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "unavailable body"));

    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "../invalid",
        "invalid-message",
        "private invalid body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "Unprojectable message fact"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "Unprojectable message fact"));
    assert!(!vt.screen_contains(100, "private invalid body"));

    renderer
        .agent_navigation()
        .lock()
        .expect("agent navigation lock")
        .mark_live("loaded-agent");
    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "loaded-agent",
        "loaded-message",
        "loaded body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "loaded body"));
    renderer.switch_agent("loaded-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "loaded body"));
    assert!(!vt.screen_contains(100, "for Tau target loaded-agent"));
}

/// The compact delivered-message shape wraps naturally at narrow terminal
/// widths, preserves its immediate body, and styles only publisher provenance
/// as inline code.
#[test]
fn compact_message_fact_wraps_at_narrow_width_with_code_styled_publisher() {
    let (_term, handle, vt) = setup(28, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer
        .agent_navigation()
        .lock()
        .expect("agent navigation lock")
        .mark_live("selected-agent");
    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("fedi-slack")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("selected-agent"),
        tau_proto::MessageFactId::new("slack-message:opaque"),
        tau_proto::MessageParty {
            stable_id: "slack-sender:opaque".to_owned(),
            display_name: Some("Dawid (dpc)".to_owned()),
            sender_auth: None,
        },
        Some(tau_proto::MessageConversation {
            stable_id: "D123".to_owned(),
            display_name: Some("dpc-dm".to_owned()),
            alias: None,
        }),
        "Can you see this?",
    )));
    sync(&handle);

    let rows = vt.screen_text(28);
    assert!(rows.iter().any(|row| row.contains("External `fedi-slack`")));
    assert!(rows.iter().any(|row| row.contains("Can you see this?")));
    assert!(!rows.iter().any(|row| row.contains("slack-message:opaque")));
    assert!(!rows.iter().any(|row| row.contains("slack-sender:opaque")));
    assert!(!rows.iter().any(|row| row.contains("D123")));
    assert!(!rows.iter().any(|row| row.contains("Text:")));

    if !std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        let publisher_row = rows
            .iter()
            .position(|row| row.contains("`fedi-slack`"))
            .expect("publisher row") as u16;
        let publisher_text = &rows[publisher_row as usize];
        let publisher_byte = publisher_text.find("fedi-slack").expect("publisher column");
        let publisher_col = publisher_text[..publisher_byte].chars().count() as u16;
        let external_byte = publisher_text.find("External").expect("heading column");
        let external_col = publisher_text[..external_byte].chars().count() as u16;
        assert_ne!(
            vt.cell_style(publisher_row, publisher_col).0,
            vt.cell_style(publisher_row, external_col).0,
            "publisher should use the inline-code foreground; rows={rows:?}"
        );
    }
}

/// A replayed global fact on the initial no-agent screen remains owned by that
/// screen instead of being adopted into the first fresh agent transcript.
#[test]
fn initial_replayed_global_message_fact_survives_first_agent_selection() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("unavailable-agent"),
        tau_proto::MessageFactId::new("replayed-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "initial replay body",
    ));
    let delivery = tau_proto::EventDelivery::replay(tau_proto::UnixMicros::new(1_000_000), fact);
    let (event, replay, recorded_at) = delivery.into_parts();
    assert!(replay);
    renderer.handle_recorded_at(&event, recorded_at.expect("replay timestamp"));
    sync(&handle);
    assert!(vt.screen_contains(100, "initial replay body"));

    renderer.switch_agent("first-fresh-agent".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "initial replay body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "initial replay body"));
}

/// A live global fact received after explicit deselection stays in the
/// no-agent snapshot when the user switches to a never-cached agent.
#[test]
fn deselected_live_global_message_fact_survives_fresh_agent_selection() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("existing-agent".to_owned());
    renderer.clear_selected_agent();
    renderer.handle(&Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("unavailable-agent"),
        tau_proto::MessageFactId::new("live-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "deselected live body",
    )));
    sync(&handle);
    assert!(vt.screen_contains(100, "deselected live body"));

    renderer.switch_agent("never-cached-agent".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "deselected live body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "deselected live body"));
}

/// Dynamic action results must render in the transcript that was viewed when
/// the action was invoked. The result event itself has no agent id, so routing
/// it by the currently selected agent would leak output into whichever
/// transcript the user switched to while the extension was working.
#[test]
fn action_result_routes_to_invocation_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent-a".to_owned());
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-1")
            .expect("test identifier must satisfy its grammar"),
        Some("agent-a".to_owned()),
    );
    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-1")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "agent a action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "agent a action output"));

    renderer.switch_agent("agent-a".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "agent a action output"));
}

/// Dynamic action errors invoked from the initial no-agent screen are adopted
/// by the first selected agent, matching successful action output and startup
/// extension status.
#[test]
fn initial_no_agent_action_error_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.switch_agent("fresh-agent".to_owned());
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        message: "no-agent action failed".to_owned(),
        details: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "no-agent action failed"));
}

/// Dynamic action errors invoked after explicit deselection must not appear in
/// a later-selected agent transcript. This preserves the global/no-agent
/// snapshot boundary for extension action failures just like successful output.
#[test]
fn explicit_no_agent_action_error_routes_to_no_agent_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("previous-agent".to_owned());
    renderer.clear_selected_agent();
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.switch_agent("fresh-agent".to_owned());
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: tau_proto::ActionInvocationId::parse("action-2")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        message: "no-agent action failed".to_owned(),
        details: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "no-agent action failed"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "no-agent action failed"));
}

/// No-agent action output that arrives on the initial start-new-agent screen is
/// part of the first agent conversation and should remain visible when that
/// agent is selected.
#[test]
fn initial_no_agent_action_result_appends_to_first_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "visible no-agent action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));

    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));
}

/// No-agent action output that arrives after explicit deselection must be
/// snapshotted before switching to a fresh agent. Otherwise the fresh agent
/// would inherit global action output that was never scoped to it.
#[test]
fn explicit_no_agent_action_result_is_preserved_when_switching_to_fresh_agent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("previous-agent".to_owned());
    renderer.clear_selected_agent();
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        None,
    );
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-3")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "visible no-agent action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));

    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "visible no-agent action output"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(80, "visible no-agent action output"));
}

/// Removing a visible starting block must request a redraw even when the
/// matching completion is filtered by the current notice level. Without this,
/// the stale starting line stays on screen until some unrelated redraw happens.
#[test]
fn extension_lifecycle_removal_redraws_when_completion_is_filtered() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 9.into(),
        extension_name: tau_proto::ExtensionName::parse("std-filtered")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(789),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-filtered starting"));

    renderer.apply_setting("notice-level", "warning");
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 9.into(),
        extension_name: tau_proto::ExtensionName::parse("std-filtered")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(789),
    }));

    assert!(eventually_screen_lacks(
        &vt,
        80,
        "extension std-filtered starting"
    ));
    assert!(!vt.screen_contains(80, "extension std-filtered ready"));
}

#[test]
fn hidden_agent_events_do_not_force_visible_full_redraw() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    sync(&handle);
    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn agent_stats_does_not_overwrite_display_name() {
    // `:agent switch` completions are backed by durable display names. Agent stats
    // must not replace the display name chosen by the harness template.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-Ab12"),
        role: "engineer".to_owned(),
        display_name: Some("engineer: look it up".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer-Ab12"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    let display_names = renderer.agent_display_names();
    let display_names = display_names.lock().expect("display names");
    assert_eq!(
        display_names.get("engineer-Ab12").map(String::as_str),
        Some("engineer: look it up")
    );
}

/// Renderer stats forwarding preserves exact canonical costs, rejects foreign
/// sessions, and clears the picker projection across New and Resume switches.
#[test]
fn agent_cost_projection_tracks_renderer_session_authority() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let projection = renderer.agent_estimated_api_costs();
    let cost = tau_proto::EstimatedApiCost::from_picodollars(2_140_000_000_000);
    let stats = |session: &str, agent: &str| {
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id(session),
            agent_id: agent_id(agent),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state: tau_proto::AgentRuntimeState::Idle,
            turn_activity: tau_proto::AgentTurnActivity::Idle,
            tools: tau_proto::AgentToolStats::default(),
            context: tau_proto::AgentContextStats::default(),
            estimated_api_cost: cost,
            creator_subtree_estimated_api_cost: Default::default(),
            work_status: Default::default(),
        })
    };

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&stats("s1", "agent-a"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-a")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );
    renderer.handle(&stats("s2", "foreign"));
    assert!(!projection.snapshot().contains_key(&agent_id("foreign")));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    assert!(projection.snapshot().is_empty());
    renderer.handle(&stats("s2", "agent-b"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-b")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s3"),
        reason: SessionStartReason::Resume,
    }));
    assert!(projection.snapshot().is_empty());
    renderer.handle(&stats("s3", "agent-c"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-c")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );
}

/// Ensures prompt echoes, transcript facts, and terminal events never replace
/// complete harness stats as the CLI's navigation-cache authority.
#[test]
fn prompt_and_terminal_events_do_not_replace_navigation_snapshot() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "follow up".to_owned(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("worker-1"),
        text: "follow up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q-worker".to_owned(),
        text: "done".to_owned(),
        error: None,
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(
        navigation.mode("worker-1"),
        AgentNavigationState::ActiveAuto
    );
    assert!(navigation.is_active("worker-1"));
    drop(navigation);

    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(navigation.mode("worker-1"), AgentNavigationState::Active);
    assert!(navigation.is_active("worker-1"));
}

/// An accepted inference-activating user prompt must expose main-turn activity
/// before any prompt-id or provider event exists, so local provider warm-up
/// cannot leave the UI looking idle.
#[test]
fn accepted_prompt_submission_starts_main_turn_before_provider_activity() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "warm up locally".to_owned(),
        agent_id: agent_id("local-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    assert!(!renderer.main_agent_turn_active_for_test());
    sync(&handle);
    assert!(!vt.screen_contains(80, "◇ …"));

    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("local-agent"),
        text: "warm up locally".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));

    assert!(renderer.main_agent_turn_active_for_test());
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "◇ …"),
        "screen: {:?}",
        vt.screen_text(80)
    );

    renderer.handle_disconnect(Some("provider socket closed".to_owned()));
    sync(&handle);
    assert!(!vt.screen_contains(80, "◇ …"));
    assert!(vt.screen_contains(80, "provider socket closed"));
    assert!(!renderer.main_agent_turn_active_for_test());
}

/// Applies one complete authoritative navigation snapshot in renderer tests.
fn apply_test_navigation_mode(renderer: &mut EventRenderer, mode: tau_proto::AgentNavigationMode) {
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: mode,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
}

/// Ensures requester acknowledgements and diagnostics never become cache
/// authority; only a subsequent complete stats snapshot changes navigation.
#[test]
fn navigation_mode_results_do_not_mutate_cache() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Suspended);
    for outcome in [
        tau_proto::UiSetAgentNavigationModeOutcome::Applied,
        tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
            reason: tau_proto::UiSetAgentNavigationModeRejection::StaleSession,
        },
        tau_proto::UiSetAgentNavigationModeOutcome::Rejected {
            reason: tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded,
        },
    ] {
        renderer.handle(&Event::UiSetAgentNavigationModeResult(
            tau_proto::UiSetAgentNavigationModeResult {
                request_id: "result".to_owned(),
                session_id: test_session_id("s1"),
                agent_id: agent_id("worker-1"),
                outcome,
            },
        ));
        assert_eq!(
            renderer
                .agent_navigation()
                .lock()
                .expect("navigation")
                .mode("worker-1"),
            tau_proto::AgentNavigationMode::Suspended
        );
    }
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("navigation")
            .mode("worker-1"),
        tau_proto::AgentNavigationMode::Active
    );
}

/// Ensures placeholder copy distinguishes idle automatic hiding from an
/// unconditional manual suspension.
#[test]
fn selected_hidden_agent_placeholder_distinguishes_modes() {
    // Hidden selected agents remain viewable, and copy names the explicit
    // transition.
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "active-auto agent is idle"));
    // Exercise the placeholder-only navigation path: the operation must request
    // its own redraw even when no model-status block is present to do so.
    renderer.clear_model_status_for_test();
    sync(&handle);
    let generation = vt.frame_generation();
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Suspended);
    let generation = vt.wait_for_frame_containing_after(generation, "This agent is suspended");
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    vt.wait_for_frame_containing_after(generation, "Write a message to worker-1");
}

/// Ensures start-result delivery cannot replace canonical outer-turn runtime
/// state as the effective-activity authority.
#[test]
fn delegated_agent_effectiveness_follows_stats_not_start_result() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q-worker".to_owned(),
        text: "done".to_owned(),
        error: None,
    }));
    let navigation = renderer.agent_navigation();
    assert!(
        navigation
            .lock()
            .expect("agent navigation")
            .is_active("worker-1")
    );
}

/// Ensures prompt provenance cannot overwrite a harness-authored mode snapshot.
#[test]
fn extension_replay_reconstructs_active_auto_without_overwriting_override() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let prompt = AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("worker-1"),
        text: "side task".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    };
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::AgentPromptSubmitted(prompt.clone()));
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .mode("worker-1"),
        AgentNavigationState::ActiveAuto,
    );
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    renderer.handle(&Event::AgentPromptSubmitted(prompt));
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .mode("worker-1"),
        AgentNavigationState::Active,
    );
}

/// Ensures a delayed renderer refresh after unload cannot resurrect membership
/// or attach an override to a later same-id delegated endpoint.
#[test]
fn delayed_navigation_refresh_cannot_resurrect_unloaded_agent() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker-1".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    renderer.handle(&Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker-1"),
        },
    ));

    assert!(
        !renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .is_live("worker-1")
    );

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker-2".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(navigation.mode("worker-1"), AgentNavigationState::Active);
    assert!(!navigation.is_active("worker-1"));
}

#[test]
fn clearing_selected_agent_preserves_previous_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker transcript survives".into(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.clear_selected_agent();
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-helper".to_owned(),
        agent_id: agent_id("helper-1"),
    }));
    renderer.switch_agent("helper-1".to_owned());
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "worker transcript survives"));
}

#[test]
fn new_session_resets_agent_transcripts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s2"),
        reason: tau_proto::SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "@worker-1"));
    assert!(
        !renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .iter()
            .any(|agent| agent == "worker-1")
    );
}

#[test]
fn hidden_agent_activity_keeps_global_in_progress() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        ..finished_response("worker-sp", vec![assistant_message_item("done")])
    }));

    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn switched_agent_shows_its_tool_usage() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..finished_response(
            "worker-sp",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "worker-call".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/lib.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )
    }));
    renderer.handle_recorded_at(
        &tool_started(
            "worker-call",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("worker-call", "read", "src/lib.rs", ""),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "read src/lib.rs"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/lib.rs"));
}

/// Ensures a watched agent's stats stay owned by its watcher even while that
/// watcher is not the selected transcript, preventing a hidden-row redraw from
/// leaking into another agent view.
#[test]
fn watched_agent_stats_route_to_hidden_watcher_owner() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("worker-1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_started("ap-engineer_1-0", "s1")
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 1,
            started_total: 2,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(90, "❓💤 @engineer_1"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(90, "❓💤 @engineer_1"));
    assert!(vt.screen_contains(90, "%1/2"));
}

#[test]
fn shell_progress_routes_to_command_owner_after_agent_switch() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: test_session_id("s1"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
            .expect("test identifier must satisfy its grammar"),
        command: "printf worker-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.switch_agent("main".to_owned());

    renderer.handle(&Event::ShellCommandProgress(
        tau_proto::ShellCommandProgress {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
                .expect("test identifier must satisfy its grammar"),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "worker-output".into(),
            target_agent_id: Some(agent_id("worker-1")),
        },
    ));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf worker-output".into(),
            include_in_context: false,
            target_agent_id: Some(agent_id("worker-1")),
            output: "worker-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "worker-output"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(90, "worker-output"));
}

/// Replay-boundary abandonment removes only the unconfirmed lifecycle from
/// every renderer owner while preserving unrelated and current lifecycles.
#[test]
fn shell_replay_abandonment_covers_renderer_owners_and_collision() {
    let command = |id: &str, text: &str, target: Option<&str>| {
        Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: test_session_id("s1"),
            command_id: tau_proto::ShellCommandId::parse(id).expect("command id"),
            command: text.to_owned(),
            include_in_context: false,
            target_agent_id: target.map(agent_id),
        })
    };
    let abandoned = |id: &str, target: Option<&str>| ShellStartPresentation {
        command_id: tau_proto::ShellCommandId::parse(id).expect("command id"),
        target_agent_id: target.map(agent_id),
    };

    for (case, target, initially_selected, selected_after) in [
        ("visible", Some("worker"), Some("worker"), Some("worker")),
        ("hidden", Some("worker"), Some("main"), Some("worker")),
        ("no-agent", None, None, None),
    ] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: test_session_id("s1"),
            reason: tau_proto::SessionStartReason::Initial,
        }));
        if let Some(agent) = initially_selected {
            renderer.switch_agent(agent.to_owned());
        }
        renderer.handle(&command("shell-x", &format!("{case}-remove-X"), target));
        renderer.handle(&command("shell-y", &format!("{case}-retain-Y"), target));
        renderer.abandon_shell_starts(&[abandoned("shell-x", target)]);
        if let Some(agent) = selected_after {
            renderer.switch_agent(agent.to_owned());
        }
        sync(&handle);
        assert!(!vt.screen_contains(100, &format!("{case}-remove-X")));
        assert!(vt.screen_contains(100, &format!("{case}-retain-Y")));
    }

    // Targeted starts can be deferred behind initial discovery. Removing X
    // before agent selection must prevent it from resurrecting when the queue
    // flushes, while unrelated Y survives.
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("worker-init")
                .expect("initialization id"),
            listed_skills: Vec::new(),
            agents_files: Vec::new(),
        },
    ));
    renderer.handle(&command("shell-x", "deferred-remove-X", Some("worker")));
    renderer.handle(&command("shell-y", "deferred-retain-Y", Some("worker")));
    renderer.abandon_shell_starts(&[abandoned("shell-x", Some("worker"))]);
    renderer.switch_agent("worker".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "deferred-remove-X"));
    assert!(vt.screen_contains(100, "deferred-retain-Y"));

    // A colliding historical terminal renders standalone and does not consume
    // the active row subsequently settled by the live terminal.
    renderer.handle(&command(
        "shell-collision",
        "current-collision",
        Some("worker"),
    ));
    let terminal = tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-collision").expect("command id"),
        session_id: test_session_id("s1"),
        command: "historical-collision".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker")),
        output: "historical-output".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    renderer.handle_standalone_socket_shell_finished(&terminal, tau_proto::UnixMicros::new(1), 1);
    sync(&handle);
    assert!(vt.screen_contains(100, "historical-output"));
    assert!(vt.screen_contains(100, "current-collision"));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command: "current-collision".to_owned(),
            output: "current-output".to_owned(),
            ..terminal
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "historical-output"));
    assert!(vt.screen_contains(100, "current-output"));
}

#[test]
fn shell_command_target_field_survives_switch_before_echo_and_replay() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("main".to_owned());

    // Regression: the durable event's target must own the command even if the
    // selected transcript is main by the time the renderer processes the echo.
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: test_session_id("s1"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-race")
            .expect("test identifier must satisfy its grammar"),
        command: "printf race-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-race")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf race-output".into(),
            include_in_context: false,
            target_agent_id: Some(agent_id("worker-1")),
            output: "race-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "race-output"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(90, "race-output"));

    let (_term, handle, vt) = setup(90, 24);
    let mut replay = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    replay.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    replay.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-replay")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf replay-output".into(),
            include_in_context: true,
            target_agent_id: Some(agent_id("worker-1")),
            output: "replay-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "replay-output"));

    replay.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(90, "replay-output"));
}

#[test]
fn replay_learns_side_agent_from_durable_agent_prompt_submission() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("worker-1"),
            text: "side task".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: originator.clone(),
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response(
            "worker-sp",
            vec![assistant_message_item("worker replay answer")],
        )
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker replay answer"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker replay answer"));
    assert!(!vt.screen_contains(80, "&q-worker"));
}

#[test]
fn agent_switch_preserves_separate_transcripts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response("worker-sp", vec![assistant_message_item("worker answer")])
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker answer"));
    assert!(vt.screen_contains(80, "@worker-1"));

    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));
}

#[test]
fn deselect_then_first_prompt_for_new_agent_does_not_inherit_prior_transcript() {
    // Regression: `:agent none` must restore an empty no-agent screen. The
    // first prompt that selects a new agent from that state should render into
    // that agent's own fresh transcript rather than appending to the previously
    // selected agent's terminal output.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "agent one prompt".to_owned(),
        agent_id: agent_id("agent-one"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "agent one prompt"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(!vt.screen_contains(80, "agent one prompt"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "agent two prompt".to_owned(),
        agent_id: agent_id("agent-two"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "agent two prompt"));
    assert!(!vt.screen_contains(80, "agent one prompt"));
}

#[test]
fn queued_prompt_from_old_agent_does_not_steal_no_agent_selection() {
    // Regression: after `:agent new`, an already-running agent can still emit
    // queued/dequeued prompt events. Those background events must not reselect
    // the old agent while the user is typing the prompt meant to create a fresh
    // agent.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old agent prompt"));

    renderer.clear_selected_agent();
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued old-agent prompt".into(),
        agent_id: tau_proto::AgentId::parse("old-agent").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "stale old-agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("old-agent").expect("agent id"),
        ..agent_prompt_created("old-sp", "s1")
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "queued old-agent prompt"));
    assert!(!vt.screen_contains(80, "stale old-agent prompt"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("new-agent").expect("agent id"),
        ..agent_prompt_created("new-sp", "s1")
    }));
    sync(&handle);
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some("new-agent".to_owned())
    );
}

/// `:new` leaves the old agent running while the terminal shows the all-agent
/// overview. Its messages must appear there without selecting the sender, while
/// also remaining available in the sender's own transcript.
#[test]
fn old_agent_message_updates_overview_without_selecting_sender() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old agent prompt".to_owned(),
        agent_id: agent_id("old-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old agent prompt"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old agent prompt"));

    renderer.handle(&agent_message(
        "old-agent",
        "other-agent",
        "hidden old-agent message",
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from @old-agent to @other-agent"));
    assert!(vt.screen_contains(80, "hidden old-agent message"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent("old-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "hidden old-agent message"));
}

#[test]
fn queued_prompt_selects_agent_from_empty_state() {
    // Regression: replay can start with an already-queued user prompt. The UI
    // should treat that prompt as selecting the live agent, otherwise the next
    // Enter from the empty screen would create a new agent instead of targeting
    // the active one.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued live-agent prompt".into(),
        agent_id: tau_proto::AgentId::parse("live-agent").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "queued live-agent prompt"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some("live-agent".to_owned())
    );
}

#[test]
fn manual_compaction_selects_agent_from_empty_state() {
    // Regression: replay can expose a user-triggered compaction before any
    // prompt-created/submitted event. Even though manual compaction is not
    // rendered as progress, it still identifies the agent the empty UI should
    // target for subsequent input.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentCompactionTriggered(AgentCompactionTriggered {
        agent_id: tau_proto::AgentId::parse("live-agent").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
        resume_inference: false,
    }));
    sync(&handle);

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        Some("live-agent".to_owned())
    );
}

/// A submission epoch bump must suppress a snapshot already taken by the
/// debounce worker so cleared or submitted prompt text cannot arrive late.
#[test]
fn stale_draft_snapshot_is_dropped_after_submit_epoch_bump() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("s1"),
                target_agent_id: None,
                text: Some("old".into()),
            },
        ));
    }

    let (epoch, _draft) = {
        let (mtx, _cv) = &handle;
        super::locked(mtx).pending.take().expect("pending draft")
    };
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.pending = None;
    }

    assert!(!should_send_draft_snapshot(&handle, epoch));
}

/// Role-update parsing must keep explicit `off` distinct from clearing a field;
/// otherwise `:role <role> effort off` and `:role <role> thinking-summary off`
/// would accidentally reset the selected role instead of storing the user's
/// requested off state. `reset` is the only textual way to clear a setting.
#[test]
fn role_setting_updates_are_typed_and_reset_aware() {
    use std::num::NonZeroU8;

    use super::ui_commands::parse_role_setting_update;

    let tool_names = || {
        vec![
            tau_proto::ToolName::new("web_search"),
            tau_proto::ToolName::new("grep"),
        ]
    };
    let tool_group_names = || {
        vec![
            tau_proto::ToolGroupName::new("web"),
            tau_proto::ToolGroupName::new("shell"),
        ]
    };

    for (setting, value, expected) in [
        (
            "model",
            "openai/gpt-4o",
            UiRoleUpdateAction::SetModel {
                model: Some("openai/gpt-4o".parse().expect("valid model id")),
            },
        ),
        (
            "effort",
            "off",
            UiRoleUpdateAction::SetEffort {
                effort: Some(Effort::Off),
            },
        ),
        (
            "effort",
            "reset",
            UiRoleUpdateAction::SetEffort { effort: None },
        ),
        (
            "effort",
            "increase:2",
            UiRoleUpdateAction::AdjustEffort {
                adjustment: tau_proto::UiRoleSettingAdjustment::Increase(
                    NonZeroU8::new(2).expect("positive"),
                ),
            },
        ),
        (
            "verbosity",
            "high",
            UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
            },
        ),
        (
            "verbosity",
            "decrease",
            UiRoleUpdateAction::AdjustVerbosity {
                adjustment: tau_proto::UiRoleSettingAdjustment::Decrease(
                    NonZeroU8::new(1).expect("positive"),
                ),
            },
        ),
        (
            "thinking-summary",
            "off",
            UiRoleUpdateAction::SetThinkingSummary {
                thinking_summary: Some(ThinkingSummary::Off),
            },
        ),
        (
            "thinking-summary",
            "increase:3",
            UiRoleUpdateAction::AdjustThinkingSummary {
                adjustment: tau_proto::UiRoleSettingAdjustment::Increase(
                    NonZeroU8::new(3).expect("positive"),
                ),
            },
        ),
        (
            "service-tier",
            "fast",
            UiRoleUpdateAction::SetServiceTier {
                service_tier: Some(ServiceTier::Fast),
            },
        ),
        (
            "service-tier",
            "reset",
            UiRoleUpdateAction::SetServiceTier { service_tier: None },
        ),
        (
            "compaction-threshold",
            "85000",
            UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold: Some(85000),
            },
        ),
        (
            "compaction-threshold",
            "reset",
            UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold: None,
            },
        ),
        (
            "tools",
            "web_search,grep",
            UiRoleUpdateAction::SetTools {
                tools: Some(tool_names()),
            },
        ),
        (
            "tools",
            "reset",
            UiRoleUpdateAction::SetTools { tools: None },
        ),
        (
            "enable-tool-groups",
            "web,shell",
            UiRoleUpdateAction::SetEnableToolGroups {
                enable_tool_groups: tool_group_names(),
            },
        ),
        (
            "disable-tool-groups",
            "web,shell",
            UiRoleUpdateAction::SetDisableToolGroups {
                disable_tool_groups: tool_group_names(),
            },
        ),
        (
            "enable-tools",
            "web_search,grep",
            UiRoleUpdateAction::SetEnableTools {
                enable_tools: tool_names(),
            },
        ),
        (
            "enable-tools",
            "reset",
            UiRoleUpdateAction::SetEnableTools {
                enable_tools: Vec::new(),
            },
        ),
        (
            "disable-tools",
            "web_search,grep",
            UiRoleUpdateAction::SetDisableTools {
                disable_tools: tool_names(),
            },
        ),
        (
            "disable-tools",
            "reset",
            UiRoleUpdateAction::SetDisableTools {
                disable_tools: Vec::new(),
            },
        ),
    ] {
        assert_eq!(
            parse_role_setting_update(setting, value).expect("role setting parses"),
            expected,
            "{setting} {value}"
        );
    }

    assert!(parse_role_setting_update("service-tier", "off").is_err());
    assert!(parse_role_setting_update("compaction-threshold", "999").is_err());
    assert_eq!(
        parse_role_setting_update("unknown", "value").expect_err("unknown setting"),
        "unknown setting"
    );
}

/// Action submission uses the same epoch invalidation as ordinary prompt
/// submission so a pending command draft cannot publish after it runs.
#[test]
fn action_submission_invalidates_pending_draft_like_prompt_submission() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("s1"),
                target_agent_id: None,
                text: Some(":email list".into()),
            },
        ));
    }

    invalidate_pending_draft(&handle);

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    assert_eq!(slot.epoch, 1);
    assert!(slot.pending.is_none());
}

/// An explicitly content-enabled queued draft preserves the selected agent and
/// full buffer so subscribers can associate the opt-in text with its
/// transcript.
#[test]
fn queued_draft_snapshot_records_selected_agent_target() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_id = tau_proto::AgentId::parse("agent-a").expect("agent id");

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_id.clone()),
        "draft for agent".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, test_session_id("s1"));
    assert_eq!(draft.target_agent_id, Some(agent_id));
    assert_eq!(draft.text.as_deref(), Some("draft for agent"));
}

/// A default queued draft retains liveness and target metadata while omitting
/// the buffer so normal editing cannot expose prompt content to subscribers.
#[test]
fn queued_draft_snapshot_records_no_agent_target() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );

    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        None,
        "new agent draft".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, test_session_id("s1"));
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text, None);
}

/// Content-enabled retargeting invalidates the stale snapshot and preserves the
/// full buffer under the replacement viewed agent target.
#[test]
fn retarget_draft_snapshot_replaces_agent_a_with_agent_b() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    let agent_b = tau_proto::AgentId::parse("agent-b").expect("agent id");
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_a),
        "draft".to_owned(),
    );

    retarget_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_b.clone()),
        "draft".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, Some(agent_b));
    assert_eq!(draft.text.as_deref(), Some("draft"));
}

/// Content-enabled retargeting back to the new-agent prompt keeps its
/// replacement snapshot explicitly unscoped and contentful.
#[test]
fn retarget_draft_snapshot_replaces_agent_with_no_agent() {
    let handle = (
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    );
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    queue_prompt_draft_snapshot(
        &handle,
        test_session_id("s1"),
        Some(agent_a),
        "draft".to_owned(),
    );

    retarget_prompt_draft_snapshot(&handle, test_session_id("s1"), None, "draft".to_owned());

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text.as_deref(), Some("draft"));
}

/// A newly created draft epoch is eligible for the debounce worker until a
/// submission, retarget, or shutdown invalidates that exact snapshot.
#[test]
fn current_draft_snapshot_is_sent_when_epoch_matches() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );

    assert!(should_send_draft_snapshot(&handle, 0));
}

/// Shutdown makes even a current pending draft ineligible so the worker cannot
/// write an event after the UI has begun disconnecting.
#[test]
fn draft_snapshot_is_dropped_after_shutdown() {
    let handle = (
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    );
    {
        let (mtx, _cv) = &handle;
        super::locked(mtx).done = true;
    }

    assert!(!should_send_draft_snapshot(&handle, 0));
}

/// Prompt submission must win when it invalidates a draft after the worker's
/// initial epoch check but before writer acquisition; otherwise the old prompt
/// text can be published after the submission that cleared it.
#[test]
fn prompt_submission_suppresses_draft_validated_before_writer_acquisition() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let handle = Arc::new((
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    ));
    let draft = tau_proto::UiPromptDraft {
        session_id: test_session_id("s1"),
        target_agent_id: None,
        text: Some("stale".to_owned()),
    };
    let worker_writer = writer.clone();
    let worker_handle = handle.clone();
    let (validated_tx, validated_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        send_draft_snapshot_with_before_writer(
            &worker_writer,
            worker_handle.as_ref(),
            0,
            draft,
            || {
                validated_tx
                    .send(())
                    .expect("announce initial draft validation");
                continue_rx
                    .recv()
                    .expect("wait for invalidation and submission");
            },
        )
        .expect("draft send decision")
    });
    validated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker must pause after initial epoch validation");

    invalidate_pending_draft(handle.as_ref());
    let submitted = Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "submitted".to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    super::chat::send_event(&writer, &submitted).expect("send invalidating prompt");
    continue_tx.send(()).expect("release draft worker");

    assert!(!worker.join().expect("join draft worker"));
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));
    assert!(matches!(
        reader.read_message().expect("read prompt submission"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if emit.event.as_ref() == &submitted
    ));
}

/// The draft worker must send the first snapshot without waiting, then retain
/// only the latest edit until the next coalescing boundary instead of emitting
/// every queued buffer.
#[test]
fn draft_debounce_sends_immediately_then_coalesces_latest_snapshot() {
    let (ui_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let writer = Arc::new(Mutex::new(UiWriter::new(ui_stream, UiIoMeter::default())));
    let handle = Arc::new((
        Mutex::new(DraftSlot {
            send_content: true,
            ..DraftSlot::default()
        }),
        path_std_sync::Condvar::new(),
    ));
    let worker_handle = handle.clone();
    let (boundary_tx, boundary_rx) = mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        let mut first_boundary = true;
        debounce_loop_with_wait(worker_handle, writer, move |_| {
            if !first_boundary {
                return false;
            }
            first_boundary = false;
            boundary_tx.send(()).expect("first send reached boundary");
            continue_rx.recv().expect("advance coalescing boundary");
            true
        });
    });
    let mut reader = tau_proto::HarnessInputReader::new(BufReader::new(harness_stream));

    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "first".to_owned(),
    );
    boundary_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first draft must send before the coalescing boundary");
    assert!(matches!(
        reader.read_message().expect("read immediate draft"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if matches!(
                emit.event.as_ref(),
                Event::UiPromptDraft(draft) if draft.text.as_deref() == Some("first")
            )
    ));

    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "intermediate".to_owned(),
    );
    queue_prompt_draft_snapshot(
        handle.as_ref(),
        test_session_id("s1"),
        None,
        "latest".to_owned(),
    );
    continue_tx.send(()).expect("release coalescing boundary");
    assert!(matches!(
        reader.read_message().expect("read coalesced draft"),
        Some(tau_proto::HarnessInputMessage::Emit(emit))
            if matches!(
                emit.event.as_ref(),
                Event::UiPromptDraft(draft) if draft.text.as_deref() == Some("latest")
            )
    ));
    worker.join().expect("join draft worker");
}

/// `AgentMessage` events are normal history entries, not active blocks. They
/// must render for every sender/recipient pair, emphasize `@`-qualified routing
/// identities, and scroll away as history grows.
#[test]
fn agent_messages_render_all_recipients_as_history() {
    let (_term, handle, vt) = setup(120, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("manager_11111111"),
        role: "manager".to_owned(),
        display_name: Some("add-all-agent-overview for @engineer_22222222".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));

    renderer.handle(&agent_message(
        "manager_11111111",
        "engineer_22222222",
        "hello worker",
    ));
    sync(&handle);
    assert!(vt.screen_contains(
        120,
        "Message from @manager_11111111 (add-all-agent-overview for @engineer_22222222) to @engineer_22222222:"
    ));
    assert!(vt.screen_contains(120, "hello worker"));
    let lines = vt.screen_text(120);
    let row = lines
        .iter()
        .position(|line| line.contains("Message from @manager_11111111"))
        .expect("message header row") as u16;
    use unicode_width::UnicodeWidthStr as _;
    let sender_col = lines[row as usize][..lines[row as usize]
        .find("@manager_11111111")
        .expect("sender column")]
        .width() as u16;
    let recipient_col = lines[row as usize][..lines[row as usize]
        .rfind("@engineer_22222222")
        .expect("recipient column")]
        .width() as u16;
    assert!(vt.cell_style(row, sender_col).2);
    assert!(vt.cell_style(row, recipient_col).2);
    assert!(!vt.cell_style(row, sender_col - 1).2);
    let task_context_col = lines[row as usize][..lines[row as usize]
        .find("(add-all-agent-overview for @engineer_22222222)")
        .expect("task-name context column")]
        .width() as u16;
    assert!(!vt.cell_style(row, task_context_col).2);
    let context_id_col = lines[row as usize][..lines[row as usize]
        .find("@engineer_22222222")
        .expect("routing-id text inside task-name context")]
        .width() as u16;
    assert!(!vt.cell_style(row, context_id_col).2);

    for idx in 0..20 {
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: format!("scroll filler {idx}"),
            agent_id: agent_id("engineer_22222222"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    }
    sync(&handle);
    assert!(!vt.screen_contains(
        120,
        "Message from @manager_11111111 (add-all-agent-overview for @engineer_22222222) to @engineer_22222222:"
    ));
}

/// The no-agent screen aggregates one entry per semantic inter-agent message,
/// while sender and recipient transcripts retain their own projections.
/// Starting a new agent from the overview must not adopt that aggregate
/// history.
#[test]
fn no_agent_overview_deduplicates_agent_message_projections() {
    let (_term, handle, vt) = setup(96, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("sender-agent".to_owned());

    renderer.handle(&agent_message(
        "sender-agent",
        "recipient-agent",
        "overview semantic body",
    ));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-sender-agent-recipient-agent")
                .expect("test message id must satisfy its grammar"),
            sender_id: agent_id("sender-agent"),
            sender_session_id: None,
            recipient_id: agent_id("recipient-agent"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "overview semantic body".to_owned(),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert_eq!(
        visible_lines(&vt, 96)
            .iter()
            .filter(|line| line.contains("overview semantic body"))
            .count(),
        1
    );
    renderer.handle(&agent_message(
        "sender-agent",
        "third-agent",
        "live overview body",
    ));
    sync(&handle);
    assert!(vt.screen_contains(96, "live overview body"));
    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );

    renderer.switch_agent("recipient-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "start fresh from overview".to_owned(),
        agent_id: agent_id("fresh-agent"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(96, "start fresh from overview"));
    assert!(!vt.screen_contains(96, "overview semantic body"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(96, "overview semantic body"));
}

/// Structured current watch statuses remain exclusive to their watcher
/// transcript and do not appear in the no-agent message overview.
#[test]
fn no_agent_overview_excludes_structured_current_watch_status() {
    let (_term, handle, vt) = setup(100, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let provider_status_body = "watched provider is blocked";
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("watch-provider")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("watched-agent"),
            sender_session_id: None,
            recipient_id: agent_id("watcher-agent"),
            kind: tau_proto::AgentMessageKind::WatchProviderStatus,
            watch_provider_status: Some(tau_proto::AgentWatchProviderStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "sub-1".to_owned(),
                turn_generation: 1,
                agent_prompt_id: test_agent_prompt_id("prompt-1"),
                state: tau_proto::AgentWatchProviderState::Blocked {
                    category: tau_proto::AgentWatchProviderCategory::Account,
                },
                initial: false,
            }),
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: provider_status_body.to_owned(),
        },
    ));
    let long_wait_row = "▤ @watched-agent has been working for 5 minutes";
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("watch-long-wait")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("watched-agent"),
            sender_session_id: None,
            recipient_id: agent_id("watcher-agent"),
            kind: tau_proto::AgentMessageKind::WatchLongWait,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: Some(tau_proto::AgentWatchLongWaitNotification {
                session_id: test_session_id("s1"),
                subscription_id: "sub-1".to_owned(),
                status_epoch: 1,
                threshold_minutes: 5,
            }),
            watch_lifecycle: None,
            message: String::new(),
        },
    ));
    sync(&handle);
    let provider_status_row = format!("□ {provider_status_body}");
    assert!(!vt.screen_contains(100, &provider_status_row));
    assert!(!vt.screen_contains(100, long_wait_row));

    renderer.switch_agent("watcher-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, &provider_status_row));
    assert!(vt.screen_contains(100, long_wait_row));
}

/// Cross-session labels retain and emphasize the complete session-qualified
/// identity for grammar-valid controlled session identifiers.
#[test]
fn external_agent_messages_render_session_agent_labels() {
    let (_term, handle, vt) = setup(120, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&external_agent_message(
        "manager_11111111",
        "session-2",
        "engineer_22222222",
        "hello external",
    ));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-inbound-external")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("reviewer_33333333"),
            sender_session_id: Some(test_session_id("my_project-cafe-abc123")),
            recipient_id: agent_id("manager_11111111"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "hello back".to_owned(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(
        120,
        "Message from @manager_11111111 to session-2/@engineer_22222222:"
    ));
    assert!(vt.screen_contains(
        120,
        "Message from my_project-cafe-abc123/@reviewer_33333333 to @manager_11111111:"
    ));
    let lines = vt.screen_text(120);
    let row = lines
        .iter()
        .position(|line| line.contains("my_project-cafe-abc123/@reviewer_33333333"))
        .expect("external message header row") as u16;
    let session_suffix = lines[row as usize]
        .find("project-cafe")
        .expect("session suffix") as u16;
    let remote_agent = lines[row as usize]
        .find("@reviewer_33333333")
        .expect("remote agent id") as u16;
    assert!(vt.cell_style(row, session_suffix).2);
    assert!(vt.cell_style(row, remote_agent).2);
}

#[test]
fn show_messages_none_leaves_no_visible_message_output() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    sync(&handle);
    let before = visible_lines(&vt, 80);

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&agent_message("agent-a", "agent-b", "secret hidden body"));
    sync(&handle);

    assert_eq!(visible_lines(&vt, 80), before);
    assert!(!vt.screen_contains(80, "Message from"));
    assert!(!vt.screen_contains(80, "secret hidden body"));
}

#[test]
fn show_messages_summary_modes_do_not_show_body() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.apply_setting("show-messages", "all-summary");
    renderer.handle(&agent_message(
        "agent-a",
        "agent-b",
        "secret summarized body",
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "secret summarized body"));
}

/// Late display-name facts must replace an already rendered message label
/// without changing its body or leaving the stale block visible.
#[test]
fn late_agent_names_reproject_visible_message_blocks() {
    let (_term, handle, vt) = setup(100, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");
    let message = agent_message("agent-a", "agent-b", "semantic body");
    renderer.handle(&message);
    sync(&handle);
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b:"));

    let generation = vt.frame_generation();
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "review result".to_owned(),
        },
    ));
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a to @agent-b (review result):",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (review result):"));
    assert!(!vt.screen_contains(100, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(100, "semantic body"));
}

/// Hidden agent and no-agent transcript snapshots reproject current display
/// names when selected again instead of retaining event-time labels.
#[test]
fn hidden_message_snapshots_reproject_late_agent_names() {
    let (_term, handle, vt) = setup(100, 10);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");

    renderer.switch_agent("agent-a".to_owned());
    renderer.handle(&agent_message("agent-a", "agent-b", "agent history"));
    renderer.switch_agent("viewer".to_owned());
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "late worker".to_owned(),
        },
    ));
    sync(&handle);
    let generation = vt.frame_generation();
    renderer.switch_agent("agent-a".to_owned());
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a to @agent-b (late worker):",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (late worker):"));

    renderer.clear_selected_agent();
    renderer.handle(&agent_message(
        "agent-a",
        "agent-c",
        "overview message history",
    ));
    renderer.switch_agent("viewer".to_owned());
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-a"),
            display_name: "late sender".to_owned(),
        },
    ));
    sync(&handle);
    let generation = vt.frame_generation();
    renderer.clear_selected_agent();
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a (late sender) to @agent-c:",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a (late sender) to @agent-c:"));
    assert!(vt.screen_contains(100, "overview message history"));
}

/// Retained history keeps its originating session's name authority when a
/// different resumed session later publishes metadata for the same agent id.
#[test]
fn resumed_session_names_do_not_relabel_prior_message_history() {
    let (_term, handle, vt) = setup(100, 12);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-a"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "session A worker".to_owned(),
        },
    ));
    renderer.handle(&agent_message("agent-a", "agent-b", "session A body"));
    sync(&handle);
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (session A worker):"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-b"),
        reason: SessionStartReason::Resume,
    }));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "session B worker".to_owned(),
        },
    ));
    renderer.handle(&agent_message("agent-a", "agent-b", "session B body"));
    sync(&handle);

    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (session B worker):"));
    assert_eq!(
        vt.screen_text(100)
            .iter()
            .filter(|row| row.contains("session B worker"))
            .count(),
        1,
        "only the session-B message may use session-B metadata"
    );
    assert!(vt.screen_contains(100, "session A body"));
    assert!(vt.screen_contains(100, "session B body"));
}

#[test]
fn show_messages_toggle_retroactively_hides_and_shows_history() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&agent_message("agent-a", "agent-b", "retro body"));
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "all-full");
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "none");
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));
}

#[test]
fn new_session_clears_session_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("old response"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/lib.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            ),
            (
                CborValue::Text("content".into()),
                CborValue::Text("fn main() {}\n".into()),
            ),
        ]),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/lib.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old prompt"));
    assert!(vt.screen_contains(80, "old response"));
    assert!(vt.screen_contains(80, "read src/lib.rs"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "old prompt"));
    assert!(!vt.screen_contains(80, "old response"));
    assert!(!vt.screen_contains(80, "read src/lib.rs"));
    assert!(!vt.screen_contains(80, "&s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}
/// `notice-level=warning` hides routine informational chatter while mandatory
/// warnings such as configuration errors still reach the UI.
#[test]
fn warning_notice_level_hides_diagnostics_but_keeps_alerts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "warning");

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.info".into(),
        message: "routine lifecycle note".into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "routine lifecycle note"));

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.warning".into(),
        message: "important config error".into(),
        level: tau_proto::NoticeLevel::Warning,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "important config error"));
}

/// Compact mode hides diagnostics, preserves responses and alerts, and restores
/// diagnostics after a hidden-agent transcript round trip.
#[test]
fn compact_mode_reprojects_retained_notices_without_hiding_critical_errors() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());

    for (kind, message, level, purpose) in [
        (
            "test.info",
            "status reminder",
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Diagnostic,
        ),
        (
            "test.warning",
            "mandatory warning",
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Alert,
        ),
        (
            "test.error",
            "critical harness error",
            tau_proto::NoticeLevel::Critical,
            tau_proto::NoticePurpose::Diagnostic,
        ),
    ] {
        renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: kind.into(),
            message: message.into(),
            level,
            purpose,
        }));
    }
    sync(&handle);
    for visible in [
        "status reminder",
        "mandatory warning",
        "critical harness error",
    ] {
        assert!(vt.screen_contains(80, visible));
    }

    renderer.toggle_verbose_mode();
    renderer.switch_agent("worker".to_owned());
    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "status reminder"));
    assert!(vt.screen_contains(80, "mandatory warning"));
    assert!(vt.screen_contains(80, "critical harness error"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    for restored in [
        "status reminder",
        "mandatory warning",
        "critical harness error",
    ] {
        assert!(vt.screen_contains(80, restored));
    }
}

/// A new session discards compact-hidden no-agent notices, so toggling verbose
/// later cannot revive transcript state from the previous session.
#[test]
fn new_session_discards_compact_hidden_no_agent_notices() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice::diagnostic(
        "test.info",
        "old no-agent notice",
        tau_proto::NoticeLevel::Info,
    )));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old no-agent notice"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("new-session"),
        reason: SessionStartReason::New,
    }));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(80, "old no-agent notice"));
}

/// Locally synthesized manual-compaction acceptance notices follow the same
/// compact projection even when their target agent is currently hidden.
#[test]
fn compact_mode_reprojects_hidden_target_manual_compaction_notice() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.switch_agent("worker".to_owned());
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-hidden-notice", "call-hidden-notice"),
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "accepted compaction request"));

    renderer.toggle_verbose_mode();
    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "Agent main accepted compaction request"));
}

/// The interactive renderer preserves one directed tree notice as ordered,
/// distinct lines, including anchor spacing and the selected-head marker.
#[test]
fn tree_notice_renders_multiline_result_without_reformatting() {
    let (_term, handle, vt) = setup(240, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.apply_setting("notice-level", "critical");
    let expected = crate::test_support::TREE_PREVIEW_PARITY_NOTICE
        .lines()
        .collect::<Vec<_>>();

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: tau_proto::notice_kind::HARNESS_NOTICE.into(),
        message: crate::test_support::TREE_PREVIEW_PARITY_NOTICE.into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Response,
    }));
    sync(&handle);

    let rows = vt.screen_text(240);
    let mut previous_row = None;
    let mut rendered_tree_rows = Vec::new();
    for line in &expected {
        let row = rows
            .iter()
            .position(|row| row.contains(line))
            .unwrap_or_else(|| panic!("missing exact tree row {line:?} in {rows:?}"));
        assert!(
            previous_row.is_none_or(|previous| previous < row),
            "tree rows are out of order: {rows:?}"
        );
        previous_row = Some(row);
        rendered_tree_rows.push(
            rows[row]
                .strip_prefix("□ ")
                .unwrap_or(&rows[row])
                .trim_end(),
        );
    }
    assert_eq!(rendered_tree_rows, expected);
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("before first prompt") || row.contains("before prompt"))
            .count(),
        expected.len()
    );
}

#[test]
fn critical_notice_level_keeps_harness_failure_alert() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "critical");

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: tau_proto::notice_kind::HARNESS_FAILURE.into(),
        message: "failed to dispatch queued prompt: boom".into(),
        level: tau_proto::NoticeLevel::Warning,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed to dispatch queued prompt: boom"));
}

/// Extension ready/kept messages are informational lifecycle notices, so a
/// warning threshold should keep them out of live startup and `:session new`
/// preambles.
#[test]
fn warning_notice_level_hides_routine_extension_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "warning");

    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: tau_proto::ExtensionName::parse("core-shell")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "extension core-shell"));
}

/// Compact mode hides typed lifecycle diagnostics and restores their original
/// position when verbose mode returns.
#[test]
fn compact_mode_reprojects_extension_lifecycle_diagnostic() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: tau_proto::ExtensionName::parse("core-shell")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension core-shell"));

    renderer.switch_agent("worker".to_owned());
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(80, "extension core-shell"));

    renderer.apply_setting("notice-level", "warning");
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension core-shell"));
    renderer.apply_setting("notice-level", "info");
    sync(&handle);
    assert!(vt.screen_contains(80, "extension core-shell"));

    let themed =
        tau_themes::Theme::parse(r#"{ styles: { "extension.lifecycle": { fg: "red" } } }"#)
            .expect("theme parses");
    renderer.apply_theme(themed);
    sync(&handle);
    assert_rendered_ansi_foreground(&vt, 80, "extension core-shell", 9);
}

#[test]
fn new_session_preserves_role_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(100_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "+engineer"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(!vt.screen_contains(80, "&s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

#[test]
fn model_status_uses_symbol_prefixed_chips() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams {
            verbosity: Verbosity::High,
            ..Default::default()
        },
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("tau-agent-test"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(353_400),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(80)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row");
    assert!(status_row.starts_with("+engineer ~high"));
    assert!(status_row.ends_with("#353k/200k"));
    assert!(!vt.screen_contains(80, "=test/model"));
    assert!(!vt.screen_contains(80, "v=high"));
    assert!(!vt.screen_contains(80, "ctx:"));
}

/// Exact status boundaries must use terminal display width and drop the context
/// chip atomically before the identity, including for a wide Unicode role name.
#[test]
fn model_status_progressively_hides_at_ascii_and_unicode_boundaries() {
    let cases = [
        (17, "engineer", "+engineer", Some("#-/200k")),
        (16, "engineer", "+engineer", None),
        (11, "界", "+界", Some("#-/200k")),
        (10, "界", "+界", None),
    ];

    for (width, role, identity, context) in cases {
        let (_term, handle, vt) = setup(width, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("test/model".into()),
            context_window: Some(200_000),
            role: role.into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        sync(&handle);

        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains(identity))
            .unwrap_or_else(|| panic!("missing {identity:?} status row at width {width}"));
        assert_eq!(
            status_row.contains("#-/200k"),
            context.is_some(),
            "unexpected status row at width {width}: {status_row:?}"
        );
        assert_eq!(
            status_row.trim(),
            context.map_or_else(
                || identity.to_owned(),
                |context| format!("{identity} {context}")
            )
        );
    }
}

/// Optional diagnostics must stay absent when disabled and, when enabled, hide
/// the lower-value redraw counter before the more useful UI-I/O rates.
#[test]
fn model_status_debug_elements_follow_config_and_priority() {
    for width in [20, 22] {
        let (_term, handle, vt) = setup(width, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("test/model".into()),
            context_window: None,
            role: "engineer".into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        sync(&handle);
        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains("+engineer"))
            .expect("status row without diagnostics");
        assert!(!status_row.contains("io "));

        renderer.apply_setting("show-ui-io", "true");
        renderer.apply_setting("redraw-counter", "true");
        handle.invalidate_screen();
        sync(&handle);
        renderer.handle_ui_io_sample(super::event_renderer::UiIoStats {
            uplink_max_bytes_per_sec: 1024,
            downlink_max_bytes_per_sec: 2048,
        });
        sync(&handle);

        let full_render_count = handle.full_render_count();
        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains("+engineer"))
            .expect("status row with diagnostics");
        assert!(
            status_row.contains("io ↑1K ↓2K"),
            "UI-I/O diagnostics missing at width {width}: {status_row:?}"
        );
        assert_eq!(
            status_row.ends_with(&format!(" {full_render_count}")),
            width == 22,
            "unexpected redraw-counter retention at width {width}: {status_row:?}"
        );
    }
}

/// The compact quota status uses redundant ASCII text for every pacing state,
/// so no-color and narrow terminal users do not have to infer meaning from hue.
#[test]
fn quota_status_renders_all_accessible_compact_chips() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    let now = super::event_renderer::unix_time_millis();
    let cases = [
        (1, 1_000, 5_000, 0, "Q-"),
        (2, 5_000, 5_000, 0, "Q="),
        (3, 6_000, 5_000, 0, "Q+"),
        (4, 9_000, 5_000, 0, "Q!"),
        (5, 5_000, 5_000, 16 * 60 * 1_000, "Q?"),
    ];
    for (epoch, used, elapsed, age, expected) in cases {
        let remaining = 604_800_u64 * (10_000 - elapsed) / 10_000;
        renderer.handle(&Event::HarnessProviderQuotaChanged(
            tau_proto::HarnessProviderQuotaChanged {
                provider: model.provider.clone(),
                profile_epoch: tau_proto::ProviderQuotaEpoch::parse(format!("epoch-{epoch}"))
                    .expect("valid quota test value"),
                sequence: 1,
                windows: vec![tau_proto::ProviderQuotaWindow {
                    key: tau_proto::ProviderQuotaWindowKey {
                        limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                            .expect("valid quota test value"),
                        window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                            .expect("valid quota test value"),
                    },
                    used_basis_points: used,
                    usage_observed_at_unix_ms: now - age,
                    window_seconds: 604_800,
                    reset_at_unix_seconds: Some(now / 1_000 + remaining),
                    remaining_seconds_at_timing_anchor: Some(remaining as i64),
                    timing_anchor_observed_at_unix_ms: Some(now - age),
                    server_offset_ms: Some(0),
                    server_offset_observed_at_unix_ms: Some(now - age),
                }],
                route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
                    model: model.clone(),
                    limit_ids: vec![
                        tau_proto::ProviderQuotaLimitId::parse("codex")
                            .expect("valid quota test value"),
                    ],
                    observed_at_unix_ms: now - age,
                    provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
                }],
            },
        ));
        sync(&handle);
        assert!(vt.screen_contains(80, expected), "missing {expected}");
    }
}

/// The production lightweight prompt lifecycle supplies the selected agent's
/// model and repaints pacing whether quota catch-up arrives before or after it.
#[test]
fn selected_agent_quota_repaints_for_both_event_orderings() {
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    for quota_first in [true, false] {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("other/model".into()),
            context_window: None,
            role: "engineer".into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        renderer.handle(&Event::SessionStarted(SessionStarted {
            session_id: test_session_id("quota-order"),
            reason: SessionStartReason::Initial,
        }));
        let started = Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model: model.clone(),
            ..agent_prompt_started("quota-sp", "quota-order")
        });
        let quota = danger_quota_event(&model);

        if quota_first {
            renderer.handle(&quota);
            sync(&handle);
            assert!(!vt.screen_contains(80, "Q!"));
            renderer.handle(&started);
        } else {
            renderer.handle(&started);
            sync(&handle);
            assert!(!vt.screen_contains(80, "Q!"));
            renderer.handle(&quota);
        }
        sync(&handle);

        assert!(
            vt.screen_contains(80, "Q!"),
            "selected-agent quota was not repainted when quota_first={quota_first}"
        );
    }
}

/// A selected model keeps a neutral quota chip once its provider has advertised
/// quota capability, even when the initial current-state snapshot is empty.
#[test]
fn selected_agent_empty_quota_state_renders_unknown() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("other/model".into()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model: model.clone(),
        ..agent_prompt_started("quota-empty-sp", "quota-empty")
    }));
    renderer.handle(&Event::HarnessProviderQuotaChanged(
        tau_proto::HarnessProviderQuotaChanged {
            provider: model.provider.clone(),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-empty")
                .expect("quota epoch"),
            sequence: 1,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Q?"));
}

/// A narrow status line with the real default-plus-Bengalfox account shape
/// renders pacing from the exact `codex` binding and does not let the unrelated
/// additional pool's danger state override the selected route.
#[test]
fn quota_status_narrow_two_pool_state_uses_only_bound_default_pool() {
    let (_term, handle, vt) = setup(16, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    let now = super::event_renderer::unix_time_millis();
    let remaining = 604_800_u64 / 2;
    let window = |limit_id: &str, used_basis_points| tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: tau_proto::ProviderQuotaLimitId::parse(limit_id).expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
        },
        used_basis_points,
        usage_observed_at_unix_ms: now,
        window_seconds: 604_800,
        reset_at_unix_seconds: Some(now / 1_000 + remaining),
        remaining_seconds_at_timing_anchor: Some(remaining as i64),
        timing_anchor_observed_at_unix_ms: Some(now),
        server_offset_ms: Some(0),
        server_offset_observed_at_unix_ms: Some(now),
    };
    renderer.handle(&Event::HarnessProviderQuotaChanged(
        tau_proto::HarnessProviderQuotaChanged {
            provider: model.provider.clone(),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-two-pool")
                .expect("quota epoch"),
            sequence: 2,
            windows: vec![window("codex", 1_000), window("codex_bengalfox", 9_500)],
            route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
                model,
                limit_ids: vec![
                    tau_proto::ProviderQuotaLimitId::parse("codex").expect("default pool"),
                ],
                observed_at_unix_ms: now,
                provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
            }],
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(16)
        .into_iter()
        .find(|row| row.contains("Q-"))
        .expect("narrow status row");
    assert!(status_row.ends_with("Q-"));
    assert!(!vt.screen_contains(16, "Q!"));
}

#[test]
fn status_identity_matches_no_agent_placeholder_semantics() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    // In the no-agent/start-new-agent state, the status bar mirrors the prompt
    // placeholder by showing the selected role.
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row before agent selection");
    assert!(status_row.starts_with("+engineer"));
    assert!(!status_row.contains("@engineer_abc"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer_abc").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    // Once an agent is selected, the same slot switches from role to agent id.
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer_abc"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer_abc"));
    assert!(!status_row.contains("+engineer"));

    renderer.clear_selected_agent();
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after clearing agent selection");
    assert!(status_row.starts_with("+engineer"));
    assert!(!status_row.contains("@engineer_abc"));
}

/// An authoritative display name remains supplemental and visible when its text
/// equals the operational role; equality must not be treated as synthetic.
#[test]
fn status_agent_chip_keeps_id_primary_and_display_name_secondary() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-junior_b"),
        role: "engineer-junior".to_owned(),
        display_name: Some("engineer-junior".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer-junior_b").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer-junior_b"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer-junior_b (engineer-junior)"));
    assert!(!status_row.contains("@engineer-junior (engineer-junior_b)"));
}

/// A selected agent without an explicit display name must not show its role as
/// a synthesized parenthetical in the status bar.
#[test]
fn status_agent_chip_omits_parenthetical_for_unnamed_agent() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-junior_b"),
        role: "engineer-junior".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: agent_id("engineer-junior_b"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer-junior_b"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer-junior_b"));
    assert!(!status_row.contains("(engineer-junior)"));
    assert!(!status_row.contains("@engineer-junior_b ("));
}

/// Prevents reintroducing the `watched by:` prefix while preserving the watcher
/// id beside the selected-agent label required by
/// `SPEC-tau-cli-agent-message-labels`.
#[test]
fn status_agent_chip_shows_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer_child"),
        role: "engineer".to_owned(),
        display_name: Some("fix streaming ellipsis".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer_child").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager-AjhD"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(120)
        .into_iter()
        .find(|row| row.contains("@engineer_child"))
        .expect("status row after watch update");
    assert!(status_row.contains("@engineer_child (fix streaming ellipsis)"));
    assert!(status_row.contains("manager-AjhD"));
    assert!(!status_row.contains("watched by:"));
    assert!(!status_row.contains("child of"));
}

/// Prevents reintroducing the `watched by:` prefix while preserving the sorted
/// first watcher id and `+N more agents` truncation required by
/// `SPEC-tau-cli-agent-message-labels`.
#[test]
fn status_agent_chip_truncates_multiple_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    renderer.switch_agent("engineer_child".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager-AjhD"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("reviewer-Zz99"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(120)
        .into_iter()
        .find(|row| row.contains("@engineer_child"))
        .expect("status row after watch updates");
    assert!(status_row.contains("manager-AjhD, +1 more agents"));
    assert!(!status_row.contains("watched by:"));
    assert!(!status_row.contains("reviewer-Zz99"));
}

#[test]
fn model_status_shows_context_window_until_usage_is_known() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row");
    assert!(status_row.ends_with("#-/200k"));
}

#[test]
fn focused_agent_context_usage_event_replaces_unknown_context_window() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            input_tokens: Some(12_000),
            cached_tokens: Some(0),
            context_window: Some(200_000),
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row");
    assert!(status_row.ends_with("#12k/200k -/-"));
    assert!(!status_row.contains("#-/200k"));
}

/// Main-tool progress and context retain their relative order while quota
/// pacing occupies the final, rightmost status position.
#[test]
fn model_status_shows_main_tools_then_context_then_quota() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    renderer.handle(&danger_quota_event(&model));

    // Regression coverage for the bottom status bar: main-agent tool
    // usage should mirror generic tool progress chips (`%complete/total`)
    // and should render immediately before the context chip. Quota remains
    // final, while side-conversation calls stay rolled up under their delegate.
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("side-sp"),
        agent_id: tau_proto::AgentId::parse("q1").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "side-call".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after side response");
    assert!(status_row.ends_with("#12k/200k Q!"));
    assert!(!status_row.contains('%'));

    let mut created = agent_prompt_created("main-sp", "s1");
    created.model = model.clone();
    renderer.handle(&Event::AgentPromptCreated(created));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after main response");
    assert!(
        status_row.ends_with("%0/2 #12k/200k -/- Q!"),
        "unexpected status row: {status_row:?}"
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "side-call".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("side result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },

        display: None,
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after tool result");
    assert!(status_row.ends_with("%1/2 #12k/200k -/- Q!"));

    // Regression coverage for turn visibility: once an extension/sub-agent
    // prompt becomes active, it must not steal the main transcript's tool chip;
    // main progress stays visible while side-conversation tool calls remain
    // rolled up under their own delegate blocks.
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("q2").expect("agent id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q2".to_owned(),
        },
        ..agent_prompt_created("side-sp-2", "s1")
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after side prompt starts");
    assert!(
        status_row.ends_with("%1/2 @1 #12k/200k -/- Q!"),
        "unexpected status row: {status_row:?}"
    );
    assert!(status_row.contains('%'));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after second main tool result during side turn");
    assert!(status_row.ends_with("%2/2 @1 #12k/200k -/- Q!"));
    assert!(status_row.contains('%'));

    // Main tool completions that arrive while a side conversation is active
    // update the visible main counters. The side conversation's own tool usage
    // remains hidden from the main status chip.
    let mut follow_up = agent_prompt_created("main-follow-up-sp", "s1");
    follow_up.model = model;
    renderer.handle(&Event::AgentPromptCreated(follow_up));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after main prompt resumes");
    assert!(status_row.ends_with("%2/2 @1 #12k/200k -/- Q!"));

    // The main agent's final no-tool response ends the tool-using turn and
    // hides the chip while preserving context stats.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-final-sp",
        vec![assistant_message_item("done")],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after final main response");
    assert!(status_row.ends_with("@1 #12k/200k -/- Q!"));
    assert!(!status_row.contains('%'));

    // Starting a new user task in the same session also keeps the chip hidden
    // until the main agent requests tools for that task.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "next task".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after next prompt");
    assert!(status_row.ends_with("@1 #12k/200k -/- Q!"));
    assert!(!status_row.contains('%'));

    renderer.apply_setting("show-ui-io", "true");
    renderer.apply_setting("redraw-counter", "true");
    handle.invalidate_screen();
    sync(&handle);
    let full_render_count = handle.full_render_count();
    renderer.handle_ui_io_sample(super::event_renderer::UiIoStats {
        uplink_max_bytes_per_sec: 1024,
        downlink_max_bytes_per_sec: 2048,
    });
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row with optional diagnostics");
    assert!(
        status_row.ends_with(&format!(
            "@1 #12k/200k -/- io ↑1K ↓2K {full_render_count} Q!"
        )),
        "unexpected status row: {status_row:?}"
    );
}

/// The selected creator renders independent self and inclusive descendant
/// estimates from the complete stats snapshot rather than collapsing them.
#[test]
fn model_status_shows_selected_creator_cost_pair() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "cost-sp", "main",
    )));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("main"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: tau_proto::EstimatedApiCost::from_picodollars(
            2_140_000_000_000,
        ),
        work_status: Default::default(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(40, "@main"));
    assert!(vt.screen_contains(40, "$.00/$2.1"));
}

/// Estimated cost yields to the selected-agent identity under status-line width
/// pressure instead of wrapping or clipping either element.
#[test]
fn estimated_cost_status_hides_under_width_pressure() {
    let (_term, handle, vt) = setup(13, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("a"),
        ..agent_prompt_started("cost-sp", "s1")
    }));
    renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: agent_id("a"),
            input_tokens: None,
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: None,
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(13, "@a"));
    assert!(vt.screen_contains(13, "❓✨ @a"));
    assert!(!vt.screen_contains(13, "$"));
}

#[test]
fn agent_in_progress_ignores_completed_replayed_prompt_history() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // Late subscribers can replay historical UI submit and provider-finished
    // events without replaying the old AgentPromptCreated. That sequence is
    // already complete, so it must not leave Ctrl-D permanently guarded.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "old-sp",
        vec![assistant_message_item("old answer")],
    )));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn prompt_termination_clears_live_response_and_activity() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-stale", "s1",
    )));
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    // Regression: if the harness discards a stale provider response, it now
    // publishes this terminal lifecycle fact instead of leaving the UI's live
    // response block and Ctrl-D guard stuck forever.
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_prompt_id: test_agent_prompt_id("sp-stale"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        reason: AgentPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!vt.screen_contains(80, "…"));
}

/// An unknown ordinary prompt terminal must not be mistaken for a standalone
/// compaction merely because no local prompt state exists.
#[test]
fn unknown_ordinary_prompt_termination_does_not_render_compaction() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_prompt_id: test_agent_prompt_id("sp-unknown"),
        agent_id: agent_id("main"),
        reason: AgentPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let text = vt.screen_text(80).join("\n");
    assert!(!text.contains("compact"), "{text}");
}

/// Ensures provider response stats make the standalone live indicator
/// look active without entering the final transcript.
#[test]
fn provider_response_stats_update_suffixes_live_indicator_until_finish() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "sp-progress",
            tau_proto::AgentId::parse("main").expect("agent id"),
            0,
            0,
            1_000_000,
            0,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (1s, 0B, Δ0B/s, 0B/s)"),
        "pre-output stats samples must still refresh elapsed time: {:?}",
        vt.screen_text(80)
    );
    let mut first_output = main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024);
    first_output
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(first_output));
    sync(&handle);
    assert!(vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"));
    assert!(!vt.screen_contains(80, "shell_command"));
    assert!(!vt.screen_contains(80, "tool args"));
    assert!(!vt.screen_contains(80, "tools,"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-progress"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 1,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: "thinking".to_owned(),
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"),
        "updates without a fresh stats sample must not clear cached stats: {:?}",
        vt.screen_text(80)
    );

    let mut repeated = provider_response_stats_update(
        "sp-progress",
        tau_proto::AgentId::parse("main").expect("agent id"),
        12 * 1024,
        12 * 1024,
        3_000_000,
        2_000_000,
    );
    repeated
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(repeated));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 3s, 12KB, Δ0B/s, 4KB/s)"),
        "idle stats samples must show elapsed time, zero interval rate, and total rate: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-progress"),
        agent_id: agent_id("main"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
            retry: None,
        }),
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "… (820ms,"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-progress",
        Vec::new(),
    )));
    sync(&handle);
    assert!(!vt.screen_contains(80, "… (820ms,"));
}

/// Compact mode must hide both retained in-progress response statistics and a
/// newly completed turn-stat row, then restore each retained projection in
/// verbose mode.
#[test]
fn compact_mode_hides_live_and_new_turn_statistics() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-compact-stats",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-compact-stats", 12 * 1024, 4 * 1024),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "Δ8KB/s"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "Δ8KB/s"));

    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-compact-stats", 20 * 1024, 12 * 1024),
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "Δ8KB/s"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "… (2s, 20KB, Δ8KB/s, 10KB/s)"));
    assert!(!vt.screen_contains(100, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));

    renderer.toggle_verbose_mode();
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "sp-compact-stats",
            "main",
            20_000,
            10_000,
            500,
            "compact stats answer",
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact stats answer"));
    assert!(!vt.screen_contains(100, "Δ"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "Δ"));
}

/// First-output durations switch units at the approved five-second and
/// five-minute boundaries without rendering a placeholder for absence.
#[test]
fn first_output_duration_uses_compact_boundaries() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-first-output-format",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-first-output-format", 0, 0),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "… (2s, 0B, Δ0B/s, 0B/s)"));

    for (micros, expected) in [
        (4_999_999, "… (4999ms, 600s, 1B"),
        (5_000_000, "… (5s, 600s, 1B"),
        (299_999_999, "… (299s, 600s, 1B"),
        (300_000_000, "… (5m, 600s, 1B"),
    ] {
        let mut update = provider_response_stats_update(
            "sp-first-output-format",
            agent_id("main"),
            1,
            0,
            600_000_000,
            0,
        );
        update
            .response_stats
            .as_mut()
            .expect("response stats")
            .first_semantic_output_elapsed_micros = Some(micros);
        renderer.handle(&Event::ProviderResponseUpdated(update));
        sync(&handle);
        assert!(
            vt.screen_contains(100, expected),
            "missing {expected}: {:?}",
            vt.screen_text(100)
        );
    }
}

/// Ensures response-progress stats are scoped to the agent transcript that owns
/// the prompt rather than bleeding into the currently visible transcript.
///
/// A stats sample for a hidden agent must update only that hidden snapshot; the
/// user viewing another agent should not see the live response stats line
/// appear, disappear, or change because of background activity elsewhere.
#[test]
fn hidden_provider_response_stats_do_not_update_visible_response_indicator() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent_a".to_owned());
    let mut prompt_a = agent_prompt_created("ap-agent_a-0", "s1");
    prompt_a.agent_id = agent_id("agent_a");
    renderer.handle(&Event::AgentPromptCreated(prompt_a));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_a"),
            4 * 1024,
            0,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"));

    renderer.switch_agent("agent_b".to_owned());
    let mut prompt_b = agent_prompt_created("ap-agent_b-0", "s1");
    prompt_b.agent_id = agent_id("agent_b");
    renderer.handle(&Event::AgentPromptCreated(prompt_b));
    renderer.switch_agent("agent_a".to_owned());
    let mut hidden_update = provider_response_stats_update(
        "ap-agent_b-0",
        agent_id("agent_b"),
        12 * 1024,
        4 * 1024,
        2_000_000,
        1_000_000,
    );
    hidden_update
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(hidden_update));
    sync(&handle);

    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "visible agent A stats should remain unchanged: {:?}",
        vt.screen_text(80)
    );
    assert!(
        !vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "hidden agent B stats must not render in agent A's view: {:?}",
        vt.screen_text(80)
    );
    assert!(!vt.screen_contains(80, "… (820ms,"));

    renderer.switch_agent("agent_b".to_owned());
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"),
        "hidden stats should be visible when switching to their owning agent: {:?}",
        vt.screen_text(80)
    );
}

/// Ensures the no-agent fallback still accepts stats for a visible prompt it
/// already owns, while rejecting unrelated provider response stats.
///
/// A late provider update can create live response state before the UI has
/// selected or displayed an agent. The stats guard must preserve that supported
/// adoptable transcript path without letting other agents' stats leak into it.
#[test]
fn no_agent_visible_prompt_accepts_only_matching_response_stats() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("ap-agent_a-0"),
        agent_id: agent_id("agent_a"),
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_a"),
            4 * 1024,
            0,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "matching stats should update the visible no-agent prompt: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_b"),
            12 * 1024,
            4 * 1024,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "unrelated stats should leave the visible no-agent prompt unchanged: {:?}",
        vt.screen_text(80)
    );
    assert!(
        !vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "unrelated provider response stats must not render in the visible no-agent transcript: {:?}",
        vt.screen_text(80)
    );
}

/// Ensures a stale prompt-associated stats sample received after the final
/// provider response does not recreate an already-finished live response block.
#[test]
fn late_provider_response_stats_after_finish_does_not_recreate_live_indicator() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-progress",
        vec![assistant_message_item("done")],
    )));
    let mut stale = main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024);
    stale
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(stale));
    sync(&handle);

    assert!(vt.screen_contains(80, "done"));
    assert!(!vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));
    assert!(!vt.screen_contains(80, "… (820ms,"));
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!renderer.main_agent_turn_active_for_test());
}

/// Ensures visible assistant streaming remains content-focused: provider
/// response stats are not appended to the response text while text is visibly
/// active.
#[test]
fn provider_visible_update_omits_response_stats_suffix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-visible-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-visible-progress", 5, 0),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-visible-progress"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "Hello".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));
    assert!(!vt.screen_contains(80, "Hello … (2s, 5B, Δ5B/s, 2B/s)"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-visible-progress",
        vec![assistant_message_item("Hello")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "(2s, 5B, Δ5B/s, 2B/s)"));
}

/// Ensures cancellation clears active-tool state and preserves a producer's
/// generic attempt-history display while normalizing the terminal status.
#[test]
fn agent_in_progress_clears_when_tool_is_cancelled() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // ToolCancelled is a terminal tool event just like ToolResult/ToolError.
    // The Ctrl-D guard must clear it, otherwise a cancelled tool leaves the
    // session looking busy forever after the harness has stopped the tool.
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: Some(tau_proto::ToolUseState {
            args: "query: example".to_owned(),
            info_chips: vec!["✗ Exa → ⊘ Parallel".to_owned()],
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
    }));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    sync(&handle);
    assert!(vt.screen_contains(80, "✗ Exa → ⊘ Parallel cancelled"));
}

#[test]
fn delegate_side_conversation_keeps_parent_tool_status_visible() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started(
        "delegate-call",
        "agent_start",
        CborValue::Map(Vec::new()),
    ));

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q1".to_owned(),
        agent_id: agent_id("engineer_1"),
    }));

    // A running parent `agent_start` call is the visible main-agent work while
    // the sub-agent side conversation is active. The side agent is also active
    // while its delegated request is running. Regression coverage: the side
    // prompt lifecycle must not hide `%0/1` from the status bar, because
    // otherwise users lose the only bottom-bar indication that delegation is
    // still in progress.
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },
        ..agent_prompt_created("side-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            test_agent_prompt_id("side-sp"),
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("core-subagents")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "q1".to_owned(),
            },
        )
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row during delegate side conversation");
    assert!(status_row.ends_with("%0/1 @1 #12k/200k -/-"));

    // Generic watched-agent stats no longer mutate the parent tool status chip.
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 2,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("#12k/200k"))
        .expect("status row after watched-agent stats");
    assert!(status_row.contains("@main"));
    assert!(status_row.ends_with("%0/1 @1 #12k/200k -/-"));

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q1".to_owned(),
        text: String::new(),
        error: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q2".to_owned(),
        },
        ..agent_prompt_created("later-side-sp", "s1")
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after delegate cancellation");
    assert!(status_row.ends_with("@1 #12k/200k -/-"));
}

#[test]
fn role_default_knobs_are_hidden_and_overrides_follow_role() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=medium, verbosity=medium, thinking-summary=auto"
                .to_owned(),
            role_description: None,
            details: None,
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(!vt.screen_contains(80, "^medium"));
    assert!(!vt.screen_contains(80, "~medium"));

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ~high"));
}

/// Role availability should feed `:new` argument completion as well as
/// `:role`, because `:new <role>` is the fast path for opening a fresh
/// no-agent input target that will create the next agent with that role.
#[test]
fn new_command_completes_available_roles() {
    let (_term, handle, _vt) = setup(80, 24);
    let completion_data = tau_cli_term::CompletionData::new();
    let mut renderer = EventRenderer::new(handle, completion_data.clone(), cli_test_theme());

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![
            HarnessRoleInfo {
                name: "engineer".to_owned(),
                description: "write production code".to_owned(),
                role_description: None,
                details: None,
            },
            HarnessRoleInfo {
                name: "reviewer".to_owned(),
                description: "review code changes".to_owned(),
                role_description: None,
                details: None,
            },
        ],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));

    let candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(":new", "new agent")],
        &completion_data,
        ":new rev",
        ":new rev".len(),
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].label, "reviewer");
    assert_eq!(candidates[0].replacement, ":new reviewer");
}

/// Role candidates hide every tool-policy fragment in their display
/// description, while their matching, insertion text, and subsequent
/// tool-setting completions remain available for inspecting and editing the
/// configured policy.
#[test]
fn role_completion_labels_hide_tool_policy_without_hiding_tool_settings() {
    let (_term, handle, _vt) = setup(80, 24);
    let completion_data = tau_cli_term::CompletionData::new();
    let mut renderer = EventRenderer::new(handle, completion_data.clone(), cli_test_theme());
    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "unused structured details".to_owned(),
            role_description: Some("production implementation".to_owned()),
            details: Some(tau_proto::HarnessRoleDetails {
                inference_compaction: None,
                compactions: Vec::new(),
                model: Some("provider/model".into()),
                params: tau_proto::ModelParams {
                    effort: Effort::High,
                    verbosity: Verbosity::Low,
                    thinking_summary: ThinkingSummary::Concise,
                    service_tier: Some(ServiceTier::Fast),
                },
                tools: Some(vec![tau_proto::ToolName::new("read")]),
                enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
                disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
                enable_tools: vec![tau_proto::ToolName::new("web_search")],
                disable_tools: vec![tau_proto::ToolName::new("shell")],
            }),
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));

    let role_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(
            ":role",
            "configure role",
        )],
        &completion_data,
        ":role eng",
        ":role eng".len(),
    );
    assert_eq!(role_candidates.len(), 1);
    assert_eq!(role_candidates[0].label, "engineer");
    assert_eq!(
        role_candidates[0].description,
        "provider/model e=high v=low ts=concise st=fast — production implementation"
    );
    assert_eq!(role_candidates[0].replacement, ":role engineer");

    let new_role_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(":new", "new agent")],
        &completion_data,
        ":new eng",
        ":new eng".len(),
    );
    assert_eq!(new_role_candidates.len(), 1);
    assert_eq!(
        new_role_candidates[0].description,
        "provider/model e=high v=low ts=concise st=fast tools=read etg=pim dtg=shell et=web_search dt=shell — production implementation"
    );

    let tool_setting_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(
            ":role",
            "configure role",
        )],
        &completion_data,
        ":role engineer ",
        ":role engineer ".len(),
    );
    let labels_and_descriptions = tool_setting_candidates
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.label.as_str(),
                "tools"
                    | "enable-tool-groups"
                    | "disable-tool-groups"
                    | "enable-tools"
                    | "disable-tools"
            )
        })
        .map(|candidate| (candidate.label.as_str(), candidate.description.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        labels_and_descriptions,
        vec![
            ("tools", "read"),
            ("enable-tool-groups", "pim"),
            ("disable-tool-groups", "shell"),
            ("enable-tools", "web_search"),
            ("disable-tools", "shell"),
        ]
    );
}

#[test]
fn role_state_overrides_are_compared_to_role_baseline() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // HarnessRolesAvailable describes the current role including
    // persisted state overrides. The status bar must use the role/provider
    // baseline from HarnessRoleSelected instead.
    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=low, verbosity=high, thinking-summary=auto"
                .to_owned(),
            role_description: None,
            details: None,
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: None,
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Low,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: Some(tau_proto::ServiceTier::Fast),
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s3"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ^low ~high !off"));
}

#[test]
fn single_prompt_response_cycle() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // User submits prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "> hello"));

    // Harness creates agent prompt.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    // Agent streams response.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hi there!",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hi there!"));

    // Agent finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hi there! How can I help?")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hi there! How can I help?"),
        "final response should be visible, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn thinking_renders_as_separate_block_above_response() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-0", "s1")
    }));
    sync(&handle);

    // Thinking arrives before the response text. Both should be
    // visible simultaneously, with thinking above response.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            String::new(),
            Some("planning the answer".into()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "planning the answer"),
        "thinking block should be live: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "actual answer",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "actual answer"));
    assert!(vt.screen_contains(80, "planning the answer"));

    // Order matters even during live streaming: thinking should
    // render ABOVE the response, not below it.
    let live = vt.screen_text(80);
    let live_thinking = live
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("live thinking missing: {live:?}"));
    let live_response = live
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("live response missing: {live:?}"));
    assert!(
        live_thinking < live_response,
        "live thinking should render above live response (thinking @ {live_thinking}, response @ {live_response}); lines: {live:?}",
    );

    // On finish both stick in history.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("actual answer")],
    )));
    sync(&handle);
    // Thinking should appear above the response in the history.
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("thinking should remain in history: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("response should remain in history: {lines:?}"));
    assert!(
        thinking_row < response_row,
        "thinking should render above response (thinking @ {thinking_row}, response @ {response_row}); lines: {lines:?}",
    );
}

#[test]
fn set_show_thinking_round_trip_restores_history() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "the_response",
            Some("the_thinking_text".into()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("the_response")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "the_thinking_text"));
    assert!(vt.screen_contains(80, "the_response"));

    // Off — thinking content disappears, no placeholder, no
    // blank row left behind: the response should be on the same
    // row as the (now-empty) thinking block sat before. We assert
    // this indirectly by counting non-blank lines.
    let lines_before = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    renderer.apply_setting("show-thinking", "false");
    sync(&handle);
    assert!(!vt.screen_contains(80, "the_thinking_text"));
    assert!(!vt.screen_contains(80, "thinking hidden"));
    assert!(vt.screen_contains(80, "the_response"));
    let lines_after = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    // Hiding the one thinking block should remove exactly one
    // visible line of content from the screen.
    assert_eq!(lines_after + 1, lines_before);

    // Back on — original thinking text returns in its original
    // position above the response.
    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("the_thinking_text"))
        .unwrap_or_else(|| panic!("thinking should reappear: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("the_response"))
        .unwrap_or_else(|| panic!("response should still be visible: {lines:?}"));
    assert!(thinking_row < response_row);
}

#[test]
fn thinking_created_while_off_stays_invisible_after_toggle_on() {
    // Blocks that arrive while `show_thinking == false` are
    // never rendered and never tracked, so toggling back on
    // doesn't suddenly resurrect them. Only blocks that were
    // visible at some point round-trip through `set_block`.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-thinking", "false");

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("answer")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "answer"));
    assert!(!vt.screen_contains(80, "hidden reasoning"));

    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "hidden reasoning"),
        "blocks created while off should not appear after toggle on"
    );
}

#[test]
fn no_thinking_block_when_summary_absent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello")],
    )));
    sync(&handle);
    // Just make sure we didn't crash and the response is visible.
    assert!(vt.screen_contains(80, "hello"));
}

#[test]
fn queued_prompt_renders_after_first_completes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // First prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "first".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Regression: the production busy-submit path immediately publishes
    // only `AgentPromptQueued`; there may be no preceding local
    // `UiPromptSubmitted` echo for the renderer to replace. The queued
    // event itself must make the user's prompt visible.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "second".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "second (queued)"),
        "queued indicator should show, got: {:?}",
        vt.screen_text(80)
    );

    // First finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("response one")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "response one"));

    // Second dispatched — "(queued)" should be removed.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-1", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "(queued)"),
        "queued indicator should be gone after dispatch, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> second"),
        "dispatched prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("second"))
            .count(),
        1,
        "queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-1"),
            "response two",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two"),
        "second response should stream, got: {:?}",
        vt.screen_text(80)
    );

    // Second finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item("response two complete")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two complete"),
        "final second response should show, got: {:?}",
        vt.screen_text(80)
    );
    // First response should still be visible.
    assert!(
        vt.screen_contains(80, "response one"),
        "first response should still show, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_then_late_ui_submit_advances_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: replay/late-subscribe paths can observe a queued event before
    // the matching UI submit. The submit must promote the queued marker to one
    // normal transcript item rather than leaving stale "(queued)" text behind.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "late echo".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "late echo".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "late echo (queued)"));
    assert!(vt.screen_contains(80, "> late echo"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("late echo"))
            .count(),
        1,
        "created queued prompt should be promoted once, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_steered_promotes_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: steering folds a queued prompt into the in-flight turn
    // immediately, without a later `AgentPromptCreated`. The queued
    // marker should therefore be promoted in place to one normal user
    // prompt instead of lingering or duplicating.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "folded queued prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should show before steering, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        text: "folded queued prompt".into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should be gone after steering, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> folded queued prompt"),
        "steered prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("folded queued prompt"))
            .count(),
        1,
        "steered queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );
}

/// Extension provenance wins over an ambiguous queue-text match, preserving the
/// queued user projection and rendering the extension's canonical fact once.
#[test]
fn extension_steering_does_not_promote_matching_queued_user_prompt() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());

    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "queued extension collision".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::Extension {
            name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
        },
        text: "queued extension collision".into(),
        trusted_internal_spans: Vec::new(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "queued extension collision (queued)"));
    assert!(vt.screen_contains(80, "■ External `fixture` message:"));
    assert!(!vt.screen_contains(80, "⬤ queued extension collision"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("queued extension collision"))
            .count(),
        2,
        "queue and authenticated extension projections must remain distinct: {:?}",
        vt.screen_text(80)
    );
}

/// An extension message cannot consume a queued user prompt merely because its
/// payload matches a later queued row.
#[test]
fn nonfront_queued_match_remains_a_message_without_consuming_the_front_prompt() {
    for submission_source in [tau_proto::PromptSubmissionSource::Extension {
        name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
    }] {
        let (_term, handle, vt) = setup(80, 24);
        let mut renderer = marker_test_renderer(handle.clone());
        for text in ["first queued user prompt", "second queued user prompt"] {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: text.to_owned(),
                agent_id: agent_id("main"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
        renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: false,
            submission_source,
            text: "second queued user prompt".to_owned(),
            trusted_internal_spans: Vec::new(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            ctx_id: None,
        }));
        sync(&handle);

        assert!(vt.screen_contains(80, "◯ first queued user prompt (queued)"));
        assert!(!vt.screen_contains(80, "⬤ first queued user prompt"));
        assert!(vt.screen_contains(80, "■ External `fixture` message:"));
        assert!(!vt.screen_contains(80, "⬤ second queued user prompt"));
    }
}

/// Human UI provenance promotes its front-exact queued projection before the
/// subsequent start event can duplicate it.
#[test]
fn submitted_human_prompt_promotes_matching_front_queue_before_start() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    let text = "accepted queued prompt";
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: text.to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "accepted-queued",
        "s1",
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "⬤ accepted queued prompt"));
    assert!(!vt.screen_contains(80, "■ accepted queued prompt"));
    assert!(!vt.screen_contains(80, "accepted queued prompt (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains(text))
            .count(),
        1,
        "submitted queued prompt must render once: {:?}",
        vt.screen_text(80)
    );
}

/// Harness-typed active and passive background completion notices stay out of
/// the terminal even when the operator enables other internal prompt
/// diagnostics.
#[test]
fn typed_background_completion_prompts_are_always_hidden() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "Tool call `idle` completed. Its result is queued; use `wait` to consume it.".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.apply_setting("show-internal-prompts", "off");
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: "Tool call `steered` completed. Its result is queued; use `wait` to consume it."
            .into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        ctx_id: None,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "Tool call `idle`"));
    assert!(!vt.screen_contains(80, "Tool call `steered`"));
    assert!(
        vt.screen_text(80)
            .iter()
            .all(|row| !row.contains("Tool call"))
    );

    let (_cold_term, cold_handle, cold_vt) = setup(80, 24);
    let mut cold = EventRenderer::new(
        cold_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    cold.apply_setting("show-internal-prompts", "on");
    cold.handle_recorded_at(
        &Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("main"),
            text: "Tool call `replayed` completed. Its result is queued; use `wait` to consume it."
                .into(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            display_name: None,
            ctx_id: None,
        }),
        tau_proto::UnixMicros::new(1),
    );
    sync(&cold_handle);
    assert!(!cold_vt.screen_contains(80, "Tool call `replayed`"));
}

/// UI suppression relies on authenticated typed provenance rather than prose:
/// an ordinary harness-internal prompt with identical text remains available
/// when internal prompt diagnostics are enabled.
#[test]
fn untyped_internal_prompt_matching_completion_prose_remains_visible() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-internal-prompts", "on");

    let mut prompt = AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        text: "Tool call `same-prose` completed. Its result is queued; use `wait` to consume it."
            .into(),
        trusted_internal_spans: Vec::new(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::BackgroundToolCompletion),
        ctx_id: None,
    };
    prompt.internal_kind = None;
    renderer.handle(&Event::AgentPromptSteered(prompt));
    sync(&handle);

    assert!(vt.screen_contains(80, "□ Tool call `same-prose` completed."));
}

#[test]
fn queued_prompt_does_not_replace_dispatched_same_text() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: once a local echo has been accepted as a normal prompt,
    // a later queued prompt with the same text is a separate message. Do
    // not remove the earlier transcript block while rendering the queued
    // marker.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "repeat".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "repeat".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "repeat (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("repeat"))
            .count(),
        2,
        "queued prompt should not remove an earlier dispatched prompt with the same text, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn three_queued_prompts_render_sequentially() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Three rapid prompts.
    for i in 0..3 {
        if i == 0 {
            renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: format!("msg-{i}"),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }));
            renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
                "sp-0", "s1",
            )));
        } else {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: format!("msg-{i}"),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
    }

    // Process all three sequentially, flushing between each.
    for i in 0..3 {
        let spid: tau_proto::AgentPromptId = test_agent_prompt_id(format!("sp-{i}"));
        if 0 < i {
            renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
                agent_prompt_id: spid.clone(),
                ..agent_prompt_created("sp-ignore", "s1")
            }));
        }
        renderer.handle(&Event::ProviderResponseUpdated(
            provider_response_delta_update(
                spid.clone(),
                format!("partial-{i}"),
                None,
                tau_proto::PromptOriginator::User,
            ),
        ));
        renderer.handle(&Event::ProviderResponseFinished(finished_response(
            spid.as_ref(),
            vec![assistant_message_item(format!("response-{i}"))],
        )));
        sync(&handle);
    }

    // All three responses should be visible.
    // Extra flush to catch any delayed renders.
    sync(&handle);
    for i in 0..3 {
        assert!(
            vt.screen_contains(80, &format!("response-{i}")),
            "response-{i} should be visible, got: {:?}",
            vt.screen_text(80)
        );
    }
    // No stale "..." blocks.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn streaming_indicator_appends_during_updates() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hello")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "Hello …"));
}

#[test]
fn render_compaction_block_styles_completed_status() {
    let theme = cli_test_theme();

    let block = render_compaction_block(&theme, "ok", CompactionStatus::Success);
    let spans = block.content.spans();
    let success_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_SUCCESS);
    let ok = spans
        .iter()
        .find(|span| span.text == "ok")
        .expect("completed compaction status span");

    assert_eq!(ok.style, success_style);
}

/// Self-compaction metrics must use the generic neutral stats chip while only
/// the terminal `ok` uses the success color.
#[test]
fn self_compaction_tool_row_styles_metrics_as_stats() {
    let theme = cli_test_theme();
    let display = EventRenderer::self_compaction_tool_use_state(
        CompactionStatus::Success,
        "~#110k → ~#27.7k (25%) ok".to_owned(),
    );
    let block = render_tool_block(&theme, &render_tool_use_state("compact", &display));
    let cells = priority_header_cells(&block, 100);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let metrics_start = text[..text.find("~#110k").expect("compaction metrics")]
        .chars()
        .count();
    let ok_start = text[..text.rfind("ok").expect("terminal success status")]
        .chars()
        .count();

    assert_eq!(
        cells[metrics_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO)
    );
    assert_eq!(
        cells[ok_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_SUCCESS)
    );
}

#[test]
fn render_empty_provider_response_placeholder_without_context_item() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: the empty-response notice is a CLI presentation fallback, not
    // a provider-authored assistant message inserted into durable output_items.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-empty",
        Vec::new(),
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "(provider returned an empty response)"));
}

#[test]
fn render_provider_error_from_non_context_field() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let mut finished = finished_response("sp-error", Vec::new());
    finished.stop_reason = ProviderStopReason::Error;
    finished.error = Some("LLM error: boom".to_owned());

    // Regression: provider/runtime failures should be visible to the user but
    // must not be represented as assistant output_items that replay into the
    // next prompt.
    renderer.handle(&Event::ProviderResponseFinished(finished));
    sync(&handle);

    assert!(vt.screen_contains(80, "LLM error: boom"));
}

#[test]
fn manual_compaction_trigger_does_not_render_progress_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentCompactionTriggered(AgentCompactionTriggered {
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
        resume_inference: false,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "compact"));
    assert!(!vt.screen_contains(80, "manual compaction requested"));
}

/// A self-`compact` call and its private standalone transaction must share one
/// evolving tool row with canonical `ok` success before and after the generic
/// background terminal owns the final result.
#[test]
fn self_compaction_reuses_its_tool_row_through_background_completion() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-tools", "compact");
    let mut tool_start = tool_started("call-self", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-self", "call-self"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-self", "call-self", "ct-self", "ap-self"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-self"),
    ));
    sync(&handle);

    let progress = vt.screen_text(100).join("\n");
    assert!(progress.contains("Compacting…"), "{progress}");
    assert_eq!(progress.matches("Compacting…").count(), 1, "{progress}");

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 226_200,
            provenance: tau_proto::CompactionTokenProvenance::ProviderReported,
        }),
        compacted_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 4_500,
            provenance: tau_proto::CompactionTokenProvenance::ProviderReported,
        }),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-self")
                .expect("known-safe transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-self")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    sync(&handle);
    let compacted = vt.screen_text(100).join("\n");
    assert!(
        compacted.contains("#226.2k → #4.5k (2%) 0s ok"),
        "{compacted}"
    );
    assert!(!compacted.contains("complete"), "{compacted}");

    renderer.handle(&Event::ToolBackgroundResult(ToolBackgroundResult {
        call_id: "call-self".into(),
        tool_name: tau_proto::ToolName::new("compact"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let completed = vt.screen_text(100).join("\n");
    assert!(completed.contains("ok"), "{completed}");
    assert!(!completed.contains("complete"), "{completed}");
    assert!(!completed.contains("Compacting…"), "{completed}");
}

/// Correlation must survive attach-style ordering where a standalone lifecycle
/// arrives before the reconstructed generic tool start.
#[test]
fn late_self_compaction_tool_start_adopts_retained_lifecycle_status() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-tools", "compact");
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-late", "call-late"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-late", "call-late", "ct-late", "ap-late"),
    ));
    let mut tool_start = tool_started("call-late", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert!(text.contains("Compacting…"), "{text}");
    assert_eq!(text.matches("Compacting…").count(), 1, "{text}");
}

/// A self-compaction failure and a pre-start rejection update their owning
/// generic rows instead of creating standalone lifecycle rows.
#[test]
fn self_compaction_failure_and_rejection_reuse_their_tool_rows() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-tools", "compact");

    let mut failed_start = tool_started("call-failed", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut failed_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&failed_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-failed", "call-failed"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started(
            "cr-failed",
            "call-failed",
            "ct-failed-self",
            "ap-failed-self",
        ),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("main"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-failed-self")
                .expect("known-safe transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
        },
    ));

    let mut rejected_start = tool_started("call-rejected", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut rejected_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&rejected_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-rejected", "call-rejected"),
    ));
    renderer.handle(&Event::AgentManualCompactionRequestFailed(
        tau_proto::AgentManualCompactionRequestFailed {
            request_id: tau_proto::CompactionRequestId::parse("cr-rejected")
                .expect("known-safe request id"),
            target_agent_id: agent_id("main"),
            reason: tau_proto::ManualCompactionRequestFailureReason::Unsupported,
        },
    ));

    let mut cancelled_start = tool_started("call-cancelled", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut cancelled_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&cancelled_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-cancelled", "call-cancelled"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started(
            "cr-cancelled",
            "call-cancelled",
            "ct-cancelled-self",
            "ap-cancelled-self",
        ),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-cancelled-self"),
    ));
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: agent_id("main"),
        agent_prompt_id: test_agent_prompt_id("ap-cancelled-self"),
        reason: AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("err: failed").count(), 1, "{text}");
    assert_eq!(text.matches("err: rejected").count(), 1, "{text}");
    assert_eq!(text.matches("err: stopped").count(), 1, "{text}");
    assert!(!text.contains("compact complete"), "{text}");
}

/// A pre-start rejection must return to the target's detached transcript so
/// the selected agent cannot steal its self-compaction correlation.
#[test]
fn hidden_self_compaction_rejection_updates_its_target_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("manager".to_owned());
    renderer.apply_setting("show-tools", "compact");

    let mut tool_start = tool_started("call-hidden-rejected", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-hidden-rejected", "call-hidden-rejected"),
    ));
    renderer.handle(&Event::AgentManualCompactionRequestFailed(
        tau_proto::AgentManualCompactionRequestFailed {
            request_id: tau_proto::CompactionRequestId::parse("cr-hidden-rejected")
                .expect("known-safe request id"),
            target_agent_id: agent_id("main"),
            reason: tau_proto::ManualCompactionRequestFailureReason::Unsupported,
        },
    ));

    renderer.switch_agent("main".to_owned());
    sync(&handle);
    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("err: rejected").count(), 1, "{text}");
}

/// Contradictory request correlation must preserve the independent lifecycle
/// row rather than merging an unrelated tool call into it.
#[test]
fn mismatched_self_compaction_correlation_fails_open_to_distinct_rows() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-tools", "compact");

    let mut tool_start = tool_started("call-mismatch", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-mismatch", "call-mismatch"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-other", "call-mismatch", "ct-mismatch", "ap-mismatch"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-mismatch"),
    ));
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("Compacting…").count(), 1, "{text}");
    assert!(text.contains("compact 0s pending"), "{text}");
}

/// Ensures an independent standalone compaction terminal says `compact ok`,
/// never uses a custom success verb, and keeps streamed compactor text out of
/// the transcript and editor context.
#[test]
fn standalone_compaction_stream_is_hidden_from_cli_output() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-private", "ap-private"),
    ));
    renderer.handle(&Event::ProviderPromptSubmitted(
        tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: test_agent_prompt_id("ap-private"),
            originator: tau_proto::PromptOriginator::User,
        },
    ));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("ap-private"),
            "private compactor answer",
            Some("private compactor reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(100, "private compactor answer"),
        "the delayed typed start must remove output that earlier generic events rendered"
    );

    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-private"),
    ));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact Compacting…"));
    assert!(!vt.screen_contains(100, "private compactor answer"));
    assert!(!vt.screen_contains(100, "private compactor reasoning"));
    let editor_context = renderer.editor_context();
    let editor_context = editor_context.lock().expect("editor context");
    assert!(editor_context.current_response.is_none());
    assert!(editor_context.last_response.is_none());
    drop(editor_context);

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 226_200,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        compacted_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 4_500,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-private")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-private")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![assistant_message_item("private checkpoint")],
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact ~#226.2k → ~#4.5k (2%) ok"));
    assert!(!vt.screen_contains(100, "compact complete"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!vt.screen_contains(100, "private compactor answer"));
    assert!(!vt.screen_contains(100, "private compactor reasoning"));
    assert!(!vt.screen_contains(100, "private checkpoint"));
    assert!(!renderer.agent_has_active_prompt_for_test("main"));
    assert!(!renderer.main_agent_turn_active_for_test());
    assert!(
        !renderer
            .agent_in_progress_state()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

/// Ensures cold catch-up can fold the durable standalone start and replacement
/// boundary, without the live-only prompt-start fact, without an assistant turn
/// or stale compaction marker.
#[test]
fn standalone_compaction_replay_retires_private_progress() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-replay", "ap-replay"),
    ));
    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 226_200,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        compacted_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 4_500,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-replay")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-replay")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![assistant_message_item("synthetic checkpoint")],
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact ~#226.2k → ~#4.5k (2%) ok"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!vt.screen_contains(100, "synthetic checkpoint"));
    assert!(!vt.screen_contains(100, "◆"));
}

/// Ensures failed and terminated standalone lifecycles remove their private
/// progress marker without clearing or rendering any compactor output.
#[test]
fn standalone_compaction_terminal_failures_clear_private_progress() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-failed", "ap-failed"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-failed"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("main"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-failed")
                .expect("known-safe compaction transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact failed"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!renderer.agent_has_active_prompt_for_test("main"));
    assert!(!renderer.main_agent_turn_active_for_test());

    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-terminated"),
    ));
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: agent_id("main"),
        agent_prompt_id: test_agent_prompt_id("ap-terminated"),
        reason: AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact stopped"));
    assert!(!vt.screen_contains(100, "Compacting…"));
}

/// Ensures standalone success and failure retire a watched side agent's
/// prompt fallback without waiting for an agent-stats snapshot.
#[test]
fn standalone_compaction_terminals_clear_hidden_watched_activity() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("manager".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![agent_id("engineer")],
            changed_agent_id: Some(agent_id("engineer")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));

    let mut started = standalone_compaction_started("ct-side-success", "ap-side-success");
    started.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentStandaloneCompactionStarted(started));
    let mut prompt = standalone_compaction_prompt_started("ap-side-success");
    prompt.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentPromptStarted(prompt));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 1);
    assert!(vt.screen_contains(100, "❓✨ @engineer"));
    assert!(vt.screen_contains(100, "@1"));

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: None,
        compacted_input_tokens: None,
        agent_id: agent_id("engineer"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-side-success")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-side-success")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 0);
    assert!(vt.screen_contains(100, "❓💤 @engineer"));
    assert!(!vt.screen_contains(100, "@1"));

    let mut started = standalone_compaction_started("ct-side-failed", "ap-side-failed");
    started.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentStandaloneCompactionStarted(started));
    let mut prompt = standalone_compaction_prompt_started("ap-side-failed");
    prompt.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentPromptStarted(prompt));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 1);
    assert!(vt.screen_contains(100, "❓✨ @engineer"));
    assert!(vt.screen_contains(100, "@1"));

    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("engineer"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-side-failed")
                .expect("known-safe compaction transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
        },
    ));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 0);
    assert!(vt.screen_contains(100, "❓💤 @engineer"));
    assert!(!vt.screen_contains(100, "@1"));
}

/// Ensures normal inference continues to render provider deltas when an
/// unrelated malformed standalone lifecycle does not carry its compact
/// operation.
#[test]
fn malformed_standalone_lifecycle_does_not_hide_inference_stream() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    let mut malformed = standalone_compaction_started("ct-malformed", "ap-inference");
    malformed.operation = tau_proto::PromptOperation::Inference;
    renderer.handle(&Event::AgentStandaloneCompactionStarted(malformed));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "ap-inference",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("ap-inference"),
            "ordinary inference answer",
            Some("ordinary inference reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary inference answer"));
    assert!(vt.screen_contains(100, "ordinary inference reasoning"));
}

#[test]
fn render_provider_compaction_update_as_compact_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-compact"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: Some(tau_proto::ProviderResponseCompactionUpdate {
            status: tau_proto::ProviderResponseCompactionStatus::Started,
            original_input_tokens: Some(226_200),
            compacted_input_tokens: None,
        }),
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let progress = format!("compact #226.2k {}", tau_proto::PROGRESS_INDICATOR_TEXT);
    assert!(vt.screen_contains(80, &progress));
    assert!(!vt.screen_contains(80, "compacting"));
}

#[test]
fn render_provider_compaction_item_when_response_finishes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: a manual trigger event only records the user request. The UI
    // should show compaction after the provider returns the durable compaction
    // item, which means server-side compaction has actually completed.
    let mut finished = finished_response(
        "sp-compact",
        vec![ContextItem::Compaction(OpaqueProviderItem::new(
            CborValue::Map(vec![]),
        ))],
    );
    finished.compaction_original_input_tokens = Some(226_200);
    finished.compaction_compacted_input_tokens = Some(4_500);
    renderer.handle(&Event::ProviderResponseFinished(finished));
    sync(&handle);

    assert!(vt.screen_contains(80, "compact #226.2k ok: #4.5k"));
    assert!(!vt.screen_contains(80, "compacted"));
}

/// Compaction terminals must keep `ok` last, distinguish estimates, round the
/// retained ratio, and degrade without inventing a percentage or missing count.
#[test]
fn compaction_success_status_formats_provenance_and_partial_measurements() {
    use tau_proto::{CompactionTokenMeasurement as Measurement, CompactionTokenProvenance};

    let exact = |tokens| Measurement {
        tokens,
        provenance: CompactionTokenProvenance::ProviderReported,
    };
    let estimated = |tokens| Measurement {
        tokens,
        provenance: CompactionTokenProvenance::Estimated,
    };

    assert_eq!(
        EventRenderer::standalone_compaction_success_status(
            Some(exact(226_200)),
            Some(estimated(4_500)),
        ),
        "#226.2k → ~#4.5k (2%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(
            Some(estimated(226_200)),
            Some(exact(4_500)),
        ),
        "~#226.2k → #4.5k (2%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(estimated(12_000)), None),
        "~#12k → ? ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(None, Some(exact(4_500))),
        "? → #4.5k ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(exact(0)), Some(exact(1))),
        "#0 → #1 ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(exact(3)), Some(exact(2))),
        "#3 → #2 (67%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(
            Some(exact(u64::MAX)),
            Some(exact(u64::MAX)),
        ),
        "#18446744073709.5m → #18446744073709.5m (100%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(None, None),
        "ok"
    );
}

/// Ensures idle watched status rows repaint with self-reported work and stats.
///
/// The initial unreported row must appear before model activity, then update in
/// place when the agent reports working and its counters change.
#[test]
fn watched_agent_stats_redraws_status_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::New,
    }));
    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1"),
        "watched-agent stats should repaint without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1 %3/3"),
        "watched-agent stats should repaint with tool-call-style counters without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),
        parent_agent: Some(agent_id("parent_1")),
        agent_id: agent_id("engineer_1"),
        role: "engineer".to_owned(),
        display_name: Some("worker display".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("status-engineer-1")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_1"),
            sender_session_id: None,
            recipient_id: agent_id("parent_1"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "watch-engineer-1".to_owned(),
                status_epoch: 1,
                phase: tau_proto::AgentWorkStatusPhase::Working,
                title: Some("investigate session".to_owned()),
                initial: true,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        },
    ));
    assert!(
        eventually_screen_contains(
            &vt,
            100,
            "🚀💤 @engineer_1 (worker display) investigate session %3/3",
        ),
        "the watched row should use the agent's own status title and display name: {:?}",
        vt.screen_text(100)
    );
    renderer.handle(&Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_1"),
        },
    ));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1 (worker display)"),
        "a reloaded same-id row must not retain its former self-reported title: {:?}",
        vt.screen_text(100)
    );
    let reloaded_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| {
            row.trim_start()
                .starts_with("❓💤 @engineer_1 (worker display)")
        })
        .expect("reloaded watched row");
    assert!(!reloaded_row.contains("investigate session"));
    assert!(
        !vt.screen_contains(100, "running tools"),
        "watched-agent block should keep compact tool-block layout, not prose: {:?}",
        vt.screen_text(100)
    );
}

/// Multiple active watched-agent blocks should keep a deterministic order
/// across refreshes even when prompt-start events arrive in a different order.
/// This prevents visually similar `watching` rows from flickering by swapping
/// positions between redraws.
#[test]
fn watched_agent_blocks_are_sorted_by_agent_id() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_b"), agent_id("engineer_a")],
            changed_agent_id: None,
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    for watched in ["engineer_b", "engineer_a"] {
        renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            session_id: test_session_id("s1"),
            agent_id: agent_id(watched),
            agent_prompt_id: test_agent_prompt_id(format!("ap-{watched}-0")),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: format!("delegate-{watched}"),
            },
            ctx_id: None,
        }));
    }
    sync(&handle);

    let screen = vt.screen_text(100);
    let first = screen
        .iter()
        .position(|line| line.contains("❓✨ @engineer_a"))
        .expect("engineer_a running row");
    let second = screen
        .iter()
        .position(|line| line.contains("❓✨ @engineer_b"))
        .expect("engineer_b running row");
    assert!(
        first < second,
        "watched-agent rows should be sorted by agent id: {screen:?}"
    );
}

/// Active tools, queued prompts, and watched engineers must keep their category
/// order regardless of whether tools or watches arrive first. This prevents a
/// later update from moving a lower-priority activity row above user work.
#[test]
fn mixed_live_activity_blocks_keep_category_and_internal_order() {
    for tools_arrive_first in [true, false] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.switch_agent("parent_1".to_owned());

        let render_tools = |renderer: &mut EventRenderer| {
            for call_id in ["read_one", "read_two"] {
                renderer.handle(&Event::ToolStarted(tau_proto::ToolStarted {
                    call_id: call_id.into(),
                    tool_name: tau_proto::ToolName::new(call_id),
                    arguments: CborValue::Null,
                    agent_id: agent_id("parent_1"),
                    originator: tau_proto::PromptOriginator::User,
                }));
            }
        };
        let render_watches = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentWatchesUpdated(
                tau_proto::AgentWatchesUpdated {
                    session_id: test_session_id("s1"),
                    watcher_id: agent_id("parent_1"),
                    watched_agent_ids: vec![agent_id("engineer_b"), agent_id("engineer_a")],
                    changed_agent_id: None,
                    cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
                },
            ));
        };

        if tools_arrive_first {
            render_tools(&mut renderer);
        } else {
            render_watches(&mut renderer);
        }
        for text in ["queued-one", "queued-two"] {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: text.to_owned(),
                agent_id: agent_id("parent_1"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
        if tools_arrive_first {
            render_watches(&mut renderer);
        } else {
            render_tools(&mut renderer);
        }

        // Refresh from a differently ordered update to exercise both reordering
        // existing watched rows and the mixed-category anchor placement.
        renderer.handle(&Event::AgentWatchesUpdated(
            tau_proto::AgentWatchesUpdated {
                session_id: test_session_id("s1"),
                watcher_id: agent_id("parent_1"),
                watched_agent_ids: vec![agent_id("engineer_a"), agent_id("engineer_b")],
                changed_agent_id: Some(agent_id("engineer_a")),
                cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
            },
        ));
        sync(&handle);

        let screen = vt.screen_text(100);
        let positions = [
            "read_one 0s pending",
            "read_two 0s pending",
            "queued-one (queued)",
            "queued-two (queued)",
            "❓💤 @engineer_a",
            "❓💤 @engineer_b",
        ]
        .map(|needle| {
            screen
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing `{needle}` in {screen:?}"))
        });
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "expected tools, queued prompts, then sorted watched engineers: {screen:?}"
        );
    }
}

/// A pending agent response must remain above watched-agent status rows whether
/// the response or watch snapshot arrives first.
#[test]
fn pending_agent_response_stays_above_watched_agent_rows() {
    for response_arrives_first in [true, false] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.switch_agent("main".to_owned());

        let render_response = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
                "sp-pending",
                "s1",
            )));
            renderer.handle(&Event::ProviderResponseUpdated(
                provider_response_delta_update(
                    test_agent_prompt_id("sp-pending"),
                    "pending agent response",
                    None,
                    tau_proto::PromptOriginator::User,
                ),
            ));
        };
        let render_watch = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentWatchesUpdated(
                tau_proto::AgentWatchesUpdated {
                    session_id: test_session_id("s1"),
                    watcher_id: agent_id("main"),
                    watched_agent_ids: vec![agent_id("engineer_1")],
                    changed_agent_id: Some(agent_id("engineer_1")),
                    cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
                },
            ));
        };

        if response_arrives_first {
            render_response(&mut renderer);
            render_watch(&mut renderer);
        } else {
            render_watch(&mut renderer);
            render_response(&mut renderer);
        }
        sync(&handle);

        let screen = vt.screen_text(100);
        let response = screen
            .iter()
            .position(|line| line.contains("pending agent response"))
            .unwrap_or_else(|| panic!("missing pending response in {screen:?}"));
        let watched = screen
            .iter()
            .position(|line| line.contains("❓💤 @engineer_1"))
            .unwrap_or_else(|| panic!("missing watched row in {screen:?}"));
        assert!(
            response < watched,
            "pending response should stay above watched rows: {screen:?}"
        );
    }
}

/// Recursive watched rows must include topology-only descendants before any
/// stats arrive, label their deterministic parent, and retain distinct
/// descendant-witness activity on direct rows.
///
/// This prevents a parent row from flickering out between child model rounds or
/// losing the reason an indirect child appears in the selected transcript.
#[test]
fn watched_agent_recursive_rows_keep_via_and_distinct_witness_context() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("manager".to_owned());
    for (watcher, watched) in [("manager", "reviewer"), ("reviewer", "worker")] {
        renderer.handle(&Event::AgentWatchesUpdated(
            tau_proto::AgentWatchesUpdated {
                session_id: test_session_id("s1"),
                watcher_id: agent_id(watcher),
                watched_agent_ids: vec![agent_id(watched)],
                changed_agent_id: Some(agent_id(watched)),
                cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
            },
        ));
    }
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @reviewer"));
    assert!(
        vt.screen_contains(100, "❓💤 @worker via @reviewer"),
        "topology-only indirect rows must remain visible without stats"
    );
    let prompt_started = |agent: &str| {
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            session_id: test_session_id("s1"),
            agent_id: agent_id(agent),
            agent_prompt_id: test_agent_prompt_id(format!("ap-{agent}")),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })
    };

    renderer.handle(&prompt_started("worker"));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @reviewer watching -> @worker"));
    assert!(
        vt.screen_contains(100, "❓✨ @worker via @reviewer"),
        "via context and direct running state must coexist"
    );

    renderer.handle(&prompt_started("reviewer"));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓✨ @reviewer"));
    assert!(vt.screen_contains(100, "❓✨ @worker via @reviewer"));
    assert!(
        !vt.screen_contains(100, "@reviewer -> @worker"),
        "direct-running state must replace the transitive witness"
    );
}

/// Ensures watched-agent rows remain owned by the transcript snapshot across
/// agent switches.
///
/// This prevents restoring a parent transcript that already contains a
/// `watching [...]` row while the renderer has forgotten that row's block id,
/// which would otherwise create a duplicate simultaneous row for the same
/// watched agent on the next refresh.
#[test]
fn watched_agent_status_row_does_not_duplicate_after_agent_switch() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 13,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    assert!(eventually_screen_contains(
        &vt,
        100,
        "❓💤 @engineer_1 %13/13",
    ));

    renderer.switch_agent("other_1".to_owned());
    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 42,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);

    let watching_rows: Vec<_> = vt
        .screen_text(100)
        .into_iter()
        .filter(|row| row.contains("❓💤 @engineer_1"))
        .map(|row| row.trim_end().to_owned())
        .collect();
    assert_eq!(
        watching_rows,
        vec!["❓💤 @engineer_1 %42/42"],
        "watched-agent row should update in place after transcript restore: {:?}",
        vt.screen_text(100)
    );
}

/// A provider response must stop only transient activity, not the watched row.
///
/// The row remains available for the agent's task status until the agent
/// reports `done`; a provider terminal cannot make it flicker away between
/// model rounds.
#[test]
fn watched_agent_response_finished_keeps_status_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 15,
        },
        context: tau_proto::AgentContextStats::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    assert!(eventually_screen_contains(
        &vt,
        100,
        "❓💤 @engineer_1 %15/15",
    ));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1 %15/15"),
        "outer runtime state should remain running after the inner response finishes: {:?}",
        vt.screen_text(100)
    );
}

/// A direct watched row must use the canonical task-status phase for lifetime.
///
/// Missing status is unreported; working and blocked preserve the same row
/// through turn start and stop, while done is the only phase that removes it.
#[test]
fn watched_agent_status_row_survives_turn_transitions_until_done() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    let stats = |runtime_state| {
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_1"),
            work_status: Default::default(),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state,
            turn_activity: tau_proto::AgentTurnActivity::Idle,
            tools: Default::default(),
            context: Default::default(),
            estimated_api_cost: Default::default(),
            creator_subtree_estimated_api_cost: Default::default(),
        })
    };
    let watch_status = |message_id: &str, phase, title: Option<&str>| {
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse(message_id)
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_1"),
            sender_session_id: None,
            recipient_id: agent_id("parent_1"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "watch-1".to_owned(),
                status_epoch: 1,
                phase,
                title: title.map(str::to_owned),
                initial: false,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        })
    };

    sync(&handle);
    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "an absent status snapshot is canonically unreported"
    );

    renderer.handle(&watch_status(
        "status-unreported",
        tau_proto::AgentWorkStatusPhase::Unreported,
        None,
    ));
    renderer.handle(&stats(tau_proto::AgentRuntimeState::Running));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));
    renderer.handle(&stats(tau_proto::AgentRuntimeState::Idle));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

    renderer.handle(&watch_status(
        "status-working",
        tau_proto::AgentWorkStatusPhase::Working,
        Some("implement fix"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "🚀💤 @engineer_1 implement fix"));

    renderer.handle(&watch_status(
        "status-blocked",
        tau_proto::AgentWorkStatusPhase::Blocked,
        Some("await input"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "⛔️💤 @engineer_1 await input"));

    renderer.handle(&watch_status(
        "status-waiting",
        tau_proto::AgentWorkStatusPhase::Waiting,
        Some("await automation"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "⏳💤 @engineer_1 await automation"));

    renderer.handle(&watch_status(
        "status-unknown",
        tau_proto::AgentWorkStatusPhase::Unknown,
        None,
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

    renderer.handle(&watch_status(
        "status-done",
        tau_proto::AgentWorkStatusPhase::Done,
        Some("finished"),
    ));
    sync(&handle);
    assert!(
        vt.screen_text(100)
            .iter()
            .all(|row| !row.trim_start().starts_with("@engineer_1 ")),
        "done must remove the watched-agent activity row"
    );
}

/// Provider-prompt fallback must clear activity on terminal without removing
/// the watched row.
///
/// This covers backends or replay paths that omit `agent.prompt_started` before
/// provider work, preventing their terminal event from looking like task done.
#[test]
fn watched_agent_provider_prompt_terminal_keeps_status_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "delegate-1".to_owned(),
        agent_id: agent_id("engineer_1"),
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderPromptSubmitted(
        tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
            originator: tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "delegate-1".to_owned(),
            },
        },
    ));
    sync(&handle);

    assert!(eventually_screen_contains(&vt, 100, "❓✨ @engineer_1",));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "provider-fallback terminal should retain the watched status row: {:?}",
        vt.screen_text(100)
    );
}

/// Provider response updates use their explicit agent id as the active prompt
/// owner, then terminal cleanup clears activity without removing its status
/// row.
///
/// This prevents a provider-update-only path from accidentally marking the
/// current/originator agent active and leaving the watched response owner stale
/// after `provider.response_finished`.
#[test]
fn watched_agent_provider_response_update_keeps_status_row_after_terminal() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            test_agent_prompt_id("ap-engineer_1-0"),
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "parent-query".to_owned(),
            },
        )
    }));
    sync(&handle);

    assert!(eventually_screen_contains(&vt, 100, "❓✨ @engineer_1",));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "parent-query".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "terminal prompt id should clear activity but retain the status row: {:?}",
        vt.screen_text(100)
    );
}

/// Terminal prompt events tombstone their prompt id so delayed start or create
/// events cannot reactivate a persistent watched status row.
#[test]
fn watched_agent_terminal_event_wins_over_delayed_prompt_start() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_created("ap-engineer_1-0", "s1")
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "delayed start/create must retain, not reactivate, the status row: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures the now-immediate `agent_start` completion remains informative even
/// though the child agent's final answer is delivered later through
/// `agent_watch` notifications.

#[test]
fn immediate_agent_start_completion_shows_agent_stats_and_standard_status() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&tool_started(
        "delegate-call",
        "agent_start",
        CborValue::Map(Vec::new()),
    ));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(Vec::new()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "[audit]".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(2),
                bytes: Some(12),
            },
            info_chips: vec!["@engineer_child".into()],
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let rows = vt.screen_text(100);
    let line = rows
        .iter()
        .find(|row| row.contains("agent_start"))
        .expect("agent_start completion line");
    assert!(
        line.contains("agent_start [audit] 2L, 12B @engineer_child"),
        "immediate agent_start completion should include spawned id and prompt size: {rows:?}",
    );
    assert!(
        line.contains("ok"),
        "immediate agent_start completion should use the standard success status: {rows:?}",
    );
}

#[test]
fn provider_tool_error_before_tool_started_is_ignored() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-args".into(),
                name: tau_proto::ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("unknown_option".into()),
                    CborValue::Text("invalid".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "delegate 0s …"));

    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "bad-args".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments for tool `agent_start`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "delegate err: invalid"));
    assert!(!vt.screen_contains(80, "delegate 0s …"));
}
#[test]
fn logical_and_provider_tool_errors_render_one_terminal_line() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("overlap-edit", "edit", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolError(ToolError {
            presentation: Default::default(),
            call_id: "overlap-edit".into(),
            tool_name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            message: "overlapping edits".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "overlap-edit".into(),
            tool_name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            message: "overlapping edits".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_100_000),
    );
    sync(&handle);

    let text = vt.screen_text(80).join("\n");
    assert!(text.contains("edit 1s err: overlapping edits"));
    assert_eq!(text.matches("overlapping edits").count(), 1);
}

/// Inter-session receiver rejection must retain its actionable fixed detail in
/// the terminal's ordinary tool-error presentation.
#[test]
fn message_tool_receiver_rejection_renders_actionable_detail() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("message-no-receiver", "message", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolError(ToolError {
            presentation: Default::default(),
            call_id: "message-no-receiver".into(),
            tool_name: tau_proto::ToolName::new("message"),
            tool_type: tau_proto::ToolType::Function,
            message: "target live; no receiver; set `inter_session_receiver`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    let text = vt.screen_text(120).join("\n");
    assert!(
        text.contains("target live; no receiv"),
        "terminal did not retain the receiver diagnosis: {text}"
    );
    assert!(
        text.contains("inter_session_receiver"),
        "terminal did not retain the configuration key: {text}"
    );
}

/// Provider-facing errors must not finish live UI tool blocks. The harness is
/// responsible for publishing a logical `ToolError` for user-visible failures.
#[test]
fn provider_tool_error_without_logical_tool_error_does_not_finish_live_tool() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-args".into(),
                name: tau_proto::ToolName::new("strict_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started("bad-args", "strict_tool", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_500_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "strict_tool 0s pending"));
    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "bad-args".into(),
            tool_name: tau_proto::ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments: unexpected argument `extra`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "err: invalid"));
    assert!(vt.screen_contains(80, "strict_tool 0s pending"));
}

/// The normalized wait bound must be visible both while the call is live and
/// after its generic result replaces the pending block.
#[test]
fn wait_timeout_label_survives_live_to_retained_transition() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let arguments = CborValue::Map(vec![(
        CborValue::Text("timeout_minutes".to_owned()),
        CborValue::Integer(75.into()),
    )]);

    renderer.handle_recorded_at(
        &tool_started("wait-timeout", "wait", arguments),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("wait-timeout", "wait", "60m", ""),
        tau_proto::UnixMicros::new(1_100_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "wait 60m"));

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "wait-timeout".into(),
            tool_name: tau_proto::ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("timed_out: true".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "60m".to_owned(),
                status: tau_proto::ToolUseStatus::Warning,
                status_text: "timeout".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "wait 60m 1s timeout"));

    // Durable replay does not include transient progress, so the terminal
    // descriptor must remain self-contained.
    let (_replay_term, replay_handle, replay_vt) = setup(80, 24);
    let mut replay = EventRenderer::new(
        replay_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    replay.handle_recorded_at(
        &tool_started(
            "replayed-wait",
            "wait",
            CborValue::Map(vec![(
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(75.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    replay.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "replayed-wait".into(),
            tool_name: tau_proto::ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Map(vec![(
                CborValue::Text("timed_out".to_owned()),
                CborValue::Bool(true),
            )]),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "60m".to_owned(),
                status: tau_proto::ToolUseStatus::Warning,
                status_text: "timeout".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&replay_handle);
    assert!(replay_vt.screen_contains(80, "wait 60m 1s timeout"));
}

/// Ensures canonical completed cold-replay facts fold directly to one terminal
/// dummy-tool row and cannot be resurrected by later transcript activity.
#[test]
fn completed_dummy_tool_replay_is_terminal_idle_and_stays_terminal() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-tool-prompt",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "opud-call".into(),
            name: tau_proto::ToolName::new("restart_test_dummy"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "opud-call".into(),
        tool_name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("restart succeeded".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-final-prompt",
        vec![assistant_message_item("opud-tool-complete")],
    )));
    sync(&handle);
    let row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("restart_test_dummy"))
        .expect("terminal dummy row");
    assert!(row.contains("ok"), "{row}");
    assert!(!row.contains("pending"), "{row}");
    assert_eq!(renderer.test_active_tool_count(), 0);

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-later-prompt",
        vec![assistant_message_item("later response")],
    )));
    sync(&handle);
    assert_eq!(renderer.test_active_tool_count(), 0);
    assert!(!vt.screen_contains(100, "restart_test_dummy 0s pending"));
}

/// Ensures an incomplete historical tool call repaired on resume is shown as
/// explicitly uncertain/error rather than silently successful or left active.
#[test]
fn incomplete_dummy_tool_replay_is_repaired_honestly_and_not_active() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-incomplete-prompt",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "opud-incomplete".into(),
            name: tau_proto::ToolName::new("restart_test_dummy"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: "opud-incomplete".into(),
        tool_name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        message: "Interrupted during restart. Side effects may have occurred.".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    }));
    sync(&handle);
    let row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("restart_test_dummy"))
        .expect("repaired dummy row");
    assert!(row.contains("err"), "{row}");
    assert!(!row.contains(" ok"), "{row}");
    assert!(!row.contains("pending"), "{row}");
    assert_eq!(renderer.test_active_tool_count(), 0);
}

/// A running tool call remains visibly pending until its result arrives.
#[test]
fn running_tool_call_shows_ellipsis_until_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("call-1", "read", "src/main.rs", ""),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Map(vec![
                (
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                ),
                (
                    CborValue::Text("content".into()),
                    CborValue::Text("fn main() {}\n".into()),
                ),
            ]),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolUseStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 2s ok"));
    assert!(!vt.screen_contains(80, "read src/main.rs …"));
}

#[test]
fn tool_progress_display_replaces_live_state_generically() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("call-1", "dir_lock", CborValue::Map(vec![])),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("dir_lock"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "update /tmp/project".into(),
                info_chips: vec!["dir lock".into()],
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: "waiting".into(),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );

    // Regression: ToolProgress.display is a complete ToolUseState replacement.
    // The renderer must preserve generic stats/counters/chips/status instead of
    // treating progress as just a name/args/ellipsis header.
    sync(&handle);
    assert!(vt.screen_contains(80, "dir_lock update /tmp/project"));
    assert!(vt.screen_contains(80, "dir lock"));
    assert!(vt.screen_contains(80, "waiting"));
}

/// A status call's semantic descriptor must survive the complete event-renderer
/// lifecycle, with terminal duration and outcome retained in their real order.
#[test]
fn status_descriptor_survives_terminal_tool_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("status-call", "status", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress(
            "status-call",
            "status",
            "working: implementing renderer coverage",
            "",
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "status-call".into(),
            tool_name: tau_proto::ToolName::new("status"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("Status accepted".into()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "working: implementing renderer coverage".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(80, "status working: implementing renderer coverage 2s ok"));
    assert!(!vt.screen_contains(80, "status 2s ok"));
}

/// A harness-originated message call must leave the live-progress set when its
/// canonical provider terminal arrives, even when the renderer does not receive
/// the redundant transient `tool.result` projection.
#[test]
fn provider_terminal_finishes_harness_originated_message_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("__harness__").expect("extension name"),
        query_id: "peer-auto-start".to_owned(),
    };
    let mut started = match tool_started(
        "message-call",
        "message",
        CborValue::Map(vec![(
            CborValue::Text("recipient_id".into()),
            CborValue::Text("engineer".into()),
        )]),
    ) {
        Event::ToolStarted(started) => started,
        _ => unreachable!("tool_started helper always returns ToolStarted"),
    };
    started.agent_id = agent_id("coordinator");
    started.originator = originator.clone();
    renderer.handle(&Event::ToolStarted(started));
    renderer.handle(&Event::ToolProgress(tau_proto::ToolProgress {
        call_id: "message-call".into(),
        tool_name: tau_proto::ToolName::new("message"),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: "engineer".into(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
            ..Default::default()
        }),
    }));
    renderer.handle(&Event::ProviderToolResult(tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "message-call".into(),
        tool_name: tau_proto::ToolName::new("message"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("Message sent".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "message engineer"));
}

#[test]
fn tool_started_renders_pending_until_provider_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ToolStarted(tau_proto::ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("fallback.rs".into()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read 0s pending"));
    assert!(!vt.screen_contains(80, "fallback.rs"));

    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "semantic.rs".into(),
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read semantic.rs"));
}

/// Ensures shell-command duration chips retain both an explicit timeout and the
/// provider's effective default after the running call becomes a history row.
#[test]
fn shell_command_duration_shows_effective_timeout() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started(
            "shell-default",
            "gpt_shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text("sleep 300".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "shell-default".into(),
            tool_name: tau_proto::ToolName::new("gpt_shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "sleep 300".into(),
                mode: "rw".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(207_000_000),
    );

    renderer.handle_recorded_at(
        &tool_started(
            "shell-explicit",
            "gpt_shell",
            CborValue::Map(vec![
                (
                    CborValue::Text("command".into()),
                    CborValue::Text("sleep 400".into()),
                ),
                (
                    CborValue::Text("timeout".into()),
                    CborValue::Integer(300.into()),
                ),
            ]),
        ),
        tau_proto::UnixMicros::new(210_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "shell-explicit".into(),
            tool_name: tau_proto::ToolName::new("gpt_shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "sleep 400".into(),
                mode: "rw".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(416_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "gpt_shell rw sleep 300 206/300s ok"));
    assert!(vt.screen_contains(100, "gpt_shell rw sleep 400 206/300s ok"));
}

#[test]
fn backgrounded_tool_stays_visibly_running_until_background_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("command".into()),
                        CborValue::Text("sleep 10".into()),
                    ),
                    (CborValue::Text("mode".into()), CborValue::Text("ro".into())),
                ]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![
                (
                    CborValue::Text("command".into()),
                    CborValue::Text("sleep 10".into()),
                ),
                (CborValue::Text("mode".into()), CborValue::Text("ro".into())),
            ]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("call-1", "shell", "sleep 10", "ro"),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text(
                "tau_internal: true\n\nTool call `call-1` is running in the background.".into(),
            ),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell ro sleep 10"));
    assert!(!vt.screen_contains(80, "shell 1s ok"));
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-final",
        vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done for now".into(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle_recorded_at(
        &Event::ToolBackgroundResult(ToolBackgroundResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".into()),
            display: Some(tau_proto::ToolUseState {
                args: "ro sleep 10".into(),
                status: tau_proto::ToolUseStatus::Error,
                status_text: "false-error".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(4_000_000),
    );
    sync(&handle);
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell ro sleep 10 3/300s ok"));
    assert!(vt.screen_contains(80, "1/1"));
}

/// Regression coverage for multiline `shell` calls in `show-tools=full`:
/// the running block must already reserve/show the command body, matching the
/// final result block and avoiding a layout jump when the command finishes.
#[test]
fn running_shell_tool_shows_multiline_command_body_in_full_mode() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let command = "printf hello\nprintf world";

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text(command.into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text(command.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "printf hello".to_owned(),
                mode: "rw".to_owned(),
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: command.to_owned(),
                }),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell rw printf hello"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "running shell command body should be on its own row"
    );

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "rw printf hello".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: command.into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell rw printf hello 1/300s ok"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "finished shell command body should stay on its own row"
    );
}

#[test]
fn finished_tool_result_preserves_message_and_tool_item_order() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("before tool"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            assistant_message_item("after tool"),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let lines = vt.screen_text(100);
    let before = lines
        .iter()
        .position(|line| line.contains("before tool"))
        .unwrap_or_else(|| panic!("missing first message: {lines:?}"));
    let tool = lines
        .iter()
        .position(|line| line.contains("read src/main.rs"))
        .unwrap_or_else(|| panic!("missing tool call: {lines:?}"));
    let after = lines
        .iter()
        .position(|line| line.contains("after tool"))
        .unwrap_or_else(|| panic!("missing second message: {lines:?}"));
    assert!(
        before < tool && tool < after,
        "output_items order should be preserved; lines: {lines:?}",
    );
}

#[test]
fn live_tool_timer_updates_do_not_mutate_scrolled_history() {
    // Running tool calls live in the fixed active-tools area above the prompt.
    // Timer ticks should therefore repaint that visible area only, not trigger a
    // hidden-prefix full redraw of old transcript rows that have moved to
    // scrollback.
    let (_term, handle, vt) = setup(80, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-history",
        (0..10)
            .map(|i| assistant_message_item(format!("history line {i}")))
            .collect(),
    )));
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-tool",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: read_args.clone(),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started("call-1", "read", read_args));
    renderer.handle(&initial_tool_progress("call-1", "read", "src/main.rs", ""));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));

    let full_renders_before = handle.full_render_count();
    renderer.handle_tool_timer_tick();
    sync(&handle);

    assert_eq!(
        handle.full_render_count(),
        full_renders_before,
        "live timer ticks must not full-redraw hidden transcript rows",
    );
    assert!(vt.screen_contains(80, "read src/main.rs"));
}

/// Streaming assistant text must stay above already-running tool calls so the
/// tool UI remains pinned near the prompt even when the live response grows
/// taller than the viewport.
#[test]
fn active_tool_stays_below_streaming_response() {
    let (_term, handle, vt) = setup(80, 6);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&tool_started("call-1", "read", read_args));
    renderer.handle(&initial_tool_progress("call-1", "read", "src/main.rs", ""));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));
    let full_renders_before = handle.full_render_count();

    let long_response = (0..12)
        .map(|i| format!("streaming response line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-streaming",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-streaming"),
            long_response,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    let lines = visible_lines(&vt, 80);
    let response_line = lines
        .iter()
        .position(|line| line.contains("streaming response line"))
        .unwrap_or_else(|| panic!("missing streaming response line: {lines:?}"));
    let tool_line = lines
        .iter()
        .position(|line| line.contains("read src/main.rs"))
        .unwrap_or_else(|| panic!("missing pinned tool line: {lines:?}"));
    assert!(
        response_line < tool_line,
        "active tool should stay below the streaming response: {lines:?}",
    );
    assert_eq!(
        handle.full_render_count(),
        full_renders_before,
        "pinning live tool calls must not force a full redraw",
    );
}

#[test]
fn live_multiline_payload_tool_uses_static_duration_placeholder() {
    // Multi-line live tool payloads can extend above the visible active-tools
    // area. Updating only the elapsed seconds would force visible churn without
    // changing useful content, so keep the live duration stable until completion.
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "compact");
    let args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-tool",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: args.clone(),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started("call-1", "read", args));
    renderer.handle(&Event::ToolProgress(tau_proto::ToolProgress {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: "line 1\nline 2".into(),
            }),
            ..Default::default()
        }),
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "read src/main.rs 0s"));

    renderer.apply_setting("show-tools", "full");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));
    assert!(vt.screen_contains(80, "line 1"));

    renderer.apply_setting("show-tools", "compact");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 0s"));

    renderer.apply_setting("show-tools", "full");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));
    assert!(vt.screen_contains(80, "line 1"));

    let full_renders_before = handle.full_render_count();
    renderer.handle_tool_timer_tick();
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_renders_before);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: "line 1\nline 2".into(),
            }),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "read src/main.rs 0s ok"));
}

/// Compact mode must be a reversible projection: it retains conversation text,
/// removes thinking and completed tools, and exposes one payload-free row for
/// every tool that is still running, regardless of its eventual terminal kind.
#[test]
fn verbose_mode_round_trips_thinking_and_overlapping_tool_outcomes() {
    let (_term, handle, vt) = setup(120, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-turn-stats", "true");

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-verbose",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-verbose"),
            "conversation answer",
            Some("private reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-verbose",
        vec![assistant_message_item("conversation answer")],
    )));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("sp-stats", "main", 20_000, 10_000, 500, "stats answer"),
    ));

    for (call_id, tool_name) in [
        ("call-ok", "read"),
        ("call-error", "search"),
        ("call-cancel", "write"),
    ] {
        let mut started = tool_started(call_id, tool_name, CborValue::Null);
        let Event::ToolStarted(started_event) = &mut started else {
            unreachable!("tool_started helper returns a tool start");
        };
        started_event.agent_id = agent_id("main");
        renderer.handle(&started);
        renderer.handle(&initial_tool_progress(
            call_id,
            tool_name,
            "SECRET_ARGUMENT",
            "SECRET_MODE",
        ));
    }
    renderer.toggle_verbose_mode();
    sync(&handle);

    let compact = vt.screen_text(120).join("\n");
    assert!(compact.contains("conversation answer"), "{compact}");
    assert!(!compact.contains("private reasoning"), "{compact}");
    assert!(!compact.contains('Δ'), "{compact}");
    assert!(!compact.contains("SECRET_ARGUMENT"), "{compact}");
    assert!(!compact.contains("SECRET_MODE"), "{compact}");
    for tool_name in ["read", "search", "write"] {
        assert_eq!(compact.matches(tool_name).count(), 1, "{compact}");
    }

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-ok".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("SECRET_RESULT".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "SECRET_ARGUMENT".to_owned(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: "call-error".into(),
        tool_name: tau_proto::ToolName::new("search"),
        tool_type: tau_proto::ToolType::Function,
        message: "SECRET_ERROR".to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "call-cancel".into(),
        tool_name: tau_proto::ToolName::new("write"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    sync(&handle);

    let completed_compact = vt.screen_text(120).join("\n");
    for hidden in [
        "read",
        "search",
        "write",
        "SECRET_RESULT",
        "SECRET_ERROR",
        "SECRET_ARGUMENT",
    ] {
        assert!(!completed_compact.contains(hidden), "{completed_compact}");
    }

    renderer.toggle_verbose_mode();
    sync(&handle);
    let restored = vt.screen_text(120).join("\n");
    assert!(restored.contains("private reasoning"), "{restored}");
    assert!(restored.contains('Δ'), "{restored}");
    assert!(restored.contains("read"), "{restored}");
    assert!(restored.contains("search"), "{restored}");
    assert!(restored.contains("write"), "{restored}");
    assert!(restored.contains("SECRET_ERROR"), "{restored}");
}

/// Live and attach-reconstructed blocks must use the same mode projection when
/// they move between visible and detached agent transcripts.
#[test]
fn verbose_mode_reprojects_streaming_thinking_hidden_agents_and_attach_tools() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-live",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("main-live"),
            "",
            Some("main live reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "main live reasoning"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "main live reasoning"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "main live reasoning"));

    let mut worker_created = agent_prompt_created("worker-live", "s1");
    worker_created.agent_id = agent_id("worker");
    renderer.handle(&Event::AgentPromptCreated(worker_created));
    let mut worker_update = provider_response_delta_update(
        test_agent_prompt_id("worker-live"),
        "",
        Some("worker hidden reasoning".to_owned()),
        tau_proto::PromptOriginator::User,
    );
    worker_update.agent_id = agent_id("worker");
    renderer.handle(&Event::ProviderResponseUpdated(worker_update));

    renderer.toggle_verbose_mode();
    renderer.switch_agent("worker".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "worker hidden reasoning"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "worker hidden reasoning"));

    renderer.toggle_verbose_mode();
    let mut reconstructed = tool_started(
        "attach-tool",
        "read",
        CborValue::Text("SECRET_ATTACH_ARGUMENT".to_owned()),
    );
    let Event::ToolStarted(started) = &mut reconstructed else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("worker");
    renderer.handle_reconstructed_tool_start_socket_delivery(
        &reconstructed,
        &agent_id("worker"),
        tau_proto::UnixMicros::new(1),
        1,
    );
    sync(&handle);
    let pending = vt.screen_text(100).join("\n");
    assert!(pending.contains("read pending"), "{pending}");
    assert!(!pending.contains("SECRET_ATTACH_ARGUMENT"), "{pending}");

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "attach-tool".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "read pending"));
}

#[test]
fn show_tools_summarize_turn_summarizes_tool_batch() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "summarize-turn");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("pattern".into()),
                    CborValue::Text("foo".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 0/2 …"));
    assert!(!vt.screen_contains(80, "read src/main.rs"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        message: "nope".into(),
        details: None,
        display: Some(tau_proto::ToolUseState {
            args: "foo".into(),
            status: tau_proto::ToolUseStatus::Error,
            status_text: "err: nope".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 1L, 13B ok: 1 err: 1"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo err: nope"));
}

#[test]
fn show_tools_summarize_prompt_aggregates_across_tool_followups() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "summarize-prompt");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/1 1L, 13B ok: 1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-2".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("pattern".into()),
                CborValue::Text("foo".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/2 1L, 13B ok: 1 …"));
    assert!(!vt.screen_contains(80, "tools 1/1"));
    assert!(!vt.screen_contains(80, "grep foo"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "foo".into(),
            stats: tau_proto::ToolUseStats {
                matches: Some(3),
                lines: None,
                bytes: None,
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 3, 1L, 13B ok: 2"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo (3 matches) ok"));
}

#[test]
fn show_tools_compact_hides_payload_body() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "compact");

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolUseStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: "fn main() {}\n".into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 0s ok"));
    assert!(!vt.screen_contains(80, "fn main()"));
}

/// A bounded one-line tool header keeps its identity, status, and timing while
/// full mode reveals the exact Unicode payload and compact mode hides it.
#[test]
fn show_tools_full_reveals_truncated_one_line_payload() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "full");
    let payload = "αβγδεζηθικλμνξοπρστυφχψω一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let args = "αβγδεζηθικλμνξοπρστυ┄一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text(payload.into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text(payload.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: args.into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: payload.into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    let header = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("shell "))
        .expect("bounded shell header");
    assert!(header.contains('┄'), "{header:?}");
    assert!(header.contains(" 1/300s ok"), "{header:?}");
    assert!(!header.contains(args), "{header:?}");
    assert!(
        vt.screen_text(100).iter().any(|row| row.trim() == payload),
        "full Unicode payload should render beneath the compact header"
    );

    renderer.apply_setting("show-tools", "compact");
    sync(&handle);
    let header = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("shell "))
        .expect("bounded compact shell header");
    assert!(header.contains('┄'), "{header:?}");
    assert!(header.contains(" 1/300s ok"), "{header:?}");
    assert!(
        !vt.screen_text(100).iter().any(|row| row.trim() == payload),
        "compact mode should continue hiding payload bodies"
    );
}

#[test]
fn show_tools_off_hides_tool_blocks() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "off");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "tools"));
    assert!(!vt.screen_contains(80, "read"));
}

#[test]
fn websearch_tool_result_shows_result_count_and_size() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-web".into(),
        tool_name: tau_proto::ToolName::new("websearch_exa"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(
            "Title: One\nURL: https://one.example\n\nTitle: Two\nURL: https://two.example\n".into(),
        ),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: String::new(),
            stats: tau_proto::ToolUseStats {
                matches: Some(2),
                lines: Some(193),
                bytes: Some(7370),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "websearch_exa 2, 193L, 7.2kB ok"));
}

#[test]
fn streaming_block_does_not_duplicate_on_finish() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello!",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello!")],
    )));
    sync(&handle);

    // Count how many rows contain "hello!".
    let count = vt
        .screen_text(80)
        .iter()
        .filter(|r| r.contains("hello!"))
        .count();
    assert_eq!(
        count,
        1,
        "response should appear exactly once, got {count}: {:?}",
        vt.screen_text(80)
    );
}
#[test]
fn render_tool_use_state_assembles_chips_in_order() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    // grep-style: matches + stats + status.
    let display = ToolUseState {
        args: "\"foo\" in src".into(),
        stats: ToolUseStats {
            matches: Some(3),
            lines: Some(7),
            bytes: Some(120),
        },
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("grep", &display);
    assert_eq!(rendered.tool_name, "grep");
    assert_eq!(rendered.args, "\"foo\" in src");
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["3, 7L, 120B", "ok"]);
    assert!(matches!(
        rendered.suffixes.last().expect("status suffix").status,
        ToolStatus::Success
    ));
}

/// Every structured progress counter keeps counter priority, including custom
/// and unlabelled counters, while free-form info remains independently lower.
#[test]
fn structured_counters_outrank_generic_info() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            progress_counters: vec![
                ProgressCounter {
                    label: Some("count".into()),
                    unit: ProgressUnit::Count,
                    complete: Some(1),
                    total: None,
                },
                ProgressCounter {
                    label: None,
                    unit: ProgressUnit::Count,
                    complete: Some(2),
                    total: None,
                },
            ],
            info_chips: vec!["optional".into()],
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    assert!(matches!(display.suffixes[0].status, ToolStatus::Counter));
    assert!(matches!(display.suffixes[1].status, ToolStatus::Counter));
    assert!(matches!(display.suffixes[2].status, ToolStatus::Info));

    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 18);
    assert_eq!(header, "tool count: 1 2 ok");
}

/// A status descriptor keeps its state and title visible beside terminal
/// duration/outcome metadata, and truncates the title before hiding the
/// outcome.
#[test]
fn status_tool_header_preserves_semantics_and_outcome_when_narrow() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let mut display = render_tool_use_state(
        "status",
        &ToolUseState {
            args: "working: implementing focused renderer regression coverage".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let status_index = display.suffixes.len().saturating_sub(1);
    display.suffixes.insert(
        status_index,
        crate::tool_render::tool_duration_suffix(path_std_time::Duration::from_secs(12)),
    );
    let block = render_tool_block(&cli_test_theme(), &display);

    assert_eq!(
        priority_header_text(&block, 100),
        "status working: implementing fo┄rer regression coverage 12s ok"
    );
    assert_eq!(
        priority_header_text(&block, 25),
        "status worki┄erage 12s ok"
    );
    assert_eq!(priority_header_text(&block, 9), "status ok");
}

/// Only a complete syntactically valid agent reference receives the agent-id
/// priority; arbitrary free-form `@` chips retain generic-info semantics.
#[test]
fn info_chip_agent_classification_is_strict() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            info_chips: vec!["@engineer_child".into(), "@not an agent".into()],
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );

    assert!(matches!(display.suffixes[0].status, ToolStatus::Agent));
    assert!(matches!(display.suffixes[1].status, ToolStatus::Info));
}

/// Tool-line elements retain their documented truncation order, including the
/// task-title band that yields after a display name and before telemetry.
#[test]
fn tool_line_priorities_cover_every_element() {
    let priorities = [
        (ToolLineElement::Identity, 0),
        (ToolLineElement::ResultStatus, 10),
        (ToolLineElement::ErrorDetails, 20),
        (ToolLineElement::Arguments, 30),
        (ToolLineElement::AgentId, 40),
        (ToolLineElement::Mode, 50),
        (ToolLineElement::Range, 60),
        (ToolLineElement::Counter, 70),
        (ToolLineElement::WorkTitle, 75),
        (ToolLineElement::Info, 80),
        (ToolLineElement::Duration, 90),
    ];

    for (element, expected) in priorities {
        assert_eq!(element.priority().get(), expected);
    }
}

/// Exact narrow boundaries must retain minimum middle-truncated arguments and
/// agent ids, then drop them by priority while keeping `err` atomic and
/// visible.
#[test]
fn tool_error_line_degrades_at_exact_priority_boundaries() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "extraordinarily_long_tool",
        &ToolUseState {
            mode: "read-write-mode".into(),
            args: "arguments-abcdefghijklmnopqrstuvwxyz".into(),
            range: Some(ToolUseRange {
                start: Some("2026-01-01".into()),
                end: Some("2026-12-31".into()),
            }),
            info_chips: vec![
                "@agent-abcdefghijklmnopqrstuvwxyz".into(),
                "optional-information".into(),
            ],
            status: ToolUseStatus::Error,
            status_text: "permission denied for a very long resource name".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);

    let at_all_minima = priority_header_text(&block, 25);
    assert_eq!(at_all_minima, "ex┄l ar┄yz @a┄yz err: ┄me");

    let without_agent = priority_header_text(&block, 24);
    assert!(without_agent.contains("ar┄yz"), "{without_agent:?}");
    assert!(!without_agent.contains('@'), "{without_agent:?}");
    assert!(without_agent.contains("err:"), "{without_agent:?}");

    let without_arguments = priority_header_text(&block, 18);
    assert!(
        !without_arguments.contains("ar┄yz"),
        "{without_arguments:?}"
    );
    assert!(!without_arguments.contains('@'), "{without_arguments:?}");
    assert!(without_arguments.contains("err:"), "{without_arguments:?}");

    let status_only_detail_drop = priority_header_text(&block, 12);
    assert!(status_only_detail_drop.ends_with(" err"));
    assert!(!status_only_detail_drop.contains("permission"));

    assert_eq!(priority_header_text(&block, 7), "");
}

/// Every documented truncatable tool category must enforce its configured
/// maximum at a wide terminal rather than reverting to unbounded content.
#[test]
fn tool_line_truncation_maxima_cover_every_category() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        &"i".repeat(40),
        &ToolUseState {
            mode: "m".repeat(30),
            args: "a".repeat(60),
            range: Some(ToolUseRange {
                start: Some("r".repeat(40)),
                end: None,
            }),
            info_chips: vec![format!("@agent_{}", "g".repeat(40))],
            status: ToolUseStatus::Error,
            status_text: "d".repeat(60),
            ..Default::default()
        },
    );
    assert!(matches!(display.suffixes[0].status, ToolStatus::Agent));
    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 300);
    let fields: Vec<&str> = header.split_whitespace().collect();

    assert_eq!(fields[0].chars().count(), 32, "{header:?}");
    assert_eq!(fields[1].chars().count(), 16, "{header:?}");
    assert_eq!(fields[2].chars().count(), 48, "{header:?}");
    assert_eq!(fields[3].chars().count(), 32, "{header:?}");
    assert_eq!(fields[4].chars().count(), 32, "{header:?}");
    assert_eq!(fields[5], "err:");
    assert_eq!(fields[6].chars().count(), 46, "{header:?}");
    for field in [
        &fields[0], &fields[1], &fields[2], &fields[3], &fields[4], &fields[6],
    ] {
        assert!(field.contains('┄'), "{field:?} in {header:?}");
    }
}

/// Mode and range retain their exact configured minima together, then the
/// lower-priority range disappears cleanly one column below that boundary.
#[test]
fn tool_line_mode_and_range_minimum_boundaries_are_exact() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            mode: "mode-value".into(),
            range: Some(ToolUseRange {
                start: Some("range-value".into()),
                end: None,
            }),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);
    let at_minima = priority_header_text(&block, 17);
    assert_eq!(at_minima, "tool m┄e ra┄.. ok");

    let without_range = priority_header_text(&block, 16);
    assert!(without_range.contains("mode"));
    assert!(!without_range.contains("range"));
    assert!(without_range.ends_with(" ok"));
}

/// Success and failure labels must both remain exact essential elements: a
/// terminal too narrow for identity plus status renders no ambiguous tool row.
#[test]
fn tool_result_status_never_truncates_or_disappears_alone() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    for (status, status_text, exact_width, expected) in [
        (ToolUseStatus::Success, "ok", 7, "ab┄z ok"),
        (ToolUseStatus::Error, "failure", 8, "ab┄z err"),
    ] {
        let display = render_tool_use_state(
            "abcdefghijklmnopqrstuvwxyz",
            &ToolUseState {
                status,
                status_text: status_text.into(),
                ..Default::default()
            },
        );
        let block = render_tool_block(&cli_test_theme(), &display);
        assert_eq!(priority_header_text(&block, exact_width), expected);
        assert_eq!(priority_header_text(&block, exact_width - 1), "");
    }
}

/// Empty protocol labels must still produce explicit truthful lifecycle
/// statuses so the essential status band cannot silently vanish.
#[test]
fn empty_tool_status_labels_receive_unambiguous_defaults() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    for (status, expected) in [
        (ToolUseStatus::Success, "ok"),
        (ToolUseStatus::Warning, "warn"),
        (ToolUseStatus::Error, "err"),
        (
            ToolUseStatus::InProgress,
            tau_proto::PROGRESS_INDICATOR_TEXT,
        ),
    ] {
        for supplied in [" \t ", "\u{200b}\u{200d}\u{fe0f}"] {
            let display = render_tool_use_state(
                "tool",
                &ToolUseState {
                    status,
                    status_text: supplied.into(),
                    ..Default::default()
                },
            );
            assert_eq!(
                display.suffixes.last().map(|suffix| suffix.text.as_str()),
                Some(expected)
            );
            let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 80);
            assert_eq!(header, format!("tool {expected}"));
        }
    }
}

/// Tool-line truncation must measure wide graphemes in terminal columns and
/// recompute from full immutable content when the same block is resized.
#[test]
fn tool_line_unicode_resize_restores_bounded_content() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "read",
        &ToolUseState {
            args: "ab界cd界efghijklmnopqrstuvwxyz".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);
    let wide = priority_header_text(&block, 40);
    let narrow = priority_header_text(&block, 13);
    assert!(wide.contains('界'));
    assert!(narrow.contains('┄'));
    assert!(narrow.ends_with(" ok"));
    assert_eq!(priority_header_text(&block, 40), wide);
}

/// Untrusted tool fields must remain one row and pass through the terminal
/// cell sanitizer while the adaptive header removes embedded line breaks.
#[test]
fn tool_line_preserves_control_character_safety() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "unsafe\nname",
        &ToolUseState {
            args: "alpha\tbeta\u{1b}[2J\nomega".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 80);

    assert!(!header.contains('\n'));
    assert!(!header.contains('\t'));
    assert!(!header.contains('\u{1b}'));
    assert!(header.contains("unsafe name"));
    assert!(header.contains("alpha beta�[2J omega"));
    assert!(header.ends_with(" ok"));
}

#[test]
fn render_tool_use_state_keeps_range_separate_from_args() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "feed/main".into(),
        range: Some(ToolUseRange {
            start: Some("2026-05-29".into()),
            end: Some("2026-05-30".into()),
        }),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("calendar", &display);
    assert_eq!(rendered.args, "feed/main");
    assert_eq!(rendered.range.as_deref(), Some("2026-05-29..2026-05-30"));
}

/// Workdir get/set labels remain accessible, prefix-safe, and compact in plain
/// and styled output.
#[test]
fn workdir_result_modes_render_consistently() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let long_path = format!("/{}", "segment/".repeat(10));
    let display = ToolUseState {
        mode: "set".into(),
        args: long_path,
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("project_a__workdir", &display);
    let theme = cli_test_theme();
    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 120);
    let plain: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = plain.find(" set ").expect("structural set mode") + 1;
    let mode_style = cells[mode_start].style;
    assert!(plain.contains("/segment/"));
    assert!(plain.contains('┄'));
    assert!(plain.trim_end().ends_with(" ok"));
    assert_eq!(
        mode_style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
    assert_ne!(
        mode_style,
        cells[plain.find("/segment").expect("compacted path")].style
    );
}

#[test]
fn running_shell_display_keeps_mode_separate_for_dedicated_style() {
    let theme = cli_test_theme();
    let display = tau_proto::ToolUseState {
        args: "printf hello".to_owned(),
        mode: "rw".to_owned(),
        status: tau_proto::ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.mode, "rw");
    assert_eq!(rendered.args, "printf hello");

    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 80);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = text.find(" rw ").expect("mode span") + 1;
    assert_eq!(
        cells[mode_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
}

#[test]
fn render_tool_block_paints_mode_with_dedicated_style() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let theme = cli_test_theme();
    let display = ToolUseState {
        mode: "rw".into(),
        args: "printf hello".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.mode, "rw");
    assert_eq!(rendered.args, "printf hello");

    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 80);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = text.find(" rw ").expect("mode span") + 1;
    assert_eq!(
        cells[mode_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
}

#[test]
fn delegate_completion_keeps_input_stats_with_output_stats() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text("ok\nmore".into()), None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolUseStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_uses_output_stats_from_duration_result_map() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };
    let details = CborValue::Map(vec![
        (
            CborValue::Text("output".into()),
            CborValue::Text("ok\nmore".into()),
        ),
        (
            CborValue::Text("duration_seconds".into()),
            CborValue::Integer(6.into()),
        ),
    ]);

    let display = build_delegate_completion_display(Some(&cached), &details, None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolUseStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_keeps_input_stats_for_empty_output() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text(String::new()), None);

    assert_eq!(display.stats, ToolUseStats::default());
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

/// Context counters use the status bar's compact magnitude policy without
/// changing the broader token-progress formatting policy.
#[test]
fn render_tool_use_state_token_progress_formats_context_like_status_bar() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "[research]".into(),
        progress_counters: vec![
            ProgressCounter {
                label: Some("ctx".into()),
                unit: ProgressUnit::Tokens,
                complete: Some(133_400),
                total: Some(200_000),
            },
            ProgressCounter {
                label: Some("tokens".into()),
                unit: ProgressUnit::Tokens,
                complete: Some(133_400),
                total: Some(200_000),
            },
        ],
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("agent_start", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "#133k/200k",
            "tokens: 133.4k/200k",
            tau_proto::PROGRESS_INDICATOR_TEXT,
        ]
    );
}

/// Ensures watched-agent rows put self-reported status before stable identity
/// and title while preserving generic telemetry, styling, and truncation
/// priority.
#[test]
fn watched_agent_display_uses_tool_block_styles_and_counters() {
    let theme = cli_test_theme();
    let stats = tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 1,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats {
            input_tokens: Some(133_400),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(67),
        },
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    let status = tau_proto::AgentWatchWorkStatusNotification {
        session_id: test_session_id("s1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 1,
        phase: tau_proto::AgentWorkStatusPhase::Working,
        title: Some("review changes".to_owned()),
        initial: false,
    };

    let display = watched_agent_tool_display(
        Some("review"),
        "engineer_1",
        None,
        Some(&stats),
        WatchedAgentActivity::Running,
        Some(&status),
    );
    assert_eq!(display.tool_name, "@engineer_1");
    assert_eq!(display.args, "");
    let leading: Vec<&str> = display
        .leading_segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(
        display
            .status_prefix
            .as_ref()
            .map(|(text, _)| text.as_str()),
        Some("🚀💤")
    );
    assert_eq!(leading, vec!["(review)", "review changes"]);
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["%2/3", "#133k/200k"]);

    let block = render_tool_block(&theme, &display);
    assert_eq!(
        priority_header_text(&block, 100),
        "🚀💤 @engineer_1 (review) review changes %2/3 #133k/200k"
    );
    let wide_cells = priority_header_cells(&block, 100);
    let identity_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '@')
        .expect("stable identity span");
    assert_eq!(
        wide_cells[identity_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME),
        "stable agent identity keeps the watched-agent style"
    );
    let display_name_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '(')
        .expect("display name span")
        + 1;
    let phase_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '🚀')
        .expect("work phase span");
    let title_start = wide_cells
        .iter()
        .rposition(|cell| cell.ch == 'r')
        .expect("self-reported title span");
    assert_eq!(
        wide_cells[display_name_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "persisted display names remain informational metadata"
    );
    assert_eq!(
        wide_cells[phase_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROGRESS_INDICATOR),
        "self-reported work phases retain the progress semantic style"
    );
    assert_eq!(
        wide_cells[title_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "self-reported task titles remain informational metadata"
    );
    let without_display_name = priority_header_text(&block, 40);
    assert!(!without_display_name.contains("(review)"));
    assert!(
        without_display_name.contains("🚀"),
        "{without_display_name:?}"
    );
    assert!(
        without_display_name.contains("re┄es"),
        "{without_display_name:?}"
    );
    let without_task_title = priority_header_text(&block, 19);
    assert!(
        without_task_title.starts_with("🚀💤 @"),
        "{without_task_title:?}"
    );
    assert!(without_task_title.contains("🚀"), "{without_task_title:?}");
    assert!(
        !without_task_title.contains("review"),
        "{without_task_title:?}"
    );

    let percent_only_stats = tau_proto::AgentStatsUpdated {
        context: tau_proto::AgentContextStats {
            input_tokens: None,
            cached_tokens: None,
            context_window: None,
            percent_used: Some(67),
        },
        ..stats
    };
    let display = watched_agent_tool_display(
        Some("review"),
        "engineer_1",
        None,
        Some(&percent_only_stats),
        WatchedAgentActivity::Watching { witness: "leaf" },
        Some(&status),
    );
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(display.tool_name, "@engineer_1");
    assert_eq!(texts, vec!["-> @leaf", "%2/3", "#67%"]);
    let block = render_tool_block(&theme, &display);
    let watching = priority_header_cells(&block, 100)
        .into_iter()
        .find(|cell| cell.ch == '@')
        .expect("stable identity cell");
    assert_eq!(
        watching.style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME)
    );
    assert_eq!(watching.style.fg, Some(Color::DarkYellow));
}

/// Indirect-row context styles its parent ID as a watched-agent identity while
/// retaining the `via` label's context style. It also follows stable identity
/// at practical widths, while narrow layouts retain the fixed work-status
/// prefix and a distinguishable identity within the exact terminal budget.
#[test]
fn watched_agent_indirect_context_respects_width_priorities() {
    let theme = cli_test_theme();
    let status = tau_proto::AgentWatchWorkStatusNotification {
        session_id: test_session_id("s1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: 1,
        phase: tau_proto::AgentWorkStatusPhase::Working,
        title: Some("review changes".to_owned()),
        initial: false,
    };
    let stats = tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-with-long-id"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 1,
        },
        context: tau_proto::AgentContextStats {
            input_tokens: None,
            cached_tokens: None,
            context_window: None,
            percent_used: Some(67),
        },
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    let display = watched_agent_tool_display(
        None,
        "worker-with-long-id",
        Some("reviewer"),
        Some(&stats),
        WatchedAgentActivity::Idle,
        Some(&status),
    );
    let block = render_tool_block(&theme, &display);

    assert_eq!(
        priority_header_text(&block, 100),
        "🚀💤 @worker-with-long-id via @reviewer review changes %1/1 #67%"
    );
    let cells = priority_header_cells(&block, 100);
    let parent_identity_start = cells
        .iter()
        .rposition(|cell| cell.ch == '@')
        .expect("recursive watch parent identity");
    assert_eq!(
        cells[parent_identity_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME),
        "recursive watch parent identity uses the watched-agent identity style"
    );
    let via_start = cells
        .iter()
        .position(|cell| cell.ch == 'v')
        .expect("recursive watch context label");
    assert_eq!(
        cells[via_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "recursive watch context label remains agent-context metadata"
    );
    let boundary = priority_header_text(&block, 39);
    assert!(boundary.contains("via @reviewer"), "{boundary:?}");
    assert!(boundary.starts_with("🚀💤 @"), "{boundary:?}");
    for width in [12, 16, 24, 40] {
        let text = priority_header_text(&block, width);
        assert!(text.starts_with("🚀💤 @"), "{width}: {text:?}");
        assert!(
            tau_term_screen::display_width(&text) <= width,
            "{width}: {text:?}"
        );
    }
}

#[test]
fn render_tool_use_state_text_payload_is_preserved_for_block_rendering() {
    use tau_proto::{ToolUsePayload, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "printf hello".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolUsePayload::Text {
            text: "printf hello\nprintf world".into(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.args, "printf hello");
    assert_eq!(rendered.payload, display.payload);
}

#[test]
fn render_tool_use_state_diff_payload_adds_plus_minus_chips() {
    use tau_proto::{DiffSummary, ToolUsePayload, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "src/main.rs".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolUsePayload::Diff(DiffSummary {
            added: 12,
            removed: 3,
            hunks: vec![],
        })),
        ..Default::default()
    };
    let rendered = render_tool_use_state("edit", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["+12", "-3", "ok"]);
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::DiffAdded));
    assert!(matches!(
        rendered.suffixes[1].status,
        ToolStatus::DiffRemoved
    ));
}

#[test]
fn render_diff_tool_block_uses_unified_diff_line_prefixes() {
    use tau_proto::{DiffHunk, DiffLine, DiffSegment, DiffSummary, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "edit",
        &ToolUseState {
            args: "src/main.rs 10..11".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let diff = DiffSummary {
        added: 2,
        removed: 2,
        hunks: vec![DiffHunk {
            old_start: 10,
            old_count: 2,
            new_start: 10,
            new_count: 2,
            lines: vec![
                DiffLine::Equal {
                    text: "    unchanged();".into(),
                },
                DiffLine::Remove {
                    text: "    old();".into(),
                },
                DiffLine::Add {
                    text: "    new();".into(),
                },
                DiffLine::Modify {
                    old: vec![
                        DiffSegment::Equal {
                            text: "let x = ".into(),
                        },
                        DiffSegment::Remove { text: "1".into() },
                        DiffSegment::Equal { text: ";".into() },
                    ],
                    new: vec![
                        DiffSegment::Equal {
                            text: "let x = ".into(),
                        },
                        DiffSegment::Add { text: "2".into() },
                        DiffSegment::Equal { text: ";".into() },
                    ],
                },
            ],
        }],
    };

    let block = render_diff_tool_block(&cli_test_theme(), &display, &diff, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.starts_with("@@ -10,2 +10,2 @@\n     unchanged();"));
    assert!(text.contains("\n-    old();"));
    assert!(text.contains("\n+    new();"));
    assert!(text.contains("\n-let x = 1;\n+let x = 2;"));
    assert!(!text.contains("\n-     old();"));
    assert!(!text.contains("\n+     new();"));
    let removed_line = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "-    old();")
        .expect("removed line uses one span");
    assert_eq!(removed_line.style.fg, Some(tau_cli_term::Color::DarkRed));

    let added_line = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "+    new();")
        .expect("added line uses one span");
    assert_eq!(added_line.style.fg, Some(tau_cli_term::Color::DarkGreen));

    let changed_removed = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "1")
        .expect("removed changed token is split into its own span");
    assert_eq!(changed_removed.style.fg, Some(tau_cli_term::Color::Red));
    assert!(changed_removed.style.bold);

    let changed_added = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "2")
        .expect("added changed token is split into its own span");
    assert_eq!(changed_added.style.fg, Some(tau_cli_term::Color::Green));
    assert!(changed_added.style.bold);
}

/// Ensures multi-file mutation payloads keep per-file headers and aggregate
/// diff chips, so apply_patch results can show structured UI diffs for every
/// changed file instead of falling back to plain text summaries.
#[test]
fn render_multi_diff_tool_block_preserves_file_boundaries() {
    use tau_proto::{
        DiffHunk, DiffLine, DiffSummary, FileDiffSummary, ToolUsePayload, ToolUseState,
        ToolUseStatus,
    };

    let files = vec![
        FileDiffSummary {
            path: "a.txt".into(),
            diff: DiffSummary {
                added: 1,
                removed: 0,
                hunks: vec![DiffHunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![DiffLine::Add {
                        text: "alpha".into(),
                    }],
                }],
            },
        },
        FileDiffSummary {
            path: "b.txt".into(),
            diff: DiffSummary {
                added: 0,
                removed: 1,
                hunks: vec![DiffHunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 0,
                    new_count: 0,
                    lines: vec![DiffLine::Remove {
                        text: "beta".into(),
                    }],
                }],
            },
        },
    ];
    let display = render_tool_use_state(
        "apply_patch",
        &ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(ToolUsePayload::Diffs {
                files: files.clone(),
            }),
            ..Default::default()
        },
    );

    let suffixes: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(suffixes, vec!["+1", "-1", "ok"]);
    let block = render_multi_diff_tool_block(&cli_test_theme(), &display, &files, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.starts_with("--- a.txt"));
    assert!(text.contains("\n+alpha"));
    assert!(text.contains("\n--- b.txt"));
    assert!(text.contains("\n-beta"));
}

/// Ensures a one-file `Diffs` payload still labels its hunks, because freeform
/// apply_patch calls have no compact argument path that could identify them.
#[test]
fn render_multi_diff_tool_block_labels_single_file_hunks_once() {
    use tau_proto::{
        DiffHunk, DiffLine, DiffSummary, FileDiffSummary, ToolUsePayload, ToolUseState,
        ToolUseStatus,
    };

    let files = vec![FileDiffSummary {
        path: "src/lib.rs".into(),
        diff: DiffSummary {
            added: 1,
            removed: 1,
            hunks: vec![DiffHunk {
                old_start: 3,
                old_count: 1,
                new_start: 3,
                new_count: 1,
                lines: vec![
                    DiffLine::Remove { text: "old".into() },
                    DiffLine::Add { text: "new".into() },
                ],
            }],
        },
    }];
    let display = render_tool_use_state(
        "apply_patch",
        &ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(ToolUsePayload::Diffs {
                files: files.clone(),
            }),
            ..Default::default()
        },
    );
    let block = render_multi_diff_tool_block(&cli_test_theme(), &display, &files, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert_eq!(text.matches("--- src/lib.rs").count(), 1);
    assert!(text.contains("@@ -3,1 +3,1 @@"));
    assert!(text.contains("\n-old"));
    assert!(text.contains("\n+new"));
}

#[test]
fn synthesize_fallback_display_is_minimal() {
    let ok = synthesize_fallback_display("my_tool", None);
    assert_eq!(ok.args, "");
    assert_eq!(ok.status_text, "ok");
    assert!(matches!(ok.status, tau_proto::ToolUseStatus::Success));

    let err =
        synthesize_fallback_display("my_tool", Some("failure description\nwith trailing line"));
    assert_eq!(err.status_text, "failure description");
    assert!(matches!(err.status, tau_proto::ToolUseStatus::Error));
}

#[test]
fn fallback_error_status_is_abbreviated_only_by_renderer() {
    let message =
        "failed to access /home/dpc/agent/.agents/skills: No such file or directory (os error 2)";
    let display = synthesize_fallback_display("ls", Some(message));
    assert_eq!(display.status_text, message);
    assert!(!display.status_text.contains("err:"));
    assert!(!display.status_text.contains('…'));

    let rendered = render_tool_use_state("ls", &display);
    let block = render_tool_block(&cli_test_theme(), &rendered);
    let text = priority_header_text(&block, 80);

    assert!(text.contains('┄'));
    assert!(!text.contains('…'));
}

#[test]
fn render_tool_use_state_error_status_picks_error_severity() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "/etc".into(),
        status: ToolUseStatus::Error,
        status_text: "permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("ls", &display);
    assert_eq!(rendered.suffixes.len(), 1);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::Error));

    let legacy_display = ToolUseState {
        args: "/etc".into(),
        status: ToolUseStatus::Error,
        status_text: "err: permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("ls", &legacy_display);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
}

#[test]
fn render_tool_block_abbreviates_inline_args_and_error_but_preserves_payload() {
    use tau_proto::{ToolUsePayload, ToolUseState, ToolUseStatus};

    let payload = "full payload line one\nfull payload line two".to_owned();
    let display = ToolUseState {
        args: "LOG_MODULE_WALLETV2|LOG_CLIENT_MODULE_WALLETV2 in modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src".into(),
        status: ToolUseStatus::Error,
        status_text: "ripgrep error: rg: modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src: IO error for operation".into(),
        payload: Some(ToolUsePayload::Text {
            text: payload.clone(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_use_state("grep", &display);
    let block = render_tool_block(&cli_test_theme(), &rendered);
    let header = priority_header_text(&block, 200);
    let body: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(header.contains("LOG_MODULE_WALLETV2|"));
    assert!(header.contains("walletv2-client/src"));
    assert!(header.contains("err: ripgrep error:"));
    assert!(header.contains("IO error for operation"));
    assert_eq!(header.matches('┄').count(), 2);
    assert!(!header.contains(&display.args));
    assert!(!header.contains(&display.status_text));
    assert!(body.contains(&payload));
}

#[test]
fn render_shell_block_abbreviates_inline_command_and_status_but_preserves_output() {
    let command = "printf 1234567890123456789012345678901234567890";
    let status = "err: command failed after printing a very long diagnostic";
    let output = "full output line one\nfull output line two";
    let block = render_shell_block(&cli_test_theme(), command, output, Some(status));
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("printf 1234567890123┄12345678901234567890"));
    assert!(text.contains("err: command failed ┄very long diagnostic"));
    assert!(!text.contains(status));
    assert!(text.contains(output));
}

#[test]
fn format_turn_stats_line_formats_short_latencies_as_millis() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 17_341,
        prompt_cached_tokens: 16_896,
        prompt_cache_read_ceiling_tokens: Some(17_341),
        response_received_tokens: 29,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 100_000,
                cached_tokens: 50_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 16_000,
        response_received_tokens: 1_341,
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        Some(&previous_usage),
        Some(Duration::from_millis(1_240)),
        Some(Duration::from_millis(4_560)),
    );

    assert_eq!(line, "Δ97% 16.8k/17.3k ↑0 ↓29 1240ms Σ↑50k/100k ↓0 4560ms",);
}

#[test]
fn format_turn_stats_line_formats_long_latencies_compactly() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_cache_read_ceiling_tokens: Some(0),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        None,
        Some(Duration::from_millis(18_723)),
        Some(Duration::from_secs(5 * 60 + 1)),
    );

    assert_eq!(line, "Δ— 0/0 ↑0 ↓0 18s Σ↑0/1k ↓0 5m");
}

#[test]
fn format_turn_stats_line_uses_previous_turn_for_hit_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_000,
        prompt_cache_read_ceiling_tokens: Some(20_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_000,
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ95% 19k/20k ↑100 ↓0 Σ↑19k/40.1k ↓0");
}

/// Ensures a missing provider ceiling uses the existing bounded reusable-prefix
/// calculation and visibly marks both the derived ratio and denominator.
#[test]
fn format_turn_stats_line_estimates_unknown_cache_ceiling() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 121_300,
        prompt_cached_tokens: 120_300,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 120_000,
        response_received_tokens: 1_300,
        ..Default::default()
    };

    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ99%? 120.3k/121.3k? ↑0 ↓0 Σ↑0/0 ↓0");
}

/// Ensures a nonzero reusable prefix with no provider cache read remains a
/// visibly approximate zero-percent estimate rather than an exact cache miss.
#[test]
fn format_turn_stats_line_marks_estimated_zero_cache_read() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 121_300,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 120_000,
        response_received_tokens: 1_300,
        ..Default::default()
    };

    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ0%? 0/121.3k? ↑0 ↓0 Σ↑0/0 ↓0");
}

/// Ensures a provider chain reset cannot show more cacheable tokens than the
/// current full-replay request contains.
#[test]
fn format_turn_stats_line_caps_cache_possible_after_chain_reset() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 13_659,
        prompt_cached_tokens: 3_840,
        prompt_cache_read_ceiling_tokens: Some(13_659),
        response_received_tokens: 116,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 157_101,
        response_received_tokens: 31,
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ28% 3.8k/13.6k ↑0 ↓116 Σ↑0/0 ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_nothing_could_be_cached() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ—? 0/0? ↑1k ↓0 Σ↑0/1k ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_no_prompt_sent() {
    let usage = tau_proto::ProviderTokenUsage::default();
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ—? 0/0? ↑0 ↓0 Σ↑0/0 ↓0");
}

/// Ensures malformed totals and cache counts that exceed the bounded estimate
/// retain invalid rendering instead of displaying an impossible percentage.
#[test]
fn format_turn_stats_line_rejects_invalid_or_inconsistent_cache_counts() {
    let malformed = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 1_001,
        ..Default::default()
    };
    let inconsistent = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_000,
        prompt_cached_tokens: 15_000,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        response_received_tokens: 1,
        ..Default::default()
    };

    assert_eq!(
        format_turn_stats_line(&malformed, None, None, None),
        "Δ! 1k/? ↑1k ↓0 Σ↑0/0 ↓0"
    );
    assert_eq!(
        format_turn_stats_line(&inconsistent, Some(&previous_usage), None, None),
        "Δ! 15k/? ↑9.9k ↓0 Σ↑0/0 ↓0"
    );
}

/// Action output begins with the notice marker while retaining the dedicated
/// styles that distinguish actionable approval identifiers and labels.
#[test]
fn render_action_output_block_highlights_approval_ids_and_labels() {
    let theme = cli_test_theme();
    let block = render_action_output_block(
        &theme,
        "Incoming approval 7\nstatus: pending\n8 account=personal folder=INBOX\n",
    );
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let label_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_LABEL);
    let marker_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER_SUBMITTED);

    let heading_id = spans
        .iter()
        .find(|span| span.text == "7")
        .expect("heading approval id span");
    let row_id = spans
        .iter()
        .find(|span| span.text == "8")
        .expect("list row approval id span");
    let status_label = spans
        .iter()
        .find(|span| span.text == "status:")
        .expect("status label span");
    let account_label = spans
        .iter()
        .find(|span| span.text == "account=")
        .expect("key-value label span");

    assert_eq!(heading_id.style, id_style);
    assert_eq!(row_id.style, id_style);
    assert_eq!(status_label.style, label_style);
    assert_eq!(account_label.style, label_style);
    assert_eq!(spans[0].text, "□ ");
    assert_eq!(spans[0].style, marker_style);
}

/// Action errors begin with the same notice marker without flattening their
/// identifier and diagnostic styles into the generic feedback style.
#[test]
fn render_action_error_block_uses_action_error_styles() {
    let theme = cli_test_theme();
    let block = render_action_error_block(&theme, "7", "invalid input");
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let error_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ERROR);
    let marker_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER_SUBMITTED);

    assert_eq!(spans[0].text, "□ ");
    assert_eq!(spans[0].style, marker_style);
    assert_eq!(spans[1].text, "7");
    assert_eq!(spans[1].style, id_style);
    assert_eq!(spans[3].text, "invalid input");
    assert_eq!(spans[3].style, error_style);
}

#[test]
fn render_turn_stats_block_uses_dedicated_styles() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 900,
        prompt_cache_read_ceiling_tokens: Some(1_000),
        response_received_tokens: 42,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 2_000,
                cached_tokens: 1_000,
                received_tokens: 100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ90%");
    assert!(spans[0].style.bold);
    assert_eq!(spans[0].style.fg, Some(Color::DarkGrey));
    assert_eq!(spans[1].text, " 900/1k");
    assert!(!spans[1].style.bold);
    assert_eq!(spans[1].style.fg, Some(Color::Red));
    let sigma = spans
        .iter()
        .find(|span| span.text == " Σ")
        .expect("sigma span is rendered");
    assert!(sigma.style.bold);
    assert_eq!(sigma.style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_warns_for_exact_99_percent_efficiency() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_456,
        prompt_cache_read_ceiling_tokens: Some(19_500),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_456,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 19_500,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ99%");
    assert_eq!(spans[1].text, " 19.4k/19.5k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_warns_cache_hit_above_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_100,
        prompt_cache_read_ceiling_tokens: Some(10_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ91%");
    assert_eq!(spans[1].text, " 9.1k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_highlights_cache_hit_at_or_below_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_000,
        prompt_cache_read_ceiling_tokens: Some(10_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ90%");
    assert_eq!(spans[1].text, " 9k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::Red));
}

/// Ensures cached tokens without any reusable predecessor are treated as
/// invalid for an estimated ceiling, just like an invalid exact ceiling.
#[test]
fn format_turn_stats_line_rejects_cache_without_reusable_prefix() {
    let unknown = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 2_000,
        prompt_cached_tokens: 1_500,
        ..Default::default()
    };
    let invalid = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 2_000,
        prompt_cached_tokens: 1_500,
        prompt_cache_read_ceiling_tokens: Some(1_000),
        ..Default::default()
    };
    assert!(format_turn_stats_line(&unknown, None, None, None).starts_with("Δ! 1.5k/?"));
    assert!(format_turn_stats_line(&invalid, None, None, None).starts_with("Δ! 1.5k/?"));
}

#[test]
fn streaming_block_handles_each_trailing_case() {
    let theme = cli_test_theme();
    let cases = [
        ("", "…"),
        ("Hello", "Hello …"),
        ("Hello ", "Hello …"),
        ("Hello\t", "Hello\t…"),
        ("line\n", "line\n…"),
        ("line\n  ", "line\n  …"),
    ];
    for (input, expected) in cases {
        let block = streaming_block(&theme, tau_themes::names::AGENT_RESPONSE, input);
        let actual: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(actual, expected, "input was {input:?}");
    }
}

/// Reproduces the user-reported bug: send 3 prompts during the
/// first response's streaming. After all responses complete, the
/// prompt must be visible and all 3 responses rendered.
#[test]
fn three_prompts_during_streaming_all_render_correctly() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // User sends first prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Agent starts streaming response 1.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hello"),
        "streaming should show, got: {:?}",
        vt.screen_text(80)
    );

    // User sends 2nd and 3rd prompts while streaming.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));

    // More streaming updates (multi-line, like a real LLM).
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "!\n\nHow can I help you today?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    // Response 1 finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(
            "Hello!\n\nHow can I help you today?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 should be in history, got: {:?}",
        vt.screen_text(80)
    );

    // Second prompt dispatched.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-1"),
            "Hello again!\n\nHow can I help you?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item(
            "Hello again!\n\nHow can I help you?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 should be visible, got: {:?}",
        vt.screen_text(80)
    );

    // Third prompt dispatched.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-2", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-2"),
            "Hi there!\n\nWhat can I help you with?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-2",
        vec![assistant_message_item(
            "Hi there!\n\nWhat can I help you with?",
        )],
    )));
    sync(&handle);

    // All three responses should be visible.
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "What can I help you with?"),
        "response 3 missing, got: {:?}",
        vt.screen_text(80)
    );

    // The prompt must be visible at the bottom.
    assert!(
        vt.screen_contains(80, "> "),
        "prompt should be visible after all responses, got: {:?}",
        vt.screen_text(80)
    );

    // No stale streaming blocks should remain.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

/// Emoji (wide characters) in responses must not corrupt the
/// layout. Each emoji occupies 2 terminal columns; if we count
/// them as 1, text after the emoji shifts right and wraps
/// incorrectly.
#[test]
fn emoji_in_response_renders_correctly() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Response with emoji followed by text on next line.
    let response = "Hello! 👋\n\nHow can I help you today?";
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            response,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);

    // "Hello! 👋" should be on its own line, not merged with the
    // next line.
    assert!(
        vt.screen_contains(40, "Hello!"),
        "emoji line missing, got: {:?}",
        text
    );
    // The text after \n\n should start at column 0, not offset.
    assert!(
        text.iter().any(|r| r.starts_with("How can I help")),
        "text after emoji should start at column 0, got: {:?}",
        text
    );
    // Prompt must be visible.
    assert!(
        vt.screen_contains(40, "> "),
        "prompt missing, got: {:?}",
        text
    );
}

/// Multiple emoji in a single line must not cause column drift.
#[test]
fn multiple_emoji_no_column_drift() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // 3 emoji = 6 columns + "end" = 9 columns total.
    let response = "🎉🎊🎈end\nnext line here";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    // "next line here" should start at column 0.
    assert!(
        text.iter().any(|r| r.starts_with("next line here")),
        "line after emoji should start at col 0, got: {:?}",
        text
    );
}

/// Replacing a long streaming block with its final settled output
/// must not leave stale partial lines behind, even when the live
/// block overflowed the viewport while streaming.
#[test]
fn overflowing_stream_replaced_cleanly_on_finish() {
    let (_term, handle, vt) = setup(40, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "overflow please".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    let partial = "stream 0\nstream 1\nstream 2\nstream 3\nPARTIAL ONLY";
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            partial,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(40, "PARTIAL ONLY"),
        "partial overflowed response should be visible before finish, got: {:?}",
        vt.screen_text(40)
    );

    let final_text = "final 0\nfinal 1\nfinal 2";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(final_text)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    assert!(
        vt.screen_contains(40, "final 1"),
        "final response missing, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "final 2"),
        "final response tail missing, got: {:?}",
        text
    );
    assert!(
        !vt.screen_contains(40, "PARTIAL ONLY"),
        "stale partial content should be gone, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "> "),
        "prompt should remain visible, got: {:?}",
        text
    );
}
