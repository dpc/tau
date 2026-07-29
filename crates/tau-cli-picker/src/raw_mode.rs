use std::io;

/// Raw-mode owner that can explicitly restore the terminal before drop.
pub(crate) trait RawModeCleanup {
    /// Restores cooked terminal mode.
    ///
    /// Implementations should keep their drop fallback armed when this returns
    /// an error so callers get a best-effort second cleanup attempt.
    fn restore_raw_mode(&mut self) -> io::Result<()>;
}

/// Guard that enables terminal raw mode on construction and restores
/// cooked mode on drop.
///
/// Callers must not construct this while a parent component already owns
/// raw mode — the drop will silently leave the parent in cooked mode.
pub(crate) struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    /// Enables terminal raw mode and returns a guard that restores cooked mode.
    pub(crate) fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self { active: true })
    }
}

impl RawModeCleanup for RawModeGuard {
    fn restore_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
