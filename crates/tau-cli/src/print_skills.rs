use std::io::Write;
use std::path::PathBuf;

use tau_harness::SessionLaunchStatus;
use tau_proto::{Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage};

use crate::cli::SkillOutputFormat;
use crate::daemon::{DaemonCliOverrides, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};

/// Component and directory selected for a fresh, non-inferencing preview.
pub(crate) struct DiscoveryPrintOptions<'a> {
    /// Explicit role, or the configured startup role.
    pub(crate) role: Option<&'a str>,
    /// Optional startup directory for the preview harness and its agent.
    pub(crate) workdir: Option<&'a std::path::Path>,
    /// Envelope or retained inventory encoding.
    pub(crate) format: SkillOutputFormat,
    /// Select AGENTS.md rather than skill advertisements.
    pub(crate) agents_md: bool,
}

/// Both inspection projections come from the same initialized preview agent.
struct DiscoveryPreview {
    /// Eligible inventory, including unadvertised skills for legacy formats.
    effective_skills: Vec<tau_proto::DiscoveryEffectiveSkill>,
    /// Full initial content rendered by the harness's normal discovery path.
    delivery: tau_proto::DiscoveryContextDelivery,
}

/// Stable developer-facing projection of one effective role skill.
#[derive(Debug, Eq, PartialEq, serde::Serialize)]
struct SkillPreview {
    /// Validated effective skill name.
    name: tau_proto::SkillName,
    /// Filesystem source path, or `None` for a skill built into Tau.
    path: Option<PathBuf>,
    /// Short human-facing purpose from skill frontmatter.
    description: String,
}

impl From<tau_proto::DiscoveryEffectiveSkill> for SkillPreview {
    fn from(skill: tau_proto::DiscoveryEffectiveSkill) -> Self {
        let path = match skill.source {
            tau_proto::DiscoveryEffectiveSkillSource::File { path } => Some(path),
            tau_proto::DiscoveryEffectiveSkillSource::BuiltIn => None,
        };
        Self {
            name: skill.name,
            path,
            description: skill.description,
        }
    }
}

/// Prints the collision-resolved, readable effective skill set for one role.
pub(crate) fn run_print_discovery(
    options: DiscoveryPrintOptions<'_>,
    profile: Option<&tau_config::settings::ProfileSelection>,
    role_cli_overrides: &[tau_config::settings::RoleCliOverride],
    extension_cli_overrides: &[tau_config::settings::ExtensionCliOverride],
    extension_environment: &[String],
    harness_config_overrides: &[tau_config::settings::HarnessConfigCliOverride],
) -> Result<(), CliError> {
    let session_id = mint_short_id("print-skills");
    let storage_mode = tau_harness::HarnessStorageMode::SessionEphemeral;
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
        options.role,
        DaemonCliOverrides {
            workdir: options.workdir,
            profile,
            role: role_cli_overrides,
            extension: extension_cli_overrides,
            extension_environment: Some(extension_environment),
            harness_config: harness_config_overrides,
            memory_only_agent_store: true,
        },
        storage_mode,
    )?;

    let result = get_discovery_preview(&mut daemon, options.role);
    daemon.wait_requested_exit_or_leak(crate::daemon::REQUESTED_DAEMON_EXIT_WAIT);
    let preview = result?;
    if options.format == SkillOutputFormat::Envelope {
        return write_discovery(
            std::io::stdout().lock(),
            preview.delivery,
            options.agents_md,
        );
    }
    let skills = preview
        .effective_skills
        .into_iter()
        .map(SkillPreview::from)
        .collect::<Vec<_>>();
    write_skills(std::io::stdout().lock(), options.format, &skills)
}

/// Initializes one ephemeral role agent and returns its frozen effective
/// skills.
fn get_discovery_preview(
    daemon: &mut crate::daemon::DaemonHandle,
    role: Option<&str>,
) -> Result<DiscoveryPreview, CliError> {
    let mut effective_skills = None;
    crate::render_request::request_rendered_value_with_selectors(
        daemon,
        "tau-print-skills",
        "tau-rendered-skills",
        vec![EventSelector::Exact(
            EventName::HARNESS_AGENT_CONTEXT_INITIALIZED,
        )],
        |request_id| {
            HarnessInputMessage::GetRenderedPrompt(tau_proto::GetRenderedPrompt {
                request_id,
                role: role.map(str::to_owned),
                enable_agents_md: true,
            })
        },
        |message, request_id| match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if let Event::HarnessAgentContextInitialized(initialized) = *delivery.event {
                    effective_skills = Some(initialized.effective_skills);
                }
                RenderResponse::Ignore
            }
            HarnessOutputMessage::RenderedPromptResult(result)
                if result.request_id == request_id =>
            {
                let tau_proto::RenderedPromptResult {
                    discovery_context,
                    error,
                    ..
                } = *result;
                let result = (|| {
                    // Discovery is independently valid even when an unrelated
                    // system template fails to render. print-prompt still
                    // reports that error through its ordinary consumer.
                    let delivery =
                        discovery_context.ok_or_else(|| {
                            CliError::Participant(error.unwrap_or_else(|| {
                                "daemon returned no discovery preview".to_owned()
                            }))
                        })?;
                    Ok(DiscoveryPreview {
                        effective_skills: effective_skills.take().ok_or_else(|| {
                            CliError::Participant(
                                "daemon returned no effective skill snapshot".to_owned(),
                            )
                        })?,
                        delivery,
                    })
                })();
                RenderResponse::Matched(result)
            }
            _ => RenderResponse::Ignore,
        },
    )
}

/// Print one component using the same exact-close renderer as model delivery.
fn write_discovery(
    mut output: impl Write,
    mut delivery: tau_proto::DiscoveryContextDelivery,
    agents_md: bool,
) -> Result<(), CliError> {
    let prefix = if agents_md { "agents:" } else { "skill:" };
    delivery
        .items
        .retain(|item| item.identity.starts_with(prefix));
    delivery
        .scope
        .retain(|item| item.identity.starts_with(prefix));
    writeln!(output, "{}", delivery.render())?;
    output.flush()?;
    Ok(())
}

/// Writes the selected skill projection and terminates it with a newline.
fn write_skills(
    mut output: impl Write,
    format: SkillOutputFormat,
    skills: &[SkillPreview],
) -> Result<(), CliError> {
    match format {
        SkillOutputFormat::Envelope => unreachable!("envelopes use the discovery renderer"),
        SkillOutputFormat::Markdown => write_markdown(&mut output, skills)?,
        SkillOutputFormat::Json => {
            serde_json::to_writer_pretty(&mut output, skills).map_err(|error| {
                CliError::Participant(format!("failed to serialize skills: {error}"))
            })?
        }
    }
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// Writes a compact Markdown document with one section per skill.
fn write_markdown(mut output: impl Write, skills: &[SkillPreview]) -> std::io::Result<()> {
    writeln!(output, "# Available skills")?;
    for skill in skills {
        writeln!(output)?;
        writeln!(output, "## {}", markdown_code_span(skill.name.as_str()))?;
        match &skill.path {
            Some(path) => writeln!(
                output,
                "**Path:** {}",
                markdown_code_span(&path.to_string_lossy())
            )?,
            None => writeln!(output, "**Path:** built-in")?,
        }
        writeln!(output)?;
        writeln!(output, "{}", skill.description)?;
    }
    Ok(())
}

/// Wraps arbitrary text in a CommonMark code span without changing its value.
fn markdown_code_span(value: &str) -> String {
    let longest_run = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run + 1);
    let padding = value.contains('`')
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace);
    if padding {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

#[cfg(test)]
mod tests;
