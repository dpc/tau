//! Finite worker-side downloads and typed prompt materialization.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tau_client::{ArtifactDownload, ManualRuntimeWaker};
use tau_proto::{
    ArtifactError, ArtifactOp, ArtifactValue, ContextBlock, ContextItem, ImageReference,
    PromptContext, SessionId, ToolResultContentPart,
};
use tau_provider_codex::TurnAbort;

use super::cache::{Flight, Shared};
use super::{MAX_TRANSFERS, Rpc, Signal};

/// One non-renewable local resolution budget, independent of upstream
/// deadlines.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Artifact Busy has no external readiness notification. Only this bounded
/// retry cadence uses a timer; internal waits use completion/cancellation
/// wakes.
const BUSY_BACKOFF: Duration = Duration::from_millis(25);
/// Existing retained-history logical bound also bounds worker materialization.
const MAX_CONTEXT_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

/// Cloneable worker endpoint; all RPC travels through the existing main loop.
#[derive(Clone)]
pub(crate) struct Client {
    /// Process-local verified cache and bounded distinct downloads.
    cache: Arc<Shared>,
    /// Existing worker-to-runtime mailbox.
    tx: mpsc::Sender<crate::WorkerMessage>,
    /// Wakes the existing input loop without introducing a competing reader.
    waker: ManualRuntimeWaker,
}

impl Client {
    /// Binds a worker to the current provider process and runtime mailbox.
    pub(super) fn new(
        cache: Arc<Shared>,
        tx: mpsc::Sender<crate::WorkerMessage>,
        waker: ManualRuntimeWaker,
    ) -> Self {
        Self { cache, tx, waker }
    }

    /// Resolves only typed references, never strings or opaque provider JSON.
    /// The caller owns a private attempt copy, not its retained retry snapshot.
    pub(crate) fn resolve(
        &self,
        context: &mut PromptContext,
        session: &SessionId,
        abort: &mut impl TurnAbort,
    ) -> Result<(), ArtifactError> {
        self.resolve_until(context, session, abort, Instant::now() + TIMEOUT)
    }

    /// Shares one absolute deadline across cache waits and every transfer step.
    pub(super) fn resolve_until(
        &self,
        context: &mut PromptContext,
        session: &SessionId,
        abort: &mut impl TurnAbort,
        deadline: Instant,
    ) -> Result<(), ArtifactError> {
        let mut total = 0_u64;
        visit_parts(context, |parts| {
            for part in parts {
                total = total
                    .checked_add(part.encoded_image_bytes())
                    .ok_or(ArtifactError::Invalid)?;
                if MAX_CONTEXT_IMAGE_BYTES < total {
                    return Err(ArtifactError::Invalid);
                }
                if let ToolResultContentPart::ImageReference(reference) = part {
                    tau_core::validate_provider_image_reference(reference)
                        .map_err(|_| ArtifactError::Invalid)?;
                }
            }
            Ok(())
        })?;
        visit_parts(context, |parts| {
            for part in parts {
                self.check(abort, deadline)?;
                let ToolResultContentPart::ImageReference(reference) = part else {
                    continue;
                };
                let bytes = self.bytes(reference, session, abort, deadline)?;
                let mut image = reference.metadata_only_image();
                image.data = bytes;
                // Cache identity verifies bytes, not a later occurrence's
                // independently supplied MIME, dimensions or detail.
                if !std::panic::catch_unwind(|| {
                    tau_core::validate_inline_provider_image(&image).is_ok()
                })
                .unwrap_or(false)
                {
                    return Err(ArtifactError::Invalid);
                }
                self.check(abort, deadline)?;
                *part = ToolResultContentPart::Image(image);
            }
            Ok(())
        })
    }

    /// Waits for a duplicate flight or bounded capacity under the same
    /// deadline.
    fn bytes(
        &self,
        reference: &ImageReference,
        session: &SessionId,
        abort: &mut impl TurnAbort,
        deadline: Instant,
    ) -> Result<Arc<[u8]>, ArtifactError> {
        let key = &reference.blob.key;
        let (waiter, wake, changed) = self.cache.subscribe()?;
        let abort_waker = abort.register_waker(Arc::new(move || {
            let _ = wake.try_send(());
        }));
        loop {
            self.check(abort, deadline)?;
            let mut cache = self
                .cache
                .inner
                .lock()
                .map_err(|_| ArtifactError::Unavailable)?;
            if let Some(bytes) = cache.get(key) {
                drop(cache);
                // The same public key is not permission to substitute identity
                // metadata on a later occurrence, even when bytes are cached.
                reference.blob.verify(&bytes)?;
                return Ok(bytes);
            }
            if cache.fetching.len() < MAX_TRANSFERS && cache.fetching.insert(key.clone()) {
                break;
            }
            drop(cache);
            changed
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| ArtifactError::Unavailable)?;
        }
        drop(abort_waker);
        drop(waiter);
        let _flight = Flight {
            shared: Arc::clone(&self.cache),
            key: key.clone(),
        };
        let bytes = self.download(reference, session, abort, deadline)?;
        self.check(abort, deadline)?;
        let bytes: Arc<[u8]> = bytes.into();
        let mut cache = self
            .cache
            .inner
            .lock()
            .map_err(|_| ArtifactError::Unavailable)?;
        if self.cache.closed.load(Ordering::Acquire) || deadline <= Instant::now() {
            return Err(ArtifactError::Unavailable);
        }
        cache.insert(key.clone(), Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Uses the established exact-size/digest/acknowledged-close downloader.
    fn download(
        &self,
        reference: &ImageReference,
        session: &SessionId,
        abort: &mut impl TurnAbort,
        deadline: Instant,
    ) -> Result<Vec<u8>, ArtifactError> {
        let mut download = ArtifactDownload::new(reference.blob.key.clone());
        let result = (|| {
            while let Some(op) = download.next_op() {
                let value = self.request(session, op, abort, deadline)?;
                download.accept(value)?;
                if download
                    .descriptor()
                    .is_some_and(|actual| actual != &reference.blob)
                {
                    return Err(ArtifactError::Integrity);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(op) = download.close_op() {
                self.close(session, op);
            }
            return Err(error);
        }
        download.into_bytes()
    }

    /// Bounded Busy handling stays local; it never retries provider work.
    fn request(
        &self,
        session: &SessionId,
        op: ArtifactOp,
        abort: &mut impl TurnAbort,
        deadline: Instant,
    ) -> Result<ArtifactValue, ArtifactError> {
        loop {
            self.check(abort, deadline)?;
            let (reply, receive) = mpsc::sync_channel(1);
            let _keepalive = reply.clone();
            let wake = reply.clone();
            let _abort_waker = abort.register_waker(Arc::new(move || {
                let _ = wake.try_send(Signal::Aborted);
            }));
            let live = Arc::new(AtomicBool::new(true));
            let _pending = Pending(Arc::clone(&live));
            crate::send_worker_message(
                &self.tx,
                &self.waker,
                crate::WorkerMessage::ImageResolveRpc(Rpc {
                    session: session.clone(),
                    op: op.clone(),
                    reply,
                    live,
                }),
            )
            .map_err(|_| ArtifactError::Unavailable)?;
            self.check(abort, deadline)?;
            let result =
                match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Signal::Reply(value)) => {
                        self.check(abort, deadline)?;
                        value.take()
                    }
                    Ok(Signal::Aborted) | Err(_) => return Err(ArtifactError::Unavailable),
                };
            if !matches!(result, Err(ArtifactError::Busy)) {
                return result;
            }
            // The harness service exposes Busy, not a capacity wake. Preserve
            // the original deadline and let cancellation interrupt its backoff.
            match receive
                .recv_timeout(BUSY_BACKOFF.min(deadline.saturating_duration_since(Instant::now())))
            {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return Err(ArtifactError::Unavailable),
            }
        }
    }

    /// Cleanup is best effort and does not retain a worker waiting for its
    /// reply.
    fn close(&self, session: &SessionId, op: ArtifactOp) {
        let (reply, _) = mpsc::sync_channel(1);
        let _ = crate::send_worker_message(
            &self.tx,
            &self.waker,
            crate::WorkerMessage::ImageResolveRpc(Rpc {
                session: session.clone(),
                op,
                reply,
                live: Arc::new(AtomicBool::new(true)),
            }),
        );
    }

    /// Checks both owning cancellation and the non-renewable local deadline.
    fn check(&self, abort: &mut impl TurnAbort, deadline: Instant) -> Result<(), ArtifactError> {
        if abort.is_aborted()
            || self.cache.closed.load(Ordering::Acquire)
            || deadline <= Instant::now()
        {
            Err(ArtifactError::Unavailable)
        } else {
            Ok(())
        }
    }
}

/// A canceled waiter leaves a sticky signal for late allocation cleanup.
struct Pending(Arc<AtomicBool>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Borrow typed parts in all three block families without flattening or copying
/// opaque data.
fn visit_parts(
    context: &mut PromptContext,
    mut visit: impl FnMut(&mut [ToolResultContentPart]) -> Result<(), ArtifactError>,
) -> Result<(), ArtifactError> {
    for block in &mut context.blocks {
        let items = match block {
            ContextBlock::ToolResults(block) => {
                for result in &mut block.items {
                    visit(&mut result.provider_content)?;
                }
                continue;
            }
            ContextBlock::UserInput(block) => &mut block.items,
            ContextBlock::AssistantResponse(block) => &mut block.output_items,
        };
        for item in items {
            if let ContextItem::ToolResult(result) = item {
                visit(&mut result.provider_content)?;
            }
        }
    }
    Ok(())
}
