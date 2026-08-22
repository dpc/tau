use std::io::Write;

use tau_harness::SessionLaunchStatus;
use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};

/// Storage and identity policy for one prompt diagnostic.
enum RenderDiagnosticKind {
    /// Fresh-agent effective prompt with ordinary configured-extension storage.
    EffectivePrompt,
    /// Existing conservative system-prompt inspection using MemoryOnly storage.
    SystemPrompt,
}

impl RenderDiagnosticKind {
    /// Returns the minted session-id prefix for this diagnostic.
    const fn session_prefix(&self) -> &'static str {
        match self {
            Self::EffectivePrompt => "print-prompt",
            Self::SystemPrompt => "print-system-prompt",
        }
    }

    /// Returns the immutable harness storage policy for this diagnostic.
    const fn storage_mode(&self) -> tau_harness::HarnessStorageMode {
        match self {
            Self::EffectivePrompt => tau_harness::HarnessStorageMode::SessionEphemeral,
            Self::SystemPrompt => tau_harness::HarnessStorageMode::MemoryOnly,
        }
    }
}

pub(crate) fn run_print_prompt(
    role: Option<&str>,
    enable_agents_md: bool,
    profile: Option<&tau_config::settings::ProfileSelection>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    extension_environment: &[String],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let mut daemon = launch_render_daemon(
        RenderDiagnosticKind::EffectivePrompt,
        role,
        profile,
        role_cli_overrides,
        extension_cli_overrides,
        extension_environment,
        harness_config_overrides,
    )?;

    let prompt = get_rendered_prompt(&mut daemon, role, enable_agents_md)?;
    print_prompt(&prompt)
}

pub(crate) fn run_print_system_prompt(
    role: &str,
    profile: Option<&tau_config::settings::ProfileSelection>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    extension_environment: &[String],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let mut daemon = launch_render_daemon(
        RenderDiagnosticKind::SystemPrompt,
        Some(role),
        profile,
        role_cli_overrides,
        extension_cli_overrides,
        extension_environment,
        harness_config_overrides,
    )?;

    let prompt = get_rendered_system_prompt(&mut daemon, role)?;
    print_prompt(&prompt)
}

fn launch_render_daemon(
    kind: RenderDiagnosticKind,
    role: Option<&str>,
    profile: Option<&tau_config::settings::ProfileSelection>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    extension_environment: &[String],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<DaemonHandle, CliError> {
    let session_id = mint_short_id(kind.session_prefix());
    let storage_mode = kind.storage_mode();
    let output = daemon_output_for_session(
        &session_id,
        storage_mode,
        tau_harness::SessionLaunchStatus::New,
    )?;
    let mut daemon = resolve_daemon(
        false,
        &session_id,
        SessionLaunchStatus::New,
        Some(output),
        role,
        DaemonCliOverrides {
            profile,
            role: role_cli_overrides,
            extension: extension_cli_overrides,
            extension_environment: Some(extension_environment),
            harness_config: harness_config_overrides,
            memory_only_agent_store: true,
        },
        storage_mode,
    )?;
    daemon.ensure_runtime_pair_cleanup_after_reap();
    Ok(daemon)
}

fn print_prompt(prompt: &str) -> Result<(), CliError> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(prompt.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn get_rendered_prompt(
    daemon: &mut DaemonHandle,
    role: Option<&str>,
    enable_agents_md: bool,
) -> Result<String, CliError> {
    crate::render_request::request_rendered_value(
        daemon,
        "tau-print-prompt",
        "tau-rendered-prompt",
        |request_id| {
            HarnessInputMessage::GetRenderedPrompt(tau_proto::GetRenderedPrompt {
                request_id,
                role: role.map(str::to_owned),
                enable_agents_md,
            })
        },
        |message, request_id| match message {
            HarnessOutputMessage::RenderedPromptResult(result)
                if result.request_id == request_id =>
            {
                let prompt = if let Some(error) = result.error {
                    Err(CliError::Participant(error))
                } else {
                    result.prompt.ok_or_else(|| {
                        CliError::Participant("daemon returned no rendered prompt".to_owned())
                    })
                };
                RenderResponse::Matched(prompt)
            }
            _ => RenderResponse::Ignore,
        },
    )
}

fn get_rendered_system_prompt(daemon: &mut DaemonHandle, role: &str) -> Result<String, CliError> {
    crate::render_request::request_rendered_value(
        daemon,
        "tau-print-system-prompt",
        "tau-rendered-system-prompt",
        |request_id| {
            HarnessInputMessage::GetRenderedSystemPrompt(tau_proto::GetRenderedSystemPrompt {
                request_id,
                role: role.to_owned(),
            })
        },
        |message, request_id| match message {
            HarnessOutputMessage::RenderedSystemPromptResult(result)
                if result.request_id == request_id =>
            {
                let prompt = if let Some(error) = result.error {
                    Err(CliError::Participant(error))
                } else {
                    result.prompt.ok_or_else(|| {
                        CliError::Participant(
                            "daemon returned no rendered system prompt".to_owned(),
                        )
                    })
                };
                RenderResponse::Matched(prompt)
            }
            _ => RenderResponse::Ignore,
        },
    )
}
