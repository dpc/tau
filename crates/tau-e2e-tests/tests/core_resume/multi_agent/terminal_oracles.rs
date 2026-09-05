//! Narrow VT transcript and terminal-row oracles for S8.

use tau_proto::AgentId;

use super::{
    HIDDEN_MODEL_SENTINEL, Identities, MAIN_FINAL_RESPONSE, MAIN_PROMPT, MAIN_START_RESPONSE,
    WORKER_PROMPT, WORKER_RESPONSE,
};

/// Returns the exact compact idle row for the selected worker.
pub(super) fn worker_compact_idle_row(agent_id: &AgentId) -> String {
    format!("❓💤 @{agent_id}")
}

/// Projects stable agent-owned prompt/response rows and rejects output owned
/// exclusively by the other selected transcript.
pub(super) fn assert_transcript_rows(
    frame: &str,
    agent_id: &AgentId,
    identities: &Identities,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let (required, forbidden) = if agent_id == &identities.main {
        (
            [MAIN_START_RESPONSE, WORKER_RESPONSE, MAIN_FINAL_RESPONSE].as_slice(),
            [WORKER_PROMPT].as_slice(),
        )
    } else {
        (
            [WORKER_PROMPT, WORKER_RESPONSE].as_slice(),
            [MAIN_PROMPT, MAIN_START_RESPONSE, MAIN_FINAL_RESPONSE].as_slice(),
        )
    };
    let projected = frame
        .lines()
        .filter(|row| required.iter().any(|marker| row.contains(marker)))
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let actual_order = projected
        .iter()
        .flat_map(|row| {
            required
                .iter()
                .copied()
                .filter(move |marker| row.contains(marker))
        })
        .collect::<Vec<_>>();
    if actual_order != required
        || required
            .iter()
            .any(|marker| frame.match_indices(marker).count() > 1)
        || forbidden.iter().any(|marker| frame.contains(marker))
        || frame.contains(HIDDEN_MODEL_SENTINEL)
    {
        return Err(
            format!("agent {agent_id} transcript materialization mixed rows:\n{frame}").into(),
        );
    }
    Ok(projected)
}

/// Stable worker classes derived from visible semantic boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkerSemanticClass {
    /// Exact beginning of the internal worker prompt.
    InternalPromptStart,
    /// Exact terminal instruction boundary of that prompt.
    InternalPromptInstruction,
    /// Completed worker response.
    Response,
    /// Exact idle-navigation state.
    Idle,
    /// Selected stable worker identity.
    Selected(AgentId),
}

/// Derives exact ordered worker classes across allowed wrapping.
pub(super) fn assert_worker_size_projection(
    frame: &str,
    agent_id: &AgentId,
    identities: &Identities,
) -> Result<Vec<WorkerSemanticClass>, Box<dyn std::error::Error>> {
    assert_transcript_rows(frame, agent_id, identities)?;
    let semantic = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    let initial_start = "You were started by an agent `main`.";
    let required = [
        (initial_start, WorkerSemanticClass::InternalPromptStart),
        (
            super::WORKER_PROMPT,
            WorkerSemanticClass::InternalPromptInstruction,
        ),
        (super::WORKER_RESPONSE, WorkerSemanticClass::Response),
        (
            "This active-auto agent is idle. Use :resume to keep it in navigation.",
            WorkerSemanticClass::Idle,
        ),
    ];
    let mut observed = Vec::new();
    for (text, class) in required {
        let matches = semantic.match_indices(text).collect::<Vec<_>>();
        let [(position, _)] = matches.as_slice() else {
            return Err(format!(
                "worker projection expected one `{text}` class, found {}",
                matches.len()
            )
            .into());
        };
        observed.push((*position, class));
    }
    let status_rows = frame
        .lines()
        .filter(|line| line.contains(&format!("@{agent_id} ")))
        .collect::<Vec<_>>();
    let [status] = status_rows.as_slice() else {
        return Err(format!(
            "worker projection expected one selected status for `{agent_id}`, found {}",
            status_rows.len()
        )
        .into());
    };
    let status_position = semantic
        .find(status.trim())
        .ok_or("selected status missing from normalized projection")?;
    observed.push((
        status_position,
        WorkerSemanticClass::Selected(agent_id.clone()),
    ));
    observed.sort_by_key(|(position, _)| *position);
    Ok(observed.into_iter().map(|(_, class)| class).collect())
}

/// Checks the selected main transcript and terminal `agent_start` row.
pub(super) fn assert_main_terminal_frame(frame: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !frame.contains(MAIN_PROMPT) || !frame.contains("worker completion observed") {
        return Err(format!("S8 main transcript did not restore:\n{frame}").into());
    }
    let row = frame
        .lines()
        .find(|line| line.contains("agent_start"))
        .ok_or("S8 main transcript omitted the completed agent_start row")?;
    if !row.contains("ok") || row.contains("pending") || row.contains('…') {
        return Err(format!("S8 restored agent_start row is not terminal: {row}").into());
    }
    Ok(())
}

/// Checks the selected worker shows only its completed restored transcript.
pub(super) fn assert_worker_restored_frame(frame: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !frame.contains(WORKER_PROMPT) || !frame.contains("worker boot-a complete") {
        return Err(format!("S8 worker transcript did not restore:\n{frame}").into());
    }
    if frame.contains("pending") || frame.contains("worker completion observed") {
        return Err(
            format!("S8 worker selection mixed transcript or pending state:\n{frame}").into(),
        );
    }
    Ok(())
}

/// Checks the targeted worker continuation follows its restored transcript.
pub(super) fn assert_worker_fresh_frame(frame: &str) -> Result<(), Box<dyn std::error::Error>> {
    let restored = frame
        .find("worker boot-a complete")
        .ok_or("S8 restored worker marker disappeared")?;
    let prompt = frame
        .find("fresh worker work")
        .ok_or("S8 targeted worker prompt did not render")?;
    let response = frame
        .find("fresh worker complete")
        .ok_or("S8 targeted worker response did not render")?;
    if !(restored < prompt && prompt < response) || frame.contains("pending") {
        return Err(
            format!("S8 worker transcript ordering or terminal state changed:\n{frame}").into(),
        );
    }
    Ok(())
}
