//! UI-only discovery and verified download on the existing admitted transport.

mod command;
mod rpc;
mod selection;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rpc::Rpc;
use tau_cli_term::{ArgCompleter, CompletionItem, TermHandle};
use tau_proto::{
    ArtifactKey, ArtifactListEntry, ArtifactOp, ArtifactResult, ArtifactValue, SessionId,
};

use super::WriterHandle;

/// One bounded cache and coalesced demand, never an all-store in-memory index.
#[derive(Default)]
struct State {
    /// Most recent requested prefix and menu generation.
    wanted: Option<(String, u64, TermHandle)>,
    /// Current prefix's bounded complete answer (or empty failed answer).
    cache: Option<(String, Instant, Vec<CompletionItem>)>,
    /// One reserved export waiting for the worker.
    export: Option<Export>,
}

/// Selected original and revocable raw-terminal reservation.
struct Export {
    /// Full validated content address.
    key: ArtifactKey,
    /// Explicit or automatic clipboard intent resolved after download.
    mode: command::Mode,
    /// Terminal reservation captured before any asynchronous work.
    token: u64,
    /// Destination attachment, never a second terminal output owner.
    term: TermHandle,
}

/// Bounded single worker for one attachment's list and download requests.
#[derive(Clone)]
pub(super) struct ClipboardExport {
    /// One coalesced wake, independent of result delivery.
    wake: mpsc::SyncSender<()>,
    /// Latest user demand and bounded cache.
    state: Arc<Mutex<State>>,
    /// Exact outstanding response correlation; one result can be delivered.
    rpc: Arc<Rpc>,
    /// Prevents queued exports from multiplying while a download unwinds.
    exporting: Arc<AtomicBool>,
}

impl ClipboardExport {
    /// Starts one worker; all RPC writes and waits remain off the UI loop.
    pub(super) fn spawn(writer: WriterHandle, session: SessionId) -> (Self, JoinHandle<()>) {
        let (wake, receiver) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State::default()));
        let rpc = Arc::new(Rpc::new(writer, session));
        let exporting = Arc::new(AtomicBool::new(false));
        let handle = Self {
            wake,
            state,
            rpc,
            exporting,
        };
        let worker = handle.clone();
        let join = std::thread::spawn(move || worker.run(receiver));
        (handle, join)
    }

    /// Consumes matching responses; returns unrelated upload replies unchanged.
    pub(super) fn deliver(&self, result: ArtifactResult) -> Option<ArtifactResult> {
        self.rpc.deliver(result)
    }

    /// Revokes worker authority before joining after transport shutdown.
    pub(super) fn stop(&self) {
        self.rpc.stop();
        let _ = self.wake.try_send(());
    }

    /// Builds a nonblocking prefix-before-limit completion callback.
    pub(super) fn completer(&self, term: TermHandle, supported: bool) -> ArgCompleter {
        let this = self.clone();
        Arc::new(move |args| {
            let [prefix] = args else { return vec![] };
            if !supported || !tau_proto::artifact_prefix_valid(prefix) {
                return vec![];
            }
            let generation = term.completion_refresh_generation();
            let mut state = this.state.lock().expect("clipboard completion state");
            if let Some((cached, at, rows)) = &state.cache
                && cached == prefix
                && at.elapsed() < Duration::from_secs(3)
            {
                return rows.clone();
            }
            state.wanted = Some(((*prefix).to_owned(), generation, term.clone()));
            drop(state);
            let _ = this.wake.try_send(());
            vec![]
        })
    }

    /// Validates command grammar and reserves lifecycle authority before
    /// queuing.
    pub(super) fn start(&self, args: &str, term: TermHandle) -> Result<(), &'static str> {
        let command::Command { key, mode } = command::Command::parse(args)?;
        if self
            .exporting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Clipboard export worker busy; retry after the previous request settles");
        }
        let token = match term.reserve_clipboard_export() {
            Ok(token) => token,
            Err(error) => {
                self.exporting.store(false, Ordering::Release);
                return Err(error);
            }
        };
        self.state
            .lock()
            .expect("clipboard completion state")
            .export = Some(Export {
            key,
            mode,
            token,
            term,
        });
        let _ = self.wake.try_send(());
        Ok(())
    }

    fn run(&self, receiver: mpsc::Receiver<()>) {
        while receiver.recv().is_ok() && self.rpc.alive() {
            // One quiet period coalesces edits; no scan in a completion
            // callback.
            let debounce_end = Instant::now() + Duration::from_millis(250);
            while Instant::now() < debounce_end
                && receiver.recv_timeout(Duration::from_millis(80)).is_ok()
            {
                if !self.rpc.alive() {
                    return;
                }
            }
            let export = self
                .state
                .lock()
                .expect("clipboard completion state")
                .export
                .take();
            if let Some(export) = export {
                self.download(export);
                self.exporting.store(false, Ordering::Release);
            }
            if !self.rpc.alive() {
                break;
            }
            let wanted = self
                .state
                .lock()
                .expect("clipboard completion state")
                .wanted
                .clone();
            let Some((prefix, _, _)) = wanted else {
                continue;
            };
            let result = self.rpc.request(
                ArtifactOp::List {
                    prefix: prefix.clone(),
                    limit: tau_proto::ARTIFACT_LIST_LIMIT,
                },
                Instant::now() + Duration::from_secs(4),
                || true,
            );
            let (rows, failed) = match result {
                Ok(ArtifactValue::Listed { entries }) if valid_page(&prefix, &entries) => {
                    (entries.into_iter().map(row).collect(), false)
                }
                _ => (vec![], true),
            };
            let refresh = {
                let mut state = self.state.lock().expect("clipboard completion state");
                if state
                    .wanted
                    .as_ref()
                    .is_some_and(|(current, _, _)| current == &prefix)
                {
                    state.cache = Some((prefix, Instant::now(), rows));
                    state.wanted.take()
                } else {
                    None
                }
            };
            if let Some((_, generation, term)) = refresh {
                if failed && term.completion_refresh_generation() == generation {
                    term.export_notice(
                        "Artifact suggestions unavailable; no partial recent-key list was used.",
                    );
                }
                term.request_completion_refresh_if_generation(generation);
            }
        }
    }

    fn download(&self, export: Export) {
        if let Err(error) = export
            .term
            .watch_clipboard_export(export.token, self.rpc.wake.clone())
        {
            export.term.fail_clipboard_export(export.token, error);
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(50);
        let result =
            self.rpc
                .download(export.key.clone(), deadline, || {
                    export.term.clipboard_export_live(export.token)
                })
                .and_then(|bytes| {
                    match selection::Selection::select(&export.key, export.mode, &bytes)? {
                        selection::Selection::File(name) => export
                            .term
                            .supply_clipboard_file_export(export.token, bytes, &name),
                        selection::Selection::Raw(media) => export.term.supply_clipboard_export(
                            export.token,
                            bytes,
                            &media.mime,
                            media.plain_alias,
                        ),
                    }
                });
        if let Err(error) = result {
            export.term.fail_clipboard_export(export.token, error);
        }
    }
}

/// Checks bounded, prefix-filtered, sorted pages before caching remote
/// metadata.
fn valid_page(prefix: &str, entries: &[ArtifactListEntry]) -> bool {
    tau_proto::artifact_prefix_valid(prefix)
        && entries.len() <= tau_proto::ARTIFACT_LIST_LIMIT as usize
        && entries.iter().all(|entry| {
            let key = &entry.descriptor.key;
            key.as_str().starts_with(prefix)
        })
        && entries.windows(2).all(|pair| {
            pair[0].last_put_at > pair[1].last_put_at
                || (pair[0].last_put_at == pair[1].last_put_at
                    && pair[0].descriptor.key.as_str() < pair[1].descriptor.key.as_str())
        })
}

fn row(entry: ArtifactListEntry) -> CompletionItem {
    let key = entry.descriptor.key.as_str();
    let label = format!("{}…{}", &key[..15], &key[key.len() - 6..]);
    let description = entry
        .descriptor
        .identity
        .mime_type()
        .filter(|mime| !mime.trim().is_empty())
        .map(|mime| format!("{mime} {} bytes", entry.descriptor.size.get()))
        .unwrap_or_else(|| format!("{} bytes", entry.descriptor.size.get()));
    CompletionItem::new(key, description).with_label(label)
}

/// UI controls are withheld from older harnesses rather than silently timing
/// out.
pub(super) fn supported(version: Option<tau_proto::ProtocolVersion>) -> bool {
    version.is_some_and(|version| version >= tau_proto::ProtocolVersion::new(13, 1))
}
