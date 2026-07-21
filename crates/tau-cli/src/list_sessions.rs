//! Complete live-session snapshot filtering and output.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use tau_harness::runtime_dir::RunningSession;

use crate::CliError;

/// Stable JSON projection for one responsive harness.
#[derive(Serialize)]
struct SessionListJsonEntry<'a> {
    /// Harness-owned current session identifier.
    session_id: &'a tau_proto::SessionId,
    /// Absolute canonical root captured when the harness started.
    project_root: &'a Path,
}

/// Runs `tau session list` after clap has validated and canonicalized
/// arguments.
pub(crate) fn run(args: &crate::cli::SessionListArgs) -> Result<(), CliError> {
    let sessions = tau_harness::runtime_dir::list_running_sessions()?;
    let output = render(&sessions, args)?;
    crate::line_output::write_stdout(&output)
}

/// Builds a complete output snapshot before stdout is touched.
fn render(
    sessions: &[RunningSession],
    args: &crate::cli::SessionListArgs,
) -> Result<String, CliError> {
    let matching = sessions
        .iter()
        .filter(|session| {
            args.dir
                .as_deref()
                .is_none_or(|directory| session.project_root == directory)
        })
        .collect::<Vec<_>>();
    if args.json {
        let rows = matching
            .into_iter()
            .map(|session| SessionListJsonEntry {
                session_id: &session.session_id,
                project_root: &session.project_root,
            })
            .collect::<Vec<_>>();
        let mut output = serde_json::to_string(&rows).map_err(|error| {
            CliError::Participant(format!("failed to serialize running sessions: {error}"))
        })?;
        output.push('\n');
        return Ok(output);
    }

    let session_ids = matching
        .into_iter()
        .map(|session| session.session_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut output = String::new();
    for session_id in session_ids {
        output.push_str(&crate::line_output::escape_field(session_id));
        output.push('\n');
    }
    Ok(output)
}

#[cfg(test)]
#[path = "list_sessions/tests.rs"]
mod tests;
