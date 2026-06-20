use std::io::Write;

use tau_harness::SessionLaunchStatus;
use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};
pub(crate) fn run_print_tools(
    role: &str,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let session_id = mint_short_id("print-tools");
    let output = daemon_output_for_session(&session_id, false)?;
    let mut daemon = resolve_daemon(
        false,
        &session_id,
        SessionLaunchStatus::New,
        Some(output),
        Some(role),
        DaemonCliOverrides {
            role: role_cli_overrides,
            extension: extension_cli_overrides,
            harness_config: harness_config_overrides,
        },
        false,
    )?;

    let tools = get_rendered_tool_definitions(&mut daemon, role)?;

    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &tools).map_err(|error| {
        CliError::Participant(format!("failed to serialize tool definitions: {error}"))
    })?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn get_rendered_tool_definitions(
    daemon: &mut DaemonHandle,
    role: &str,
) -> Result<Vec<tau_proto::ToolDefinition>, CliError> {
    crate::render_request::request_rendered_value(
        daemon,
        "tau-print-tools",
        "tau-rendered-tools",
        |request_id| {
            HarnessInputMessage::GetRenderedToolDefinitions(tau_proto::GetRenderedToolDefinitions {
                request_id,
                role: role.to_owned(),
            })
        },
        |message, request_id| match message {
            HarnessOutputMessage::RenderedToolDefinitionsResult(result)
                if result.request_id == request_id =>
            {
                let tools = if let Some(error) = result.error {
                    Err(CliError::Participant(error))
                } else {
                    result.tools.ok_or_else(|| {
                        CliError::Participant(
                            "daemon returned no rendered tool definitions".to_owned(),
                        )
                    })
                };
                RenderResponse::Matched(tools)
            }
            _ => RenderResponse::Ignore,
        },
    )
}
