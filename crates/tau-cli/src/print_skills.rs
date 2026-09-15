use std::io::Write;
use std::path::PathBuf;

use tau_harness::SessionLaunchStatus;
use tau_proto::{Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage};

use crate::cli::SkillOutputFormat;
use crate::daemon::{DaemonCliOverrides, daemon_output_for_session, resolve_daemon};
use crate::render_request::RenderResponse;
use crate::{CliError, mint_short_id};

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
pub(crate) fn run_print_skills(
    role: Option<&str>,
    format: SkillOutputFormat,
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

    let result = get_effective_skills(&mut daemon, role);
    daemon.wait_requested_exit_or_leak(crate::daemon::REQUESTED_DAEMON_EXIT_WAIT);
    let skills = result?
        .into_iter()
        .map(SkillPreview::from)
        .collect::<Vec<_>>();
    write_skills(std::io::stdout().lock(), format, &skills)
}

/// Initializes one ephemeral role agent and returns its frozen effective
/// skills.
fn get_effective_skills(
    daemon: &mut crate::daemon::DaemonHandle,
    role: Option<&str>,
) -> Result<Vec<tau_proto::DiscoveryEffectiveSkill>, CliError> {
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
                enable_agents_md: false,
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
                let result = effective_skills.take().ok_or_else(|| {
                    result.error.map_or_else(
                        || {
                            CliError::Participant(
                                "daemon returned no effective skill snapshot".to_owned(),
                            )
                        },
                        CliError::Participant,
                    )
                });
                RenderResponse::Matched(result)
            }
            _ => RenderResponse::Ignore,
        },
    )
}

/// Writes the selected skill projection and terminates it with a newline.
fn write_skills(
    mut output: impl Write,
    format: SkillOutputFormat,
    skills: &[SkillPreview],
) -> Result<(), CliError> {
    match format {
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
