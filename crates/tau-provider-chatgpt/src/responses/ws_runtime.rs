//! Shared Tokio runtime for provider-builtin network IO.
//!
//! One process-wide multi-thread runtime, lazily started on first
//! use. The runtime is intentionally narrow in scope today (WS pool
//! tasks for the Codex Responses backend) but built broad: any
//! future async client (a tokio-based replacement for the `ureq`
//! HTTP+SSE path, for instance) can `handle().spawn(...)` here
//! without bringing its own runtime.
//!
//! Why a dedicated runtime and not async-everything at the provider
//! boundary: the provider's main loop is sync (blocking mpsc, blocking
//! protocol frame IO). Driving async tasks from sync code
//! is fine via the `Handle::block_on` / `Sender::send` /
//! `Receiver::blocking_recv` boundary primitives.

use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

/// Worker thread count. Two is enough for typical provider operation
/// (one for the reader half of an active WS conn, one for the
/// writer half + auxiliary tasks). Bumped or made dynamic if a
/// future workload needs more.
const WORKER_THREADS: usize = 2;

/// Return a handle to the process-wide runtime, initializing it on
/// first call. The handle is `Clone` and cheap to obtain — callers
/// don't need to cache it.
///
/// Panics only on first call, and only if the OS rejects thread
/// creation. We let that surface as a panic rather than thread it
/// through every call site: the provider process is useless without
/// its network runtime, and the panic message is more actionable
/// than a `Result` that callers would `.expect()` anyway.
pub fn handle() -> Handle {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(WORKER_THREADS)
                .enable_all()
                .thread_name("tau-provider-builtin-net")
                .build()
                .expect("build tokio runtime for provider-builtin network IO")
        })
        .handle()
        .clone()
}
