use std::io::Write;

use tau_harness::SessionLaunchStatus;
use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};

/// Ordinary tools, provider-hosted tools, and exact-resolution warnings from
/// the preview harness.
type RenderedToolPreview = (
    Vec<tau_proto::ToolDefinition>,
    Vec<tau_proto::HostedToolDefinition>,
    Vec<String>,
);

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

/// One provider-hosted tool rendered separately from client-executed tools.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum ProviderVisibleToolDefinition {
    /// Ordinary Function or Custom tool routed back through Tau.
    Ordinary(ModelVisibleToolDefinition),
    /// Tool executed directly by the exact model provider.
    Hosted(ModelVisibleHostedToolDefinition),
}

/// One exact-route provider-native tool and its selected controls.
#[derive(serde::Serialize)]
struct ModelVisibleHostedToolDefinition {
    /// Provider-visible name used by the hosted invocation.
    name: tau_proto::ToolName,
    /// Explicit execution boundary distinguishing native from Tau-routed tools.
    execution: &'static str,
    /// Hosted source access selected for provider-native web search.
    access: tau_proto::ProviderWebSearchAccess,
    /// Optional qualitative amount of provider-native search context.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_size: Option<tau_proto::WebSearchContextSize>,
    /// Optional provider-side domain restriction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allowed_domains: Vec<String>,
}

impl From<tau_proto::HostedToolDefinition> for ModelVisibleHostedToolDefinition {
    fn from(definition: tau_proto::HostedToolDefinition) -> Self {
        match definition {
            tau_proto::HostedToolDefinition::WebSearch {
                access,
                context_size,
                allowed_domains,
            } => Self {
                name: tau_proto::ToolName::new("web_search"),
                execution: "provider_native",
                access,
                context_size,
                allowed_domains,
            },
        }
    }
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
            memory_only_agent_store: true,
        },
        tau_harness::HarnessStorageMode::SessionEphemeral,
    )?;

    let result = get_rendered_tool_definitions(&mut daemon, role);
    daemon.wait_requested_exit_or_leak(crate::daemon::REQUESTED_DAEMON_EXIT_WAIT);
    let (ordinary_tools, hosted_tools, warnings) = result?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    let tools = ordinary_tools
        .into_iter()
        .map(ModelVisibleToolDefinition::from)
        .map(ProviderVisibleToolDefinition::Ordinary)
        .chain(
            hosted_tools
                .into_iter()
                .map(ModelVisibleHostedToolDefinition::from)
                .map(ProviderVisibleToolDefinition::Hosted),
        )
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
) -> Result<RenderedToolPreview, CliError> {
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
                let tau_proto::RenderedToolDefinitionsResult {
                    tools,
                    hosted_tools,
                    warnings,
                    error,
                    ..
                } = *result;
                let tools = if let Some(error) = error {
                    Err(CliError::Participant(error))
                } else {
                    tools.ok_or_else(|| {
                        CliError::Participant(
                            "daemon returned no rendered tool definitions".to_owned(),
                        )
                    })
                };
                RenderResponse::Matched(tools.map(|tools| (tools, hosted_tools, warnings)))
            }
            _ => RenderResponse::Ignore,
        },
    )
}

#[cfg(test)]
mod tests;
