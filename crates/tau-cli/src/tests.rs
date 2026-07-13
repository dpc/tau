use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use tau_cli_term::TermHandle;
use tau_cli_term_raw::{Color, Term};
use tau_proto::{
    AgentCompactionTriggered, AgentPromptCreated, AgentPromptQueued, AgentPromptSteered,
    AgentPromptSubmitted, AgentPromptTerminated, AgentPromptTerminationReason, CborValue,
    ContentPart, ContextItem, ContextRole, Effort, Event, ExtAgentsMdAvailable, ExtensionReady,
    HarnessContextUsageChanged, HarnessRoleInfo, HarnessRoleSelected, HarnessRolesAvailable,
    MessageItem, OpaqueProviderItem, ProviderResponseFinished, ProviderResponseUpdated,
    ProviderStopReason, ServiceTier, SessionStartReason, SessionStarted, ThinkingSummary,
    ToolBackgroundResult, ToolCallItem, ToolCancelled, ToolError, ToolResult, UiPromptSubmitted,
    UiRoleUpdateAction, Verbosity,
};

use super::agent_navigation::AgentNavigationState;
use super::chat::{
    DraftSlot, custom_prompt_replacement, invalidate_pending_draft, is_local_slash_command,
    leading_slash_action, next_active_agent, queue_prompt_draft_snapshot,
    redacted_command_echo_line, redacted_prompt_history_line, retarget_prompt_draft_snapshot,
    role_cycling_enabled, should_send_draft_snapshot,
};
use super::event_renderer::{EventRenderer, watched_agent_tool_display};

fn cli_test_theme() -> tau_themes::Theme {
    tau_themes::Theme::parse(
        r##"
        {
            styles: {
                "tool.mode": { fg: "yellow" },
                "watching.name": { fg: "dark_yellow" },
                "tool.status.success": { fg: "green" },
                "tool.status.error": { fg: "red" },
                "status.agents": { fg: "cyan" },
                "diff.added": { fg: "dark_green" },
                "diff.removed": { fg: "dark_red" },
                "diff.added.inline": { fg: "green", bold: true },
                "diff.removed.inline": { fg: "red", bold: true },
                "action.label": { fg: "dark_grey" },
                "action.id": { fg: "yellow", bold: true },
                "action.error": { fg: "red" },
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

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("valid test agent id")
}
use super::tool_render::{
    CompactionStatus, ToolStatus, build_delegate_completion_display, cache_hit_percent,
    format_turn_stats_line, render_action_error_block, render_action_output_block,
    render_compaction_block, render_diff_tool_block, render_multi_diff_tool_block,
    render_shell_block, render_tool_block, render_tool_use_state, render_turn_stats_block,
    streaming_block, synthesize_fallback_display,
};

#[test]
fn dev_print_prompt_uses_shared_role_flag() {
    // Diagnostics share the same harness-selection args as normal `tau`, so a
    // role can be supplied before or after the hidden dev subcommand.
    let cli = super::cli::Cli::parse_from(["tau", "dev", "print-prompt", "--role", "engineer"]);
    assert_eq!(cli.harness.role.as_deref(), Some("engineer"));
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintPrompt {
                enable_agents_md: true
            },
        })
    ));
}

/// Ensures the root run command accepts `--ephemeral` as an explicit
/// session-persistence mode without affecting the separate attach/resume flags.
#[test]
fn run_parses_ephemeral_flag() {
    let cli = super::cli::Cli::parse_from(["tau", "--ephemeral"]);
    assert!(cli.run.ephemeral);
    assert!(!cli.run.attach);
    assert!(cli.run.resume.is_none());
    assert!(cli.command.is_none());
}

/// Prevents `--ephemeral` from becoming a misleading modifier for an already
/// running or persisted session, where the new process cannot guarantee a clean
/// session-persistence boundary.
#[test]
fn run_rejects_ephemeral_with_attach_or_resume() {
    assert!(super::reject_ephemeral_incompatible(true, true, None).is_err());
    assert!(super::reject_ephemeral_incompatible(true, false, Some("")).is_err());
    assert!(super::reject_ephemeral_incompatible(true, false, Some("s1")).is_err());
    assert!(super::reject_ephemeral_incompatible(true, false, None).is_ok());
}

#[test]
fn dev_print_prompt_accepts_agents_md_toggle() {
    let cli =
        super::cli::Cli::parse_from(["tau", "dev", "print-prompt", "--enable-agents-md", "false"]);
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintPrompt {
                enable_agents_md: false,
            },
        })
    ));
}

#[test]
fn dev_print_system_prompt_uses_shared_role_flag() {
    let cli =
        super::cli::Cli::parse_from(["tau", "--role", "engineer", "dev", "print-system-prompt"]);
    assert_eq!(cli.harness.role.as_deref(), Some("engineer"));
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintSystemPrompt,
        })
    ));
}
#[test]
fn dev_print_tools_uses_shared_role_flag() {
    // `print-tools` mirrors print-prompt and uses the same global role flag.
    let cli = super::cli::Cli::parse_from(["tau", "--role", "engineer", "dev", "print-tools"]);
    assert_eq!(cli.harness.role.as_deref(), Some("engineer"));
    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintTools,
        })
    ));
}

/// Ensures the hidden tmux helper parses isolated scratch/workdir startup
/// options, because future manual E2E sessions must not accidentally inherit
/// the user's real Tau state.
#[test]
fn dev_tmux_start_parses_isolated_startup_options() {
    let cli = super::cli::Cli::parse_from([
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
    let generated = super::cli::Cli::parse_from(["tau", "dev", "tmux", "start"]);
    assert!(matches!(
        generated.command,
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::Tmux {
                command: super::cli::DevTmuxCommand::Start(args),
            },
        }) if args.common.scratch_root.is_none()
    ));

    let explicit =
        super::cli::Cli::parse_from(["tau", "dev", "tmux", "start", "--root", "/tmp/tau-e2e-test"]);
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
    let cli = super::cli::Cli::parse_from([
        "tau",
        "dev",
        "tmux",
        "send",
        "--scratch-root",
        "/tmp/tau-e2e-test",
        "--no-enter",
        "--",
        "/help",
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
            && args.text == vec!["/help".to_owned(), "with spaces".to_owned()]
    ));
}

#[test]
fn component_command_parses_harness() {
    let cli = super::cli::Cli::parse_from(["tau", "component", "harness"]);

    assert!(matches!(
        cli.command,
        Some(super::cli::Command::Component {
            name,
            initial_ui_stdio: false,
        }) if name == "harness"
    ));
}

#[test]
fn ext_command_is_not_a_component_alias() {
    let err = match super::cli::Cli::try_parse_from(["tau", "ext", "harness"]) {
        Ok(_) => panic!("ext should not remain a supported component alias"),
        Err(err) => err,
    };

    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
}

#[test]
fn startup_role_flag_is_parsed_for_default_run() {
    let cli = super::cli::Cli::parse_from(["tau", "--role", "manager"]);

    assert_eq!(cli.harness.role.as_deref(), Some("manager"));
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
    let cli = super::cli::Cli::parse_from(["tau", "--role", "manager", "--prompt-stdin"]);

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

    let err = super::reject_harness_config_overrides(&overrides, "--attach")
        .expect_err("attach cannot apply overrides");
    assert!(err.to_string().contains("starting a new harness instance"));
}

/// The legacy `--config` flag must not be silently ignored because that makes
/// harness startup appear to use a config file that was never loaded.
#[test]
fn legacy_config_path_is_rejected() {
    let cli = super::cli::Cli::parse_from(["tau", "--config", "legacy.json"]);
    let err = super::reject_legacy_config_path(cli.run.config.as_deref())
        .expect_err("legacy config path should fail");

    assert!(err.to_string().contains("--config is no longer supported"));

    let non_run_cli =
        super::cli::Cli::parse_from(["tau", "--config", "legacy.json", "session-list"]);
    let non_run_err = super::reject_legacy_config_path(non_run_cli.run.config.as_deref())
        .expect_err("legacy config path should fail before non-run dispatch");
    assert!(
        non_run_err
            .to_string()
            .contains("--config is no longer supported")
    );

    let explicit_run_cli = super::cli::Cli::parse_from(["tau", "run", "--config", "legacy.json"]);
    let Some(super::cli::Command::Run(explicit_run)) = explicit_run_cli.command else {
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

/// Attach mode connects to an existing daemon, so startup role/extension
/// overrides must fail instead of pretending to reconfigure that daemon.
#[test]
fn attach_rejects_startup_overrides_that_existing_daemon_cannot_apply() {
    let role_overrides = [tau_config::settings::RoleCliOverride::Enable(
        "manager".to_owned(),
    )];
    let extension_overrides = [tau_config::settings::ExtensionCliOverride::Disable(
        "core-shell".to_owned(),
    )];

    let role_err = super::reject_attach_startup_overrides(false, Some("manager"), &[], &[])
        .expect_err("interactive attach role should fail");
    assert!(role_err.to_string().contains("cannot apply --role"));

    let role_override_err =
        super::reject_attach_startup_overrides(false, None, &role_overrides, &[])
            .expect_err("attach role overrides should fail");
    assert!(
        role_override_err
            .to_string()
            .contains("role enable/disable")
    );

    let extension_override_err =
        super::reject_attach_startup_overrides(false, None, &[], &extension_overrides)
            .expect_err("attach extension overrides should fail");
    assert!(
        extension_override_err
            .to_string()
            .contains("extension enable/disable")
    );

    super::reject_attach_startup_overrides(true, Some("manager"), &[], &[])
        .expect("prompt-stdin uses --role for the submitted prompt");
}

#[test]
fn harness_config_flag_requires_key_value() {
    let err = match super::cli::Cli::try_parse_from(["tau", "--harness-config=missing-equals"]) {
        Ok(_) => panic!("missing KEY=VALUE must fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("expected KEY=VALUE"));
}

#[test]
fn global_harness_flags_parse_before_dev_print_prompt() {
    // Hidden diagnostic commands use the same global harness args as normal
    // startup, including flags placed before the `dev` subcommand.
    let cli = super::cli::Cli::parse_from([
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
    let cli = super::cli::Cli::parse_from([
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
    let cli = super::cli::Cli::parse_from([
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
    super::cli::Cli::command()
        .write_long_help(&mut output)
        .expect("render long help");
    let help = String::from_utf8(output).expect("help is UTF-8");
    assert!(help.contains("TAU_ENABLE_EXTENSIONS=NAME[,NAME...]"));
    assert!(help.contains("CLI enable/disable flags win"));
}

/// Ensures attaching rejects a nonempty public startup override while an empty
/// environment remains a no-op.
#[test]
fn attach_rejects_public_extension_environment() {
    super::reject_attach_extension_environment(&[]).expect("empty environment is allowed");
    let error = super::reject_attach_extension_environment(&["std-pim".to_owned()])
        .expect_err("nonempty environment must be rejected");
    assert!(error.to_string().contains("TAU_ENABLE_EXTENSIONS"));
}

/// Proves the outer `tau dev tmux` dispatcher refuses startup overrides that
/// would require normal harness configuration validation before the helper has
/// switched into its scratch HOME/XDG environment.
#[test]
fn dev_tmux_rejects_startup_overrides_before_harness_validation() {
    let role_error = super::reject_dev_tmux_startup_overrides(Some("manager"), &[], &[], &[])
        .expect_err("--role refused");
    assert!(role_error.to_string().contains("cannot use --role"));

    let extension_error = super::reject_dev_tmux_startup_overrides(
        None,
        &[],
        &[tau_config::settings::ExtensionCliOverride::DisableAll],
        &[],
    )
    .expect_err("extension override refused");
    assert!(
        extension_error
            .to_string()
            .contains("cannot use extension enable/disable overrides")
    );
}

/// Ensures `/prompt <id>` resolves a configured template to editable prompt
/// text rather than submitting it immediately.
#[test]
fn custom_prompt_command_returns_configured_prompt_text() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let replacement = custom_prompt_replacement("/prompt review", &prompts)
        .expect("prompt command")
        .expect("known prompt");

    assert_eq!(replacement, "Review this patch carefully");
}

/// Ensures unknown `/prompt` ids produce a clear local error and list
/// configured ids so users can recover without accidentally submitting the
/// command text.
#[test]
fn custom_prompt_command_reports_unknown_id() {
    let prompts = vec![tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    }];

    let error = custom_prompt_replacement("/prompt missing", &prompts)
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

/// Ensures `/prompt` remains a local slash command for command echo/history
/// routing and does not fall through as a normal user prompt.
#[test]
fn prompt_command_is_local_slash_command() {
    assert!(is_local_slash_command("/prompt review"));
}

/// Protects the final slash-command ownership fallback recorded by
/// `DESIGN-tau-cli-slash-command-ownership`: a likely mistyped leading command
/// must not become a normal prompt, while non-leading slashes remain prompt
/// text.
#[test]
fn leading_slash_actions_are_identified_before_prompt_submission() {
    assert_eq!(leading_slash_action("/typo"), Some("/typo"));
    assert_eq!(leading_slash_action("  /typo arg"), Some("/typo"));
    assert_eq!(
        leading_slash_action("/skill:jujutsu args"),
        Some("/skill:jujutsu")
    );
    assert_eq!(leading_slash_action("hello /typo"), None);
    assert_eq!(leading_slash_action("please inspect /tmp/file"), None);
    assert_eq!(leading_slash_action("./relative/path"), None);
}

#[test]
fn local_slash_commands_are_identified_for_history_rendering() {
    assert!(is_local_slash_command("/model engineer"));
    assert!(is_local_slash_command("/set show-tools compact"));
    assert!(is_local_slash_command("/theme dpc"));
    assert!(is_local_slash_command("/debug-show-ui-event-stats"));
    assert!(is_local_slash_command("/debug-show-event-stats std-shell"));
    assert!(is_local_slash_command("/quit"));
    assert!(is_local_slash_command("/agent"));
    assert!(is_local_slash_command("/agent switch worker-1"));
    assert!(is_local_slash_command("/agent suspend"));
    assert!(is_local_slash_command("/agent resume worker-1"));
    assert!(is_local_slash_command("/agent new"));
    assert!(is_local_slash_command("/new"));
    assert!(is_local_slash_command("/name Current worker"));
    assert!(is_local_slash_command("/suspend"));
    assert!(is_local_slash_command("/resume"));
    assert!(is_local_slash_command("/new now"));
    assert!(is_local_slash_command("/session new"));
    assert!(is_local_slash_command("/version"));
    assert!(is_local_slash_command("/version now"));
    assert!(is_local_slash_command("/skill jujutsu"));
    assert!(is_local_slash_command("/skill:jujutsu args"));
    assert!(!is_local_slash_command("/skillx jujutsu"));
    assert!(!is_local_slash_command("hello /model engineer"));
}

#[test]
fn gmail_oauth_finish_redirect_url_is_redacted_from_echo_and_prompt_history() {
    let line = "/email auth google finish work http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    let redacted = "/email auth google finish <redacted>";
    assert_eq!(redacted_command_echo_line(line), redacted);
    assert_eq!(redacted_prompt_history_line(line, line), redacted);
    assert!(!redacted_command_echo_line(line).contains("auth-code-secret"));
    let missing_account = "/email auth google finish http://127.0.0.1:54321/?state=state-secret&code=auth-code-secret";
    assert_eq!(redacted_command_echo_line(missing_account), redacted);
    assert!(
        !redacted_prompt_history_line(missing_account, missing_account)
            .contains("auth-code-secret")
    );
    assert_eq!(
        redacted_command_echo_line("/email auth google start work"),
        "/email auth google start work"
    );
}

#[test]
fn runtime_version_label_matches_cli_version_shape() {
    // `/version` uses this same label at runtime, so keep it aligned with the
    // custom `tau --version` output instead of clap's default package version.
    let label = super::version_label();
    assert!(label.starts_with(concat!("tau ", env!("CARGO_PKG_VERSION"), " (")));
    assert!(label.ends_with(')'));
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
            frames: Arc::new((Mutex::new(Vec::new()), std::sync::Condvar::new())),
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

fn sync(handle: &TermHandle) {
    handle.redraw_sync();
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: format!("msg-{sender_id}-{recipient}").into(),
        sender_id: agent_id(sender_id),
        recipient: if recipient == "user" {
            tau_proto::AgentMessageRecipient::User
        } else {
            tau_proto::AgentMessageRecipient::Agent {
                agent_id: agent_id(recipient),
            }
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
        message_id: format!("msg-{sender_id}-{session_id}-{recipient}").into(),
        sender_id: agent_id(sender_id),
        recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
            session_id: session_id.into(),
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
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: session_id.into(),
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
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: session_id.into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }
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
        agent_prompt_id: agent_prompt_id.into(),
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

/// Increasing `/set redraw-history-size` should restore more scrollback
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
    // later `/session new` returns to the empty start-new-agent state.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
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
    assert!(vt.screen_contains(80, "&s1 @engineer_abc12345"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
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
        provider_response_delta_update("sp-0", "hello", None, tau_proto::PromptOriginator::User),
    ));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update("sp-0", " world", None, tau_proto::PromptOriginator::User),
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
fn initial_session_started_renders_session_status_without_role_placeholder() {
    // Regression: startup may announce SessionStarted before role selection.
    // The status bar must still show the human-readable session id, without
    // adding a misleading no-role placeholder next to it.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "tau-agent-test".into(),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "&tau-agent-test"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

#[test]
fn extension_prompt_with_target_does_not_select_from_empty_state() {
    // Regression: extension side prompts now carry target_agent_id for routing,
    // but `/agent none`/startup must stay on the no-agent screen until the user
    // explicitly selects a transcript.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: "core-subagents".into(),
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
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
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
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        message_class: tau_proto::PromptMessageClass::Internal,
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
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        message_class: tau_proto::PromptMessageClass::Internal,
        ctx_id: Some("timer:wake:2".to_owned()),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "Timer `wake` woke this agent: stand up"));
    assert!(!vt.screen_contains(100, "woke this agent: Timer `wake` fired"));
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

#[test]
fn agent_switching_cycles_active_agents_and_skips_suspended() {
    // Ctrl-K/Ctrl-J should only target active agents. Suspended agents remain
    // known for completion/resume, but switching to them would leave the prompt
    // pointed at an agent that immediately refuses user prompts.
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned(), "charlie".to_owned()];
    let active_agents = HashSet::from(["alpha".to_owned(), "charlie".to_owned()]);

    assert_eq!(
        next_active_agent(Some("alpha"), &known_agents, &active_agents, 1).as_deref(),
        Some("charlie")
    );
    assert_eq!(
        next_active_agent(Some("alpha"), &known_agents, &active_agents, -1).as_deref(),
        Some("charlie")
    );
}

#[test]
fn agent_switching_without_selection_starts_at_edge_for_direction() {
    // When the user is at the no-agent prompt, the first switch should enter
    // the active-agent ring from the side implied by the shortcut direction.
    let known_agents = vec!["alpha".to_owned(), "bravo".to_owned()];
    let active_agents = HashSet::from(["alpha".to_owned(), "bravo".to_owned()]);

    assert_eq!(
        next_active_agent(None, &known_agents, &active_agents, 1).as_deref(),
        Some("alpha")
    );
    assert_eq!(
        next_active_agent(None, &known_agents, &active_agents, -1).as_deref(),
        Some("bravo")
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
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items,
        stop_reason,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
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
        provider_response_delta_update("sp-0", "Hel", None, tau_proto::PromptOriginator::User),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update("sp-0", "lo", None, tau_proto::PromptOriginator::User),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "HelHel"));
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
                "sp-md",
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
        provider_response_delta_update("sp-md", "\n", None, tau_proto::PromptOriginator::User),
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
        provider_response_delta_update("sp-late", "world", None, tau_proto::PromptOriginator::User),
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
            "sp-observed",
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
            "sp-0",
            "",
            Some("failed attempt thinking".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed attempt thinking"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: "sp-0".into(),
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
        agent_id: agent_id(agent_id_value),
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens,
            prompt_cached_tokens,
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
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
    // `/session new` intentionally moves to the start-new-agent screen and clears
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
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "first".into(),
        agent_id: tau_proto::AgentId::parse("engineer_one").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s2".into(),
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
    // `/session new` can be reached after an explicit no-agent state, but the new
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
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 88.into(),
        extension_name: "std-session".into(),
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
        extension_name: "std-session".into(),
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
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    renderer.clear_selected_agent();
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 89.into(),
        extension_name: "std-race".into(),
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
        session_id: "s1".into(),
        text: "worker one transcript".into(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.switch_agent("worker-2".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
            session_id: "s1".into(),
            text: "worker one transcript".into(),
            agent_id: agent_id("worker-1"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
        renderer.switch_agent("worker-2".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
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
            session_id: "s1".into(),
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

#[test]
fn switching_agents_preserves_turn_stats_cache_hit_baseline() {
    // Regression: switching away and back re-renders turn-stats blocks, so the
    // second response must keep the previous same-agent response as its cache-hit
    // denominator instead of falling back to the no-baseline `Δ0% .../0` display.
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

    assert!(vt.screen_contains(80, "Δ95% 19k/20k"));
    assert!(!vt.screen_contains(80, "Δ0% 19k/0"));
}

#[test]
fn switching_to_hidden_agent_preserves_turn_stats_cache_hit_baseline() {
    // Regression: hidden side-agent responses are recorded in that agent's UI
    // state and later replayed by a full transcript re-render when selected, so
    // they must also retain their per-entry cache-hit baseline.
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

    assert!(vt.screen_contains(80, "Δ95% 19k/20k"));
    assert!(!vt.screen_contains(80, "Δ0% 19k/0"));
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
            session_id: "s1".into(),
            agent_id: agent_id("worker-1"),
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(80, "context ready"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "agent @worker-1 context ready"));
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
        extension_name: "std-test".into(),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-test starting"));

    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 7.into(),
        extension_name: "std-test".into(),
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
        extension_name: "std-global".into(),
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
        extension_name: "std-global".into(),
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
        extension_name: "std-global".into(),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-global starting"));

    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension std-global starting"));

    renderer.handle(&Event::ExtensionExited(tau_proto::ExtensionExited {
        instance_id: 8.into(),
        extension_name: "std-global".into(),
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
    renderer.record_action_invocation("action-1".into(), Some("agent-a".to_owned()));
    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: "action-1".into(),
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

    renderer.record_action_invocation("action-2".into(), None);
    renderer.switch_agent("fresh-agent".to_owned());
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: "action-2".into(),
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
    renderer.record_action_invocation("action-2".into(), None);
    renderer.switch_agent("fresh-agent".to_owned());
    renderer.handle(&Event::ActionError(tau_proto::ActionError {
        invocation_id: "action-2".into(),
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

    renderer.record_action_invocation("action-3".into(), None);
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: "action-3".into(),
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
    renderer.record_action_invocation("action-3".into(), None);
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: "action-3".into(),
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
        extension_name: "std-filtered".into(),
        pid: Some(789),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-filtered starting"));

    renderer.apply_setting("notice-level", "warning");
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 9.into(),
        extension_name: "std-filtered".into(),
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
        session_id: "s1".into(),
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
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    sync(&handle);
    assert_eq!(handle.full_render_count(), full_render_count);
}

#[test]
fn agent_stats_does_not_overwrite_display_name() {
    // `/agent switch` completions are backed by durable display names. Agent stats
    // must not replace the display name chosen by the harness template.
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        parent_agent: None,
        agent_id: agent_id("engineer-Ab12"),
        role: "engineer".to_owned(),
        display_name: Some("engineer: look it up".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer-Ab12"),
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
    }));

    let display_names = renderer.agent_display_names();
    let display_names = display_names.lock().expect("display names");
    assert_eq!(
        display_names.get("engineer-Ab12").map(String::as_str),
        Some("engineer: look it up")
    );
}

/// Ensures accepted input and stale terminal events preserve a delegated
/// agent's mode.
#[test]
fn accepted_input_and_terminal_events_preserve_active_auto() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("worker-1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: Default::default(),
        context: Default::default(),
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("worker-1"),
        text: "follow up".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
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
        session_id: "s1".into(),
        agent_id: agent_id("worker-1"),
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        tools: Default::default(),
        context: Default::default(),
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(
        navigation.mode("worker-1"),
        AgentNavigationState::ActiveAuto
    );
    assert!(!navigation.is_active("worker-1"));
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
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "active-auto agent is idle"));
    // Exercise the placeholder-only navigation path: the operation must request
    // its own redraw even when no model-status block is present to do so.
    renderer.clear_model_status_for_test();
    let generation = vt.frame_generation();
    renderer.suspend_agent("worker-1");
    let frame = vt.wait_for_frame_after(generation);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("This agent is suspended"))
    );
    let generation = vt.frame_generation();
    renderer.resume_agent("worker-1".to_owned());
    let frame = vt.wait_for_frame_after(generation);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("Write a message to worker-1"))
    );
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
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    {
        let navigation = renderer.agent_navigation();
        let navigation = navigation.lock().expect("agent navigation");
        assert_eq!(
            navigation.mode("worker-1"),
            AgentNavigationState::ActiveAuto
        );
        assert!(!navigation.is_active("worker-1"));
    }
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("worker-1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: Default::default(),
        context: Default::default(),
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

/// Ensures durable extension prompt provenance reconstructs the delegated
/// default without overwriting a local explicit resume.
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
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q-worker".to_owned(),
        },
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    };
    renderer.handle(&Event::AgentPromptSubmitted(prompt.clone()));
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .mode("worker-1"),
        AgentNavigationState::ActiveAuto,
    );
    renderer.resume_agent("worker-1".to_owned());
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
    renderer.resume_agent("worker-1".to_owned());
    renderer.handle(&Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: "s1".into(),
            agent_id: agent_id("worker-1"),
        },
    ));

    renderer.refresh_agent_navigation("worker-1");
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
    assert_eq!(
        navigation.mode("worker-1"),
        AgentNavigationState::ActiveAuto
    );
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
        session_id: "s1".into(),
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
        session_id: "s2".into(),
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
            name: "core-subagents".into(),
            query_id: "q-worker".to_owned(),
        },
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    let originator = tau_proto::PromptOriginator::Extension {
        name: "core-subagents".into(),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
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

#[test]
fn watched_agent_stats_route_to_hidden_watcher_owner() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("worker-1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_started("ap-engineer_1-0", "s1")
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 1,
            started_total: 2,
        },
        context: tau_proto::AgentContextStats::default(),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(90, "watching [engineer_1]"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(90, "watching [engineer_1] @engineer_1"));
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "s1".into(),
        command_id: "ui-sh-1".into(),
        command: "printf worker-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.switch_agent("main".to_owned());

    renderer.handle(&Event::ShellCommandProgress(
        tau_proto::ShellCommandProgress {
            command_id: "ui-sh-1".into(),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "worker-output".into(),
            target_agent_id: Some(agent_id("worker-1")),
        },
    ));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: "ui-sh-1".into(),
            session_id: "s1".into(),
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

#[test]
fn shell_command_target_field_survives_switch_before_echo_and_replay() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent("main".to_owned());

    // Regression: the durable event's target must own the command even if the
    // selected transcript is main by the time the renderer processes the echo.
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "s1".into(),
        command_id: "ui-sh-race".into(),
        command: "printf race-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: "ui-sh-race".into(),
            session_id: "s1".into(),
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    replay.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: "ui-sh-replay".into(),
            session_id: "s1".into(),
            command: "printf replay-output".into(),
            include_in_context: false,
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: "core-subagents".into(),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptSubmitted(
        tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("worker-1"),
            text: "side task".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: originator.clone(),
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: "core-subagents".into(),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
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
    // Regression: `/agent none` must restore an empty no-agent screen. The
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
        session_id: "s1".into(),
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
    // Regression: after `/agent new`, an already-running agent can still emit
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
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
        session_id: "s1".into(),
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

#[test]
fn old_agent_message_does_not_leak_into_new_agent_screen() {
    // Regression: `/new` leaves the old agent running while the terminal shows
    // an empty new-agent creation screen. Agent-to-agent messages emitted by the
    // old agent during that window must update the old hidden transcript instead
    // of making a message block suddenly appear in the empty screen while the
    // user is typing the first prompt for the new agent.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
    assert!(!vt.screen_contains(80, "Message from old-agent to other-agent"));
    assert!(!vt.screen_contains(80, "hidden old-agent message"));
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

#[test]
fn stale_draft_snapshot_is_dropped_after_submit_epoch_bump() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: "s1".into(),
                target_agent_id: None,
                text: "old".into(),
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
/// otherwise `/role <role> effort off` and `/role <role> thinking-summary off`
/// would accidentally reset the selected role instead of storing the user's
/// requested off state. `reset` is the only textual way to clear a setting.
#[test]
fn role_setting_updates_are_typed_and_reset_aware() {
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
            "verbosity",
            "high",
            UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
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

#[test]
fn action_submission_invalidates_pending_draft_like_prompt_submission() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: "s1".into(),
                target_agent_id: None,
                text: "/email list".into(),
            },
        ));
    }

    invalidate_pending_draft(&handle);

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    assert_eq!(slot.epoch, 1);
    assert!(slot.pending.is_none());
}

/// A queued draft snapshot must carry the selected viewed agent so later
/// consumers can distinguish an existing-agent draft from a start-new-agent
/// draft without consulting mutable UI selection state.
#[test]
fn queued_draft_snapshot_records_selected_agent_target() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    let agent_id = tau_proto::AgentId::parse("agent-a").expect("agent id");

    queue_prompt_draft_snapshot(
        &handle,
        "s1".into(),
        Some(agent_id.clone()),
        "draft for agent".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, tau_proto::SessionId::from("s1"));
    assert_eq!(draft.target_agent_id, Some(agent_id));
    assert_eq!(draft.text, "draft for agent");
}

/// A queued start-new-agent draft must remain explicitly unscoped instead of
/// inheriting whatever agent might become current before the debounce fires.
#[test]
fn queued_draft_snapshot_records_no_agent_target() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());

    queue_prompt_draft_snapshot(&handle, "s1".into(), None, "new agent draft".to_owned());

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("pending draft");
    assert_eq!(*epoch, 0);
    assert_eq!(draft.session_id, tau_proto::SessionId::from("s1"));
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text, "new agent draft");
}

/// Switching from one viewed agent to another before the debounce fires must
/// invalidate the stale snapshot and queue the current buffer under the new
/// target.
#[test]
fn retarget_draft_snapshot_replaces_agent_a_with_agent_b() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    let agent_b = tau_proto::AgentId::parse("agent-b").expect("agent id");
    queue_prompt_draft_snapshot(&handle, "s1".into(), Some(agent_a), "draft".to_owned());

    retarget_prompt_draft_snapshot(
        &handle,
        "s1".into(),
        Some(agent_b.clone()),
        "draft".to_owned(),
    );

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, Some(agent_b));
    assert_eq!(draft.text, "draft");
}

/// Switching from a viewed agent back to the start-new-agent prompt before the
/// debounce fires must make the replacement snapshot unscoped.
#[test]
fn retarget_draft_snapshot_replaces_agent_with_no_agent() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    let agent_a = tau_proto::AgentId::parse("agent-a").expect("agent id");
    queue_prompt_draft_snapshot(&handle, "s1".into(), Some(agent_a), "draft".to_owned());

    retarget_prompt_draft_snapshot(&handle, "s1".into(), None, "draft".to_owned());

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(draft.target_agent_id, None);
    assert_eq!(draft.text, "draft");
}

#[test]
fn current_draft_snapshot_is_sent_when_epoch_matches() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());

    assert!(should_send_draft_snapshot(&handle, 0));
}

#[test]
fn draft_snapshot_is_dropped_after_shutdown() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        super::locked(mtx).done = true;
    }

    assert!(!should_send_draft_snapshot(&handle, 0));
}

/// `AgentMessage` events are normal history entries, not active blocks. They
/// must render for every sender/recipient pair and scroll away as history
/// grows.
#[test]
fn agent_messages_render_all_recipients_as_history() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&agent_message(
        "manager_11111111",
        "engineer_22222222",
        "hello worker",
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from manager_11111111 to engineer_22222222:"));
    assert!(vt.screen_contains(80, "hello worker"));

    for idx in 0..20 {
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: format!("scroll filler {idx}"),
            agent_id: agent_id("engineer_22222222"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    }
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from manager_11111111 to engineer_22222222:"));
}

#[test]
fn external_agent_messages_render_session_agent_labels() {
    let (_term, handle, vt) = setup(96, 8);
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
            message_id: "msg-inbound-external".into(),
            sender_id: agent_id("reviewer_33333333"),
            sender_session_id: Some("session-3".into()),
            recipient_id: agent_id("manager_11111111"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            watch_provider_status: None,
            message: "hello back".to_owned(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(
        96,
        "Message from manager_11111111 to session-2/engineer_22222222:"
    ));
    assert!(vt.screen_contains(
        96,
        "Message from session-3/reviewer_33333333 to manager_11111111:"
    ));
}

/// Watched-turn lifecycle records are harness-authored status events, not
/// messages authored by the watched agent, and must stay compact in the UI.
#[test]
fn watched_turn_transition_renders_as_compact_status() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: "msg-watch-turn-start".into(),
            sender_id: agent_id("researcher"),
            sender_session_id: None,
            recipient_id: agent_id("manager"),
            kind: tau_proto::AgentMessageKind::WatchTurnState,
            watch_turn_state: Some(tau_proto::AgentWatchTurnStateNotification {
                session_id: "session-1".into(),
                subscription_id: "subscription-1".to_owned(),
                state: tau_proto::AgentRuntimeState::Running,
                initial: false,
                turn_generation: 1,
            }),
            watch_provider_status: None,
            message: "[tau-internal]: compatibility presentation".to_owned(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "researcher · turn started"));
    assert!(!vt.screen_contains(80, "Message from researcher"));
    assert!(!vt.screen_contains(80, "compatibility presentation"));

    for setting in ["none", "all-full", "none"] {
        renderer.apply_setting("show-messages", setting);
        sync(&handle);
        assert!(vt.screen_contains(80, "researcher · turn started"));
        assert!(!vt.screen_contains(80, "Message from researcher"));
        assert!(!vt.screen_contains(80, "compatibility presentation"));
    }
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
fn user_recipient_agent_messages_broadcast_to_visible_agent_even_when_hidden() {
    // Messages sent to `recipient_id: "user"` are intended for the human, not
    // just the sender's private transcript. They must render in the visible UI
    // even when another agent is selected and `show-messages` hides normal
    // agent-to-agent messages.
    let (_term, handle, vt) = setup(80, 10);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-visible".to_owned(),
        agent_id: agent_id("visible-agent"),
    }));
    renderer.switch_agent("visible-agent".to_owned());
    renderer.apply_setting("show-messages", "none");
    renderer.handle(&agent_message(
        "sender-agent",
        "user",
        "broadcast body for all visible agents",
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Message from sender-agent to user:"));
    assert!(vt.screen_contains(80, "broadcast body for all visible agents"));
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

    assert!(vt.screen_contains(80, "Message from agent-a to agent-b"));
    assert!(!vt.screen_contains(80, "secret summarized body"));
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
    assert!(!vt.screen_contains(80, "Message from agent-a to agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "all-full");
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from agent-a to agent-b:"));
    assert!(vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "none");
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from agent-a to agent-b"));
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
        session_id: "s1".into(),
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
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "old prompt"));
    assert!(!vt.screen_contains(80, "old response"));
    assert!(!vt.screen_contains(80, "read src/lib.rs"));
    assert!(vt.screen_contains(80, "&s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

#[test]
fn new_session_replays_startup_context_and_kept_extensions() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: std::path::PathBuf::from("/tmp/AGENTS.md"),
        content: "# Test\n".into(),
    }));
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: "core-shell".into(),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "tau"));
    assert!(vt.screen_contains(80, "extension core-shell kept"));
}
/// `notice-level=warning` hides routine informational chatter while mandatory
/// warnings such as configuration errors still reach the UI.
#[test]
fn warning_notice_level_hides_info_but_keeps_always_show_warning() {
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
        always_show: false,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "routine lifecycle note"));

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.warning".into(),
        message: "important config error".into(),
        level: tau_proto::NoticeLevel::Warning,
        always_show: true,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "important config error"));
}

#[test]
fn critical_notice_level_keeps_always_show_harness_failure() {
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
        always_show: true,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed to dispatch queued prompt: boom"));
}

/// Extension ready/kept messages are informational lifecycle notices, so a
/// warning threshold should keep them out of live startup and `/session new`
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
        extension_name: "core-shell".into(),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "extension core-shell"));
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
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(vt.screen_contains(80, "&s2"));
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
        session_id: "tau-agent-test".into(),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
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
    assert!(status_row.starts_with("&tau-agent-test +engineer ~high"));
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!vt.screen_contains(80, "=test/model"));
    assert!(!vt.screen_contains(80, "v=high"));
    assert!(!vt.screen_contains(80, "ctx:"));
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
        session_id: "s1".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    // In the no-agent/start-new-agent state, the status bar mirrors the prompt
    // placeholder by showing the selected role immediately after the session.
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("&s1"))
        .expect("status row before agent selection");
    assert!(status_row.starts_with("&s1 +engineer"));
    assert!(!status_row.contains("@engineer_abc"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
        .find(|row| row.contains("&s1"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("&s1 @engineer_abc"));
    assert!(!status_row.contains("+engineer"));

    renderer.clear_selected_agent();
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("&s1"))
        .expect("status row after clearing agent selection");
    assert!(status_row.starts_with("&s1 +engineer"));
    assert!(!status_row.contains("@engineer_abc"));
}

#[test]
fn status_agent_chip_keeps_id_primary_and_display_name_secondary() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        parent_agent: None,
        agent_id: agent_id("engineer-junior_b"),
        role: "engineer-junior".to_owned(),
        display_name: Some("sleep 6".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
        .find(|row| row.contains("&s1"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("&s1 @engineer-junior_b (sleep 6)"));
    assert!(!status_row.contains("@sleep 6 (engineer-junior_b)"));
}

#[test]
fn status_agent_chip_shows_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        parent_agent: None,
        agent_id: agent_id("engineer_child"),
        role: "engineer".to_owned(),
        display_name: Some("fix streaming ellipsis".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer_child").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
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
        .find(|row| row.contains("&s1"))
        .expect("status row after watch update");
    assert!(status_row.contains("@engineer_child (fix streaming ellipsis)"));
    assert!(status_row.contains("watched by: manager-AjhD"));
    assert!(!status_row.contains("child of"));
}

/// Covers the watcher-derived display contract in
/// `DESIGN-tau-cli-agent-watch-display`.
#[test]
fn status_agent_chip_truncates_multiple_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::New,
    }));
    renderer.switch_agent("engineer_child".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("manager-AjhD"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
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
        .find(|row| row.contains("&s1"))
        .expect("status row after watch updates");
    assert!(status_row.contains("watched by: manager-AjhD, +1 more agents"));
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
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains("#-/200k"));
}

#[test]
fn model_status_shows_main_tool_usage_before_context() {
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

    // Regression coverage for the bottom status bar: main-agent tool
    // usage should mirror generic tool progress chips (`%complete/total`)
    // and should render immediately before the context chip, while
    // side-conversation tool calls stay rolled up under their delegate.
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: "side-sp".into(),
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
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after side response");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
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
        status_row.ends_with("%0/2 #12k/200k"),
        "unexpected status row: {status_row:?}"
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "side-call".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("side result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },

        display: None,
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
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
    assert!(status_row.ends_with("%1/2 #12k/200k"));

    // Regression coverage for turn visibility: once an extension/sub-agent
    // prompt becomes active, it must not steal the main transcript's tool chip;
    // main progress stays visible while side-conversation tool calls remain
    // rolled up under their own delegate blocks.
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("q2").expect("agent id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
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
        status_row.ends_with("%1/2 @1 #12k/200k"),
        "unexpected status row: {status_row:?}"
    );
    assert!(status_row.contains('%'));

    renderer.handle(&Event::ToolResult(ToolResult {
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
    assert!(status_row.ends_with("%2/2 @1 #12k/200k"));
    assert!(status_row.contains('%'));

    // Main tool completions that arrive while a side conversation is active
    // update the visible main counters. The side conversation's own tool usage
    // remains hidden from the main status chip.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-follow-up-sp",
        "s1",
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after main prompt resumes");
    assert!(status_row.ends_with("%2/2 @1 #12k/200k"));

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
    assert!(status_row.ends_with("@1 #12k/200k"));
    assert!(!status_row.contains('%'));

    // Starting a new user task in the same session also keeps the chip hidden
    // until the main agent requests tools for that task.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
    assert!(status_row.ends_with("@1 #12k/200k"));
    assert!(!status_row.contains('%'));
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
        session_id: "s1".into(),
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
        agent_prompt_id: "sp-stale".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        reason: AgentPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!vt.screen_contains(80, "…"));
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
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));
    assert!(!vt.screen_contains(80, "shell_command"));
    assert!(!vt.screen_contains(80, "tool args"));
    assert!(!vt.screen_contains(80, "tools,"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: "sp-progress".into(),
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
        vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "updates without a fresh stats sample must not clear cached stats: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "sp-progress",
            tau_proto::AgentId::parse("main").expect("agent id"),
            12 * 1024,
            12 * 1024,
            3_000_000,
            2_000_000,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (3s, 12KB, Δ0B/s, 4KB/s)"),
        "idle stats samples must show elapsed time, zero interval rate, and total rate: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-progress",
        Vec::new(),
    )));
    sync(&handle);
    assert!(!vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));
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
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_b-0",
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
        "visible agent A stats should remain unchanged: {:?}",
        vt.screen_text(80)
    );
    assert!(
        !vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "hidden agent B stats must not render in agent A's view: {:?}",
        vt.screen_text(80)
    );

    renderer.switch_agent("agent_b".to_owned());
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
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
        agent_prompt_id: "ap-agent_a-0".into(),
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
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "done"));
    assert!(!vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));
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
        agent_prompt_id: "sp-visible-progress".into(),
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

#[test]
fn agent_in_progress_clears_when_tool_is_cancelled() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
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
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
    }));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
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
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
        ..agent_prompt_created("side-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            "side-sp",
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: "core-subagents".into(),
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
    assert!(status_row.ends_with("%0/1 @1 #12k/200k"));

    // Generic watched-agent stats no longer mutate the parent tool status chip.
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 2,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("#12k/200k"))
        .expect("status row after watched-agent stats");
    assert!(status_row.contains("@main"));
    assert!(status_row.ends_with("%0/1 @1 #12k/200k"));

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q1".to_owned(),
        text: String::new(),
        error: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
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
    assert!(status_row.ends_with("@1 #12k/200k"));
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
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "&s2 +engineer"));
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

    assert!(vt.screen_contains(80, "&s2 +engineer ~high"));
}

/// Role availability should feed `/new` argument completion as well as
/// `/role`, because `/new <role>` is the fast path for opening a fresh
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
        &[tau_cli_term::SlashCommand::new("/new", "new agent")],
        &completion_data,
        "/new rev",
        "/new rev".len(),
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].label, "reviewer");
    assert_eq!(candidates[0].replacement, "/new reviewer");
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
        session_id: "s3".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "&s3 +engineer ^low ~high !off"));
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
        session_id: "s1".into(),
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
            "sp-0",
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
        session_id: "s1".into(),
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
            "sp-0",
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
            "sp-0",
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
        session_id: "s1".into(),
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
            "sp-0",
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
        session_id: "s1".into(),
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
        session_id: "s1".into(),
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
        provider_response_delta_update("sp-0", "hello", None, tau_proto::PromptOriginator::User),
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
        session_id: "s1".into(),
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
            "sp-1",
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
        session_id: "s1".into(),
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
        inference_activation: false,
        text: "folded queued prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
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

#[test]
fn internal_prompt_events_are_hidden() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Background tool completion prompts are delivered to the model as
    // prompt-like events, but they are internal control text and must not show
    // up in the user's transcript or queued prompt area.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "[tau-internal] Tool call `bg` is complete.".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "[tau-internal] Tool call `queued` is complete.".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        inference_activation: false,
        text: "[tau-internal] Tool call `steered` is complete.".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::Internal,
        ctx_id: None,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "Tool call"));
    assert!(
        vt.screen_text(80)
            .iter()
            .all(|row| !row.contains("Tool call"))
    );
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
        session_id: "s1".into(),
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
                session_id: "s1".into(),
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
        let spid: tau_proto::AgentPromptId = format!("sp-{i}").into();
        if i > 0 {
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
        provider_response_delta_update("sp-0", "Hello", None, tau_proto::PromptOriginator::User),
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

#[test]
fn render_provider_compaction_update_as_compact_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: "sp-compact".into(),
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

#[test]
fn watched_agent_stats_redraw_active_indicator() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "watching [engineer_1]"));

    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: "ap-engineer_1-0".into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
    }));

    assert!(
        eventually_screen_contains(&vt, 100, "watching [engineer_1] @engineer_1"),
        "watched-agent stats should repaint without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    assert!(
        eventually_screen_contains(&vt, 100, "watching [engineer_1] @engineer_1 %3/3"),
        "watched-agent stats should repaint with tool-call-style counters without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    assert!(
        !vt.screen_contains(100, "running tools"),
        "watched-agent block should keep compact passive tool-block layout, not prose: {:?}",
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
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_b"), agent_id("engineer_a")],
            changed_agent_id: None,
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    for watched in ["engineer_b", "engineer_a"] {
        renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            session_id: "s1".into(),
            agent_id: agent_id(watched),
            agent_prompt_id: format!("ap-{watched}-0").into(),
            model: "test/model".parse().expect("model id"),
            originator: tau_proto::PromptOriginator::Extension {
                name: "__harness__".into(),
                query_id: format!("delegate-{watched}"),
            },
            ctx_id: None,
        }));
    }
    sync(&handle);

    let screen = vt.screen_text(100);
    let first = screen
        .iter()
        .position(|line| line.contains("watching [engineer_a] @engineer_a"))
        .expect("engineer_a watching row");
    let second = screen
        .iter()
        .position(|line| line.contains("watching [engineer_b] @engineer_b"))
        .expect("engineer_b watching row");
    assert!(
        first < second,
        "watched-agent rows should be sorted by agent id: {screen:?}"
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
fn watched_agent_indicator_does_not_duplicate_after_agent_switch() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: "ap-engineer_1-0".into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 13,
        },
        context: tau_proto::AgentContextStats::default(),
    }));
    sync(&handle);
    assert!(eventually_screen_contains(
        &vt,
        100,
        "watching [engineer_1] @engineer_1 %13/13",
    ));

    renderer.switch_agent("other_1".to_owned());
    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 42,
        },
        context: tau_proto::AgentContextStats::default(),
    }));
    sync(&handle);

    let watching_rows: Vec<_> = vt
        .screen_text(100)
        .into_iter()
        .filter(|row| row.contains("watching [engineer_1] @engineer_1"))
        .map(|row| row.trim_end().to_owned())
        .collect();
    assert_eq!(
        watching_rows,
        vec!["watching [engineer_1] @engineer_1 %42/42"],
        "watched-agent row should update in place after transcript restore: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures watched-agent status blocks follow provider prompt lifetime rather
/// than staying visible until a later `agent.stats_updated` idle snapshot.
///
/// The live session regression showed a watched agent with `%15/15` counters
/// remaining on screen after it had produced a provider response. Removing the
/// block on `provider.response_finished` prevents a missed or delayed idle stat
/// from leaving a stale watched-agent line behind.
#[test]
fn watched_agent_response_finished_removes_active_indicator() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: "ap-engineer_1-0".into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 15,
        },
        context: tau_proto::AgentContextStats::default(),
    }));

    assert!(eventually_screen_contains(
        &vt,
        100,
        "watching [engineer_1] @engineer_1 %15/15",
    ));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: "ap-engineer_1-0".into(),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        !vt.screen_contains(100, "watching [engineer_1]"),
        "watched-agent block should be removed when provider response finishes: {:?}",
        vt.screen_text(100)
    );
}

/// A watched agent turn spans every model round and intervening tool round, so
/// prompt-terminal events must not make its running row flicker.
#[test]
fn watched_agent_turn_state_keeps_indicator_across_model_rounds() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    let watch_state = |message_id: &str, state| {
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: message_id.into(),
            sender_id: agent_id("engineer_1"),
            sender_session_id: None,
            recipient_id: agent_id("parent_1"),
            kind: tau_proto::AgentMessageKind::WatchTurnState,
            watch_turn_state: Some(tau_proto::AgentWatchTurnStateNotification {
                session_id: "s1".into(),
                subscription_id: "watch-1".to_owned(),
                state,
                initial: false,
                turn_generation: 1,
            }),
            watch_provider_status: None,
            message: "compatibility text is not UI state".to_owned(),
        })
    };
    renderer.handle(&watch_state(
        "watch-running",
        tau_proto::AgentRuntimeState::Running,
    ));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: "ap-engineer_1-0".into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_id: agent_id("engineer_1"),
        stop_reason: ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ..finished_response("ap-engineer_1-0", Vec::new())
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(100, "watching [engineer_1] @engineer_1"),
        "the outer agent turn remains running while tools are pending"
    );

    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_started("ap-engineer_1-1", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ..finished_response("ap-engineer_1-1", Vec::new())
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(100, "watching [engineer_1] @engineer_1"),
        "the provider's final model round is not the agent-turn boundary"
    );

    renderer.handle(&watch_state(
        "watch-idle",
        tau_proto::AgentRuntimeState::Idle,
    ));
    sync(&handle);
    assert!(
        !vt.screen_contains(100, "watching [engineer_1]"),
        "the row ends only at the harness-authored agent-turn idle edge"
    );
}

/// Ensures provider-level prompt submission also starts watched-agent running
/// UI for backends or replay paths that do not emit an explicit
/// `agent.prompt_started` event before provider work begins.
#[test]
fn watched_agent_provider_prompt_submitted_starts_active_indicator() {
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
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderPromptSubmitted(
        tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: "ap-engineer_1-0".into(),
            originator: tau_proto::PromptOriginator::Extension {
                name: "__harness__".into(),
                query_id: "delegate-1".to_owned(),
            },
        },
    ));
    sync(&handle);

    assert!(eventually_screen_contains(
        &vt,
        100,
        "watching [engineer_1] @engineer_1",
    ));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: "ap-engineer_1-0".into(),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        !vt.screen_contains(100, "watching [engineer_1]"),
        "provider-fallback watched-agent block should be removed on finish: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures provider response updates use their explicit agent id as the active
/// prompt owner, then terminal cleanup removes that prompt from all owners.
///
/// This prevents a provider-update-only path from accidentally marking the
/// current/originator agent active and leaving the watched response owner stale
/// after `provider.response_finished`.
#[test]
fn watched_agent_provider_response_update_uses_authoritative_agent_id() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            "ap-engineer_1-0",
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: "__harness__".into(),
                query_id: "parent-query".to_owned(),
            },
        )
    }));
    sync(&handle);

    assert!(eventually_screen_contains(
        &vt,
        100,
        "watching [engineer_1] @engineer_1",
    ));

    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: "ap-engineer_1-0".into(),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "parent-query".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        !vt.screen_contains(100, "watching [engineer_1]"),
        "watched-agent block should clear by terminal prompt id: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures terminal prompt events tombstone their prompt id so delayed start or
/// create events cannot resurrect stale watched-agent blocks.
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
            session_id: "s1".into(),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: "ap-engineer_1-0".into(),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: "ap-engineer_1-0".into(),
        model: "test/model".parse().expect("model id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: "__harness__".into(),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_created("ap-engineer_1-0", "s1")
    }));
    sync(&handle);

    assert!(
        !vt.screen_contains(100, "watching [engineer_1]"),
        "delayed start/create must not resurrect terminal prompt: {:?}",
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
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(4_000_000),
    );
    sync(&handle);
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell ro sleep 10 3s ok"));
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

    assert!(vt.screen_contains(100, "shell rw printf hello 1s ok"));
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
            "sp-streaming",
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
        session_id: "s1".into(),
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
        provider_response_delta_update("sp-0", "hello!", None, tau_proto::PromptOriginator::User),
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
fn agents_md_loaded_event_shows_output_stats() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: "/tmp/AGENTS.md".into(),
        content: "alpha\nbeta\n".into(),
    }));
    sync(&handle);

    let rows = vt.screen_text(80);
    assert!(
        rows.iter()
            .any(|row| row.contains("loaded: /tmp/AGENTS.md 2L, 11B")),
        "loaded event should include output stats: {rows:?}"
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
    let mode_span = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "rw")
        .expect("mode span");
    assert_eq!(
        mode_span.style,
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
    let mode_span = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "rw")
        .expect("mode span");
    assert_eq!(
        mode_span.style,
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

#[test]
fn render_tool_use_state_token_progress_formats_context_like_status_bar() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "[research]".into(),
        progress_counters: vec![ProgressCounter {
            label: Some("ctx".into()),
            unit: ProgressUnit::Tokens,
            complete: Some(133_400),
            total: Some(200_000),
        }],
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("agent_start", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["#133.4k/200k", tau_proto::PROGRESS_INDICATOR_TEXT]
    );
}

/// Ensures the generic watched-agent indicator keeps the compact tool-block
/// shape while using passive styling, an explicit agent-id chip, and no
/// in-progress ellipsis.
#[test]
fn watched_agent_display_uses_tool_block_styles_and_counters() {
    let theme = cli_test_theme();
    let stats = tau_proto::AgentStatsUpdated {
        session_id: "s1".into(),
        agent_id: agent_id("engineer_1"),
        runtime_state: tau_proto::AgentRuntimeState::Running,
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
    };

    let display = watched_agent_tool_display("review", "engineer_1", Some(&stats));
    assert_eq!(display.tool_name, "watching");
    assert_eq!(display.args, "[review]");
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["@engineer_1", "%2/3", "#133.4k/200k"]);

    let block = render_tool_block(&theme, &display);
    let watching = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "watching")
        .expect("watching tool-name span");
    assert_eq!(
        watching.style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME)
    );
    assert_eq!(watching.style.fg, Some(Color::DarkYellow));

    let percent_only_stats = tau_proto::AgentStatsUpdated {
        context: tau_proto::AgentContextStats {
            input_tokens: None,
            cached_tokens: None,
            context_window: None,
            percent_used: Some(67),
        },
        ..stats
    };
    let display = watched_agent_tool_display("review", "engineer_1", Some(&percent_only_stats));
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["@engineer_1", "%2/3", "#67%"]);
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
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("\n     unchanged();"));
    assert!(text.contains("\n-    old();"));
    assert!(text.contains("\n+    new();"));
    assert!(text.contains("\n-let x = 1;\n+let x = 2;"));
    assert!(!text.contains("\n-     old();"));
    assert!(!text.contains("\n+     new();"));
    let removed_line = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "-    old();")
        .expect("removed line uses one span");
    assert_eq!(removed_line.style.fg, Some(tau_cli_term::Color::DarkRed));

    let added_line = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "+    new();")
        .expect("added line uses one span");
    assert_eq!(added_line.style.fg, Some(tau_cli_term::Color::DarkGreen));

    let changed_removed = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "1")
        .expect("removed changed token is split into its own span");
    assert_eq!(changed_removed.style.fg, Some(tau_cli_term::Color::Red));
    assert!(changed_removed.style.bold);

    let changed_added = block
        .content
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
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("\n--- a.txt"));
    assert!(text.contains("\n+alpha"));
    assert!(text.contains("\n--- b.txt"));
    assert!(text.contains("\n-beta"));
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
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

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
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("LOG_MODULE_WALLETV2|┄-walletv2-client/src"));
    assert!(text.contains("err: ripgrep error: ┄ error for operation"));
    assert!(!text.contains(&display.args));
    assert!(!text.contains(&display.status_text));
    assert!(text.contains(&payload));
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

    assert_eq!(line, "Δ0% 0/0 ↑0 ↓0 18s Σ↑0/1k ↓0 5m");
}

#[test]
fn format_turn_stats_line_uses_previous_turn_for_hit_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_000,
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

/// Ensures a provider chain reset cannot show more cacheable tokens than the
/// current full-replay request contains.
#[test]
fn format_turn_stats_line_caps_cache_possible_after_chain_reset() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 13_659,
        prompt_cached_tokens: 3_840,
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

    assert_eq!(line, "Δ0% 0/0 ↑1k ↓0 Σ↑0/1k ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_no_prompt_sent() {
    let usage = tau_proto::ProviderTokenUsage::default();
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ0% 0/0 ↑0 ↓0 Σ↑0/0 ↓0");
}

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
}

#[test]
fn render_action_error_block_uses_action_error_styles() {
    let theme = cli_test_theme();
    let block = render_action_error_block(&theme, "7", "invalid input");
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let error_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ERROR);

    assert_eq!(spans[0].text, "7");
    assert_eq!(spans[0].style, id_style);
    assert_eq!(spans[2].text, "invalid input");
    assert_eq!(spans[2].style, error_style);
}

#[test]
fn render_turn_stats_block_uses_dedicated_styles() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 900,
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

    assert_eq!(spans[0].text, "Δ");
    assert!(spans[0].style.bold);
    assert_eq!(spans[0].style.fg, Some(Color::DarkGrey));
    assert_eq!(spans[1].text, "90% 900/1k");
    assert!(!spans[1].style.bold);
    assert_eq!(spans[1].style.fg, Some(Color::DarkGrey));
    let sigma = spans
        .iter()
        .find(|span| span.text == " Σ")
        .expect("sigma span is rendered");
    assert!(sigma.style.bold);
    assert_eq!(sigma.style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_greys_cache_hit_within_512_rounding_bucket() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_456,
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

    assert_eq!(spans[1].text, "99% 19.4k/19.5k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_warns_cache_hit_above_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_100,
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

    assert_eq!(spans[1].text, "91% 9.1k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_highlights_cache_hit_at_or_below_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_000,
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

    assert_eq!(spans[1].text, "90% 9k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::Red));
}

#[test]
fn cache_hit_percent_clamps_to_possible_cached_tokens() {
    assert_eq!(cache_hit_percent(Some(2_000), Some(1_500)), Some(75));
    assert_eq!(cache_hit_percent(Some(2_000), Some(3_000)), Some(100));
    assert_eq!(cache_hit_percent(Some(0), Some(0)), Some(0));
    assert_eq!(cache_hit_percent(Some(2_000), None), None);
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
        session_id: "s1".into(),
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
        provider_response_delta_update("sp-0", "Hello", None, tau_proto::PromptOriginator::User),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hello"),
        "streaming should show, got: {:?}",
        vt.screen_text(80)
    );

    // User sends 2nd and 3rd prompts while streaming.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
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
        session_id: "s1".into(),
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
            "sp-0",
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
            "sp-1",
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
            "sp-2",
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
        session_id: "s1".into(),
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
        provider_response_delta_update("sp-0", response, None, tau_proto::PromptOriginator::User),
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
        session_id: "s1".into(),
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
        session_id: "s1".into(),
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
        provider_response_delta_update("sp-0", partial, None, tau_proto::PromptOriginator::User),
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
