//! Complete native and lossy OTLP exports of durable agent journals.

mod agent_tools;
mod native;
mod otlp;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io::Seek as _;
use std::path::Path;

use tau_core::{AgentJournalLocks, read_agent_creation_record};
use tau_proto::{AgentCreator, AgentId, Event};

use crate::InspectError;

/// Supported durable-agent trace export formats.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentTraceFormat {
    /// Complete, canonical line-delimited Tau journal records.
    #[default]
    TauJsonl,
    /// Lossy OTLP/OpenInference visualization adapter.
    OtlpJson,
    /// Compact model-visible tool-call overview without output bodies.
    AgentToolsLite,
    /// Compact model-visible tool-call overview with output bodies.
    AgentToolsFull,
}

/// Whether to include the recursively creator-owned agent workflow.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DescendantSelection {
    /// Export only the requested journal.
    #[default]
    RootOnly,
    /// Export every recursively creator-owned durable descendant.
    Include,
}

/// A fully validated process-owned trace artifact staged for stdout delivery.
pub struct PreparedAgentTrace {
    /// Anonymous delete-on-close staging file.
    file: std::fs::File,
}

impl PreparedAgentTrace {
    /// Copies the prepared artifact to a caller-owned output stream.
    pub fn copy_to(&mut self, output: &mut (impl std::io::Write + ?Sized)) -> std::io::Result<u64> {
        std::io::copy(&mut self.file, output)
    }
}

/// Validates and privately stages a trace before any caller-visible output.
///
/// The temporary file is process-owned, automatically deleted, and never used
/// as durable trace state. Journal locks are released before this returns.
pub fn prepare_agent_trace(
    agents_dir: &Path,
    root_agent_id: &AgentId,
    descendants: DescendantSelection,
    format: AgentTraceFormat,
) -> Result<PreparedAgentTrace, InspectError> {
    let included = discover_agents(agents_dir, root_agent_id, descendants)?;
    let locks = AgentJournalLocks::acquire(agents_dir, included.iter().cloned())?;
    let rechecked = discover_agents(agents_dir, root_agent_id, descendants)?;
    if included != rechecked {
        return Err(InspectError::Trace(
            crate::AgentTraceError::DescendantsChanged,
        ));
    }
    let snapshot = locks.validate()?;
    let mut file = tempfile::tempfile()?;
    #[cfg(unix)]
    file.set_permissions({
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(0o600)
    })?;
    match format {
        AgentTraceFormat::TauJsonl => native::write_jsonl(root_agent_id, &snapshot, &mut file)?,
        AgentTraceFormat::OtlpJson => otlp::write_json(root_agent_id, &snapshot, &mut file)?,
        AgentTraceFormat::AgentToolsLite => {
            agent_tools::write_lite_jsonl(root_agent_id, &snapshot, &mut file)?
        }
        AgentTraceFormat::AgentToolsFull => {
            agent_tools::write_full_jsonl(root_agent_id, &snapshot, &mut file)?
        }
    }
    file.rewind()?;
    drop(snapshot);
    Ok(PreparedAgentTrace { file })
}

fn discover_agents(
    agents_dir: &Path,
    root_agent_id: &AgentId,
    descendants: DescendantSelection,
) -> Result<BTreeSet<AgentId>, InspectError> {
    let mut included = BTreeSet::from([root_agent_id.clone()]);
    if descendants == DescendantSelection::RootOnly {
        return Ok(included);
    }
    if !agents_dir.try_exists()? {
        return Ok(included);
    }
    let mut creations = Vec::new();
    for entry in std::fs::read_dir(agents_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(agent_id) = AgentId::parse(&name) else {
            continue;
        };
        let Some(record) = read_agent_creation_record(agents_dir, &agent_id)? else {
            continue;
        };
        let Event::AgentStarted(started) = record.event else {
            unreachable!("creation reader validates agent.started");
        };
        creations.push((agent_id, started.creator));
    }
    loop {
        let before = included.len();
        for (agent_id, creator) in &creations {
            if matches!(
                creator,
                Some(AgentCreator::Agent { agent_id, .. }) if included.contains(agent_id)
            ) {
                included.insert(agent_id.clone());
            }
        }
        if included.len() == before {
            break;
        }
    }
    Ok(included)
}
