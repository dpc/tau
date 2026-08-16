//! Test-only post-commit crash barriers for deterministic output-length replay.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Exact durable cut at which the deterministic daemon must stop progressing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputLengthCommitCut {
    /// The planned response committed, before its steer is scheduled.
    AfterPlannedResponse,
    /// The continuation steer committed, before its owner is scheduled.
    AfterContinuationSteer,
}

/// One installed cut and its fixture-private durable handshake path.
#[derive(Debug)]
struct Barrier {
    /// Exact write-complete boundary that consumes this barrier.
    cut: OutputLengthCommitCut,
    /// Absent marker created and synced before the daemon blocks.
    reached_path: PathBuf,
}

static BARRIER: OnceLock<Mutex<Option<Barrier>>> = OnceLock::new();

/// Install the process-local one-shot barrier before starting the test daemon.
///
/// # Errors
///
/// Returns an error if another barrier is already installed.
pub fn install(cut: OutputLengthCommitCut, reached_path: PathBuf) -> Result<(), &'static str> {
    let mut barrier = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "output-length test barrier lock poisoned")?;
    if barrier.is_some() {
        return Err("output-length test barrier already installed");
    }
    *barrier = Some(Barrier { cut, reached_path });
    Ok(())
}

/// Consume and block at the matching installed cut; nonmatching cuts are inert.
pub(crate) fn reach(cut: OutputLengthCommitCut) {
    let mut installed = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("output-length test barrier lock");
    if !installed.as_ref().is_some_and(|barrier| barrier.cut == cut) {
        return;
    }
    let barrier = installed.take().expect("matching barrier is installed");
    drop(installed);
    write_reached_marker(&barrier.reached_path)
        .expect("write durable output-length test barrier marker");
    loop {
        std::thread::park();
    }
}

fn write_reached_marker(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(b"reached\n")?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
