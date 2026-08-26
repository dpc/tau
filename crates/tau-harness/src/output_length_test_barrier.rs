//! Test-only post-commit crash barriers for deterministic output-length replay.

#[cfg(not(unix))]
use std::io;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Exact durable cut at which the deterministic daemon must stop progressing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputLengthCommitCut {
    /// The planned response committed, before its steer is scheduled.
    AfterPlannedResponse,
    /// The continuation steer committed, before its owner is scheduled.
    AfterContinuationSteer,
    /// A typed receipt and the following sender terminal both committed.
    AfterTypedReceiptSenderTerminal,
    /// Two provider terminals committed in one resumed daemon.
    AfterNextProviderResponse,
}

/// One installed cut and its fixture-private durable handshake path.
#[derive(Debug)]
struct Barrier {
    /// Exact admission-complete boundary whose persistence debt is drained
    /// first.
    cut: OutputLengthCommitCut,
    /// Absent marker created and synced before the daemon blocks.
    reached_path: PathBuf,
    /// Whether the typed-receipt cut observed its inbound durable fact.
    receipt_seen: bool,
    /// Provider terminal count for the resumed two-response cut.
    response_count: u8,
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
    *barrier = Some(Barrier {
        cut,
        reached_path,
        receipt_seen: false,
        response_count: 0,
    });
    Ok(())
}

/// Advances the typed-receipt cut and reports when its sender terminal closes
/// it.
pub(crate) fn observe_typed_receipt(event: &tau_proto::Event) -> Option<OutputLengthCommitCut> {
    let mut installed = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("typed-receipt test barrier lock");
    let barrier = installed.as_mut()?;
    if barrier.cut == OutputLengthCommitCut::AfterNextProviderResponse {
        if matches!(event, tau_proto::Event::ProviderResponseFinished(_)) {
            barrier.response_count += 1;
        }
        return (barrier.response_count == 1).then_some(barrier.cut);
    }
    if barrier.cut != OutputLengthCommitCut::AfterTypedReceiptSenderTerminal {
        return None;
    }
    if matches!(event, tau_proto::Event::AgentMessageReceived(_)) {
        barrier.receipt_seen = true;
        return None;
    }
    (barrier.receipt_seen && matches!(event, tau_proto::Event::ProviderResponseFinished(_)))
        .then_some(barrier.cut)
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
    #[cfg(unix)]
    {
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(b"reached\n")
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "test barrier requires Unix sockets",
        ))
    }
}
