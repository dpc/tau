//! Lossy bounded sidecar writes, never canonical publication or journal state.

use std::path::PathBuf;
#[cfg(test)]
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

use tau_config::provider_usage_hint::ProviderUsageHint;

/// Owns nonblocking admission to a single advisory filesystem worker.
pub(super) struct ProviderUsageWriter {
    /// Bounded queue; pressure and worker failure simply discard hints.
    sender: SyncSender<ProviderUsageHint>,
}

impl ProviderUsageWriter {
    /// Exposes bounded admission without a filesystem worker for lifecycle
    /// tests.
    #[cfg(test)]
    pub(super) fn test_channel() -> (Self, Receiver<ProviderUsageHint>) {
        let (sender, receiver) = sync_channel(32);
        (Self { sender }, receiver)
    }

    /// Starts a worker without filesystem work on the caller's thread.
    pub(super) fn start(state_dir: PathBuf) -> Option<Self> {
        let (sender, receiver) = sync_channel::<ProviderUsageHint>(32);
        thread::Builder::new()
            .name("provider-usage-hints".to_owned())
            .spawn(move || {
                while let Ok(hint) = receiver.recv() {
                    if hint.write(&state_dir).is_err() {
                        tracing::debug!(target: "tau_harness", "advisory provider usage write failed");
                    }
                }
            })
            .ok()?;
        Some(Self { sender })
    }

    /// Offers accepted merged quota without waiting for disk or queue space.
    pub(super) fn offer(&self, snapshot: &tau_proto::HarnessProviderQuotaChanged) {
        if let Some(hint) =
            ProviderUsageHint::from_windows(snapshot.provider.clone(), &snapshot.windows)
        {
            let _ = self.sender.try_send(hint);
        }
    }
}

#[cfg(test)]
mod tests;
