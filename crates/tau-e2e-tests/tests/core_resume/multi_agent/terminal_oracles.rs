//! Narrow VT transcript and terminal-row oracles for S8.

use super::{MAIN_PROMPT, WORKER_PROMPT};

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
