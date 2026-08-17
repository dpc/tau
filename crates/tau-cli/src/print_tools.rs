use std::io::Write;

use tau_harness::SessionLaunchStatus;
use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};

/// One tool definition rendered exactly as a provider exposes it to the model.
#[derive(serde::Serialize)]
struct ModelVisibleToolDefinition {
    /// Provider-visible name used by model tool calls.
    name: tau_proto::ToolName,
    /// Optional model-visible description.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Whether the tool accepts JSON-schema function input or freeform input.
    tool_type: tau_proto::ToolType,
    /// Optional JSON Schema describing function-tool input.
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
    /// Optional freeform/custom input format.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<tau_proto::ToolFormat>,
}

impl From<tau_proto::ToolDefinition> for ModelVisibleToolDefinition {
    fn from(definition: tau_proto::ToolDefinition) -> Self {
        Self {
            name: definition.model_visible_name.unwrap_or(definition.name),
            description: definition.description,
            tool_type: definition.tool_type,
            parameters: definition.parameters,
            format: definition.format,
        }
    }
}

pub(crate) fn run_print_tools(
    role: Option<&str>,
    profile: Option<&tau_config::settings::ProfileSelection>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    extension_environment: &[String],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let session_id = mint_short_id("print-tools");
    let output = daemon_output_for_session(
        &session_id,
        tau_harness::HarnessStorageMode::SessionEphemeral,
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
        },
        tau_harness::HarnessStorageMode::SessionEphemeral,
    )?;
    daemon.ensure_runtime_pair_cleanup_after_reap();

    let tools = get_rendered_tool_definitions(&mut daemon, role)?
        .into_iter()
        .map(ModelVisibleToolDefinition::from)
        .collect::<Vec<_>>();

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
    role: Option<&str>,
) -> Result<Vec<tau_proto::ToolDefinition>, CliError> {
    crate::render_request::request_rendered_value(
        daemon,
        "tau-print-tools",
        "tau-rendered-tools",
        |request_id| {
            HarnessInputMessage::GetRenderedToolDefinitions(tau_proto::GetRenderedToolDefinitions {
                request_id,
                role: role.map(str::to_owned),
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

#[cfg(test)]
mod tests;
