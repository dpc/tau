//! User-selected exits must prove the terminal boundary before daemon effects.

use super::{CommandOutcome, InputLoopExit, io};

#[cfg(test)]
mod tests;

/// Exit dependencies shared by production dispatch and ordering oracles.
pub(super) struct InteractiveExit<'a> {
    /// Bounded terminal preparation that leaves a successful attachment paused.
    pub(super) prepare: &'a dyn Fn() -> io::Result<()>,
    /// Reacquires Tau after an unacknowledged, still-connected detach.
    pub(super) resume: &'a dyn Fn() -> io::Result<()>,
    /// Requests unconditional session shutdown only after safe preparation.
    pub(super) shutdown: &'a dyn Fn() -> io::Result<()>,
    /// Requests and waits for an authoritative explicit detach decision.
    pub(super) detach: &'a dyn Fn() -> Option<tau_proto::UiQuitDisposition>,
    /// Distinguishes failed detach confirmation from lost transport.
    pub(super) disconnected: &'a dyn Fn() -> bool,
    /// Prints recoverable cancellation and explicit force warnings.
    pub(super) feedback: &'a dyn Fn(&str),
    /// Preserved old keys are not an explicit force-exit request.
    pub(super) deferred: bool,
}

impl InteractiveExit<'_> {
    /// Dispatches only recognized lifecycle commands; failure never sends a
    /// lifecycle request or silently turns a repeated quit into force.
    pub(super) fn execute(&self, text: &str) -> io::Result<CommandOutcome> {
        if !matches!(
            text,
            ":quit" | ":q" | ":quit-session" | ":detach" | ":quit-force"
        ) {
            return Ok(CommandOutcome::NotHandled);
        }
        if text == ":quit-force" {
            if self.deferred {
                (self.feedback)(
                    "Force exit received during handoff was blocked; type :quit-force again explicitly.",
                );
                return Ok(CommandOutcome::Continue);
            }
            (self.feedback)(
                "Force exit: clean shell input cannot be guaranteed; queued clipboard bytes or keys may reach the shell.",
            );
            return Ok(CommandOutcome::Exit(InputLoopExit::Quit));
        }
        if !prepare(self.prepare, self.feedback) {
            return Ok(CommandOutcome::Continue);
        }
        match text {
            ":quit-session" => {
                (self.shutdown)()?;
                Ok(CommandOutcome::Exit(InputLoopExit::QuitSession))
            }
            ":detach" => match (self.detach)() {
                Some(tau_proto::UiQuitDisposition::Detached) => {
                    Ok(CommandOutcome::Exit(InputLoopExit::Detach))
                }
                Some(tau_proto::UiQuitDisposition::Terminating) => {
                    Ok(CommandOutcome::Exit(InputLoopExit::QuitSession))
                }
                None if (self.disconnected)() => Ok(CommandOutcome::Exit(InputLoopExit::Quit)),
                None => {
                    (self.resume)()?;
                    (self.feedback)(
                        "Detach was not confirmed; this UI remains connected. Retry :detach.",
                    );
                    Ok(CommandOutcome::Continue)
                }
            },
            _ => Ok(CommandOutcome::Exit(InputLoopExit::Quit)),
        }
    }
}

/// Shared preparation also protects eligible interactive Ctrl-D.
pub(super) fn prepare(pause: &dyn Fn() -> io::Result<()>, feedback: &dyn Fn(&str)) -> bool {
    match pause() {
        Ok(()) => true,
        Err(error) => {
            feedback(&format!(
                "Quit cancelled: {error}. Retry your quit/detach command or Ctrl-D. \
                 :quit-force exits without guaranteeing clean shell input.",
            ));
            false
        }
    }
}
