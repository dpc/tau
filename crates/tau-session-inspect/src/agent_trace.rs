//! Native, OTLP, and compact exports of durable agent journals.

mod agent_tools;
mod native;
mod otlp;
mod performance;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io::Seek as _;
use std::path::Path;

use tau_core::{AgentJournalSnapshot, read_agent_creation_record};
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
    /// Compact semantic/correlation timeline as JSON Lines.
    AgentToolsJsonl(
        /// Semantic text and tool-output detail encoded in each JSONL item.
        AgentTraceMode,
    ),
    /// Compact semantic/correlation timeline as TOON.
    AgentToolsToon(
        /// Semantic text and tool-output detail encoded in each TOON item.
        AgentTraceMode,
    ),
    /// Content-free provider accounting and journal-wall timing JSON Lines.
    AgentPerformanceJsonl,
}

/// Content detail retained by compact semantic trace formats.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentTraceMode {
    /// Emit complete metrics and at most 4 KiB of each text/output item.
    #[default]
    Lite,
    /// Emit complete metrics and complete semantic text/normalized output.
    Full,
}

impl AgentTraceMode {
    /// Returns the stable compact-schema detail label.
    const fn label(self) -> &'static str {
        match self {
            Self::Lite => "bounded",
            Self::Full => "full",
        }
    }
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
    prepare_agent_trace_with_after_capture(agents_dir, root_agent_id, descendants, format, || {})
}

/// Prepares a trace with a deterministic test hook after journal capture.
fn prepare_agent_trace_with_after_capture(
    agents_dir: &Path,
    root_agent_id: &AgentId,
    descendants: DescendantSelection,
    format: AgentTraceFormat,
    after_capture: impl FnOnce(),
) -> Result<PreparedAgentTrace, InspectError> {
    let included = discover_agents(agents_dir, root_agent_id, descendants)?;
    let snapshot = AgentJournalSnapshot::capture(agents_dir, included.iter().cloned())?;
    after_capture();
    let rechecked = discover_agents(agents_dir, root_agent_id, descendants)?;
    if included != rechecked {
        return Err(InspectError::Trace(
            crate::AgentTraceError::DescendantsChanged,
        ));
    }
    let mut file = tempfile::tempfile()?;
    #[cfg(unix)]
    file.set_permissions({
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::Permissions::from_mode(0o600)
    })?;
    match format {
        AgentTraceFormat::TauJsonl => native::write_jsonl(root_agent_id, &snapshot, &mut file)?,
        AgentTraceFormat::OtlpJson => otlp::write_json(root_agent_id, &snapshot, &mut file)?,
        AgentTraceFormat::AgentToolsJsonl(mode) => {
            agent_tools::write_jsonl(root_agent_id, &snapshot, mode, &mut file)?
        }
        AgentTraceFormat::AgentToolsToon(mode) => {
            agent_tools::write_toon(root_agent_id, &snapshot, mode, &mut file)?
        }
        AgentTraceFormat::AgentPerformanceJsonl => {
            performance::write_jsonl(root_agent_id, &snapshot, &mut file)?
        }
    }
    file.rewind()?;
    drop(snapshot);
    Ok(PreparedAgentTrace { file })
}

/// Runs trace preparation with a deterministic post-capture race hook.
#[cfg(test)]
pub(crate) fn prepare_agent_trace_for_test(
    agents_dir: &Path,
    root_agent_id: &AgentId,
    descendants: DescendantSelection,
    format: AgentTraceFormat,
    after_capture: impl FnOnce(),
) -> Result<PreparedAgentTrace, InspectError> {
    prepare_agent_trace_with_after_capture(
        agents_dir,
        root_agent_id,
        descendants,
        format,
        after_capture,
    )
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
        // An unreadable sequence-zero record cannot authenticate a creation edge.
        // Ignore that unrelated artifact here; any journal whose valid edge makes
        // it reachable has its selected prefix strictly validated below.
        let Some(record) = read_agent_creation_record(agents_dir, &agent_id).unwrap_or(None) else {
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
