//! Detached, process-wide writer for best-effort debug JSONL lines.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Instant;
use std::{fs as path_std_fs, thread as path_std_thread};
#[cfg(test)]
use std::{io as path_std_io, sync as path_std_sync};

use fs2::FileExt as _;

#[cfg(test)]
use super::DebugLogWorkerTrace;
#[cfg(test)]
use super::test_io::{AppendFault, FaultInjectingFile};
use super::{DebugLogCycleResult, DebugLogError, LineAppendError, append_line};

const MAX_RETAINED_LINES: usize = 1_024;
const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;
const DROP_EPISODE_ACTIVE: u64 = 1 << 63;
const DROP_EPISODE_COUNT_MASK: u64 = !DROP_EPISODE_ACTIVE;
const DIAGNOSTIC_CHARS: usize = 256;

/// Process-wide producer handle for detached debug JSONL writes.
struct DebugWriter {
    /// Shared bounded queue and failure state.
    queue: Arc<DebugWriterQueue>,
    /// Bounded transport producer; dropping the final handle ends the worker.
    sender: Option<SyncSender<DebugWriteJob>>,
    /// Join handle used only to make direct worker tests deterministic.
    #[cfg(test)]
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Queue admission counters and worker transport shared by every producer.
struct DebugWriterQueue {
    /// Queued plus in-flight line count.
    retained_lines: AtomicUsize,
    /// Queued plus in-flight encoded-line and path bytes.
    retained_bytes: AtomicUsize,
    /// Whether uncertain rollback permanently disabled the worker.
    poisoned: AtomicBool,
    /// Active bit plus saturating count for one coherent drop episode.
    drop_episode: AtomicU64,
    /// Saturating recoverable worker-I/O failure count.
    io_failures: AtomicU64,
    /// Whether the current worker-I/O failure interval emitted a warning.
    io_warning_active: AtomicBool,
    /// Number of sidecar lock attempts, exposed only to deterministic tests.
    #[cfg(test)]
    lock_attempts: AtomicUsize,
    /// Number of completed worker jobs, exposed only to deterministic tests.
    #[cfg(test)]
    completed_jobs: AtomicUsize,
    /// Worker timing records, exposed only to deterministic tests.
    #[cfg(test)]
    worker_traces: std::sync::Mutex<Vec<DebugLogWorkerTrace>>,
    /// Deterministic concurrency pause points used only by direct worker tests.
    #[cfg(test)]
    test_pauses: DebugWriterTestPauses,
}

/// One fully serialized line retained by the bounded writer.
struct DebugWriteJob {
    /// Destination session JSONL path.
    path: PathBuf,
    /// Complete encoded JSON object including its trailing newline.
    line: Vec<u8>,
    /// RAII permit that releases the line and byte accounting on every exit.
    _permit: RetainedWorkPermit,
    /// Deterministic append fault used only by direct worker tests.
    #[cfg(test)]
    fault: Option<AppendFault>,
    /// Deterministic pre-append I/O fault used only by direct worker tests.
    #[cfg(test)]
    io_fault: Option<DebugWriterIoFault>,
}

/// Accounting permit owned by an admitted job until that job is destroyed.
struct RetainedWorkPermit {
    /// Queue whose retained-work counters this permit charged.
    queue: Arc<DebugWriterQueue>,
    /// Bytes charged for the encoded line and its destination path.
    retained_bytes: usize,
}

#[cfg(test)]
#[derive(Default)]
/// Optional pause points for deterministic admission-versus-poison testing.
struct DebugWriterTestPauses {
    /// Pause after admission reserves its accounting and before it sends.
    before_send: std::sync::Mutex<Option<DebugWriterTestPause>>,
    /// Pause after poison drains the receiver and before the receiver drops.
    after_poison_drain: std::sync::Mutex<Option<DebugWriterTestPause>>,
}

#[cfg(test)]
#[derive(Clone)]
/// Two-phase test barrier that reports arrival and awaits explicit release.
struct DebugWriterTestPause {
    /// Barrier used to report that the instrumented thread arrived.
    entered: Arc<std::sync::Barrier>,
    /// Barrier used to hold the instrumented thread until the test releases it.
    resume: Arc<std::sync::Barrier>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Worker I/O phase failed by a deterministic direct-worker test.
enum DebugWriterIoStage {
    /// Parent-directory creation.
    Directory,
    /// Sidecar file opening.
    SidecarOpen,
    /// Sidecar lock acquisition.
    Lock,
    /// Destination append-file opening.
    AppendOpen,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
/// One deterministic early worker-I/O failure and optional delay.
struct DebugWriterIoFault {
    /// Phase at which the worker must fail.
    stage: DebugWriterIoStage,
    /// Delay before failure, used to cross the slow-cycle threshold.
    delay: std::time::Duration,
}

/// Cached append handle owned exclusively by the worker.
struct CachedAppendFile {
    /// Path for which `file` was opened.
    path: PathBuf,
    /// Append-open JSONL file; every touch still requires the sidecar lock.
    file: std::fs::File,
}

#[cfg(not(test))]
static DEBUG_WRITER: OnceLock<DebugWriter> = OnceLock::new();

#[cfg(not(test))]
fn debug_writer() -> &'static DebugWriter {
    DEBUG_WRITER.get_or_init(DebugWriter::start)
}

/// Attempts immediate admission to the process-wide debug writer.
#[cfg(not(test))]
pub(super) fn enqueue(path: PathBuf, line: Vec<u8>) -> bool {
    debug_writer().enqueue(path, line)
}

impl DebugWriter {
    fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(MAX_RETAINED_LINES);
        let queue = Arc::new(DebugWriterQueue {
            retained_lines: AtomicUsize::new(0),
            retained_bytes: AtomicUsize::new(0),
            poisoned: AtomicBool::new(false),
            drop_episode: AtomicU64::new(0),
            io_failures: AtomicU64::new(0),
            io_warning_active: AtomicBool::new(false),
            #[cfg(test)]
            lock_attempts: AtomicUsize::new(0),
            #[cfg(test)]
            completed_jobs: AtomicUsize::new(0),
            #[cfg(test)]
            worker_traces: path_std_sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            test_pauses: DebugWriterTestPauses::default(),
        });
        let worker_queue = Arc::clone(&queue);
        let worker = match path_std_thread::Builder::new()
            .name("tau-debug-jsonl".to_owned())
            .spawn(move || debug_writer_worker(worker_queue, receiver))
        {
            Ok(worker) => Some(worker),
            Err(error) => {
                queue.poisoned.store(true, Ordering::Release);
                tracing::warn!(
                    target: "tau_harness::debug_log",
                    %error,
                    "debug JSONL worker could not start; diagnostic lines will be dropped"
                );
                None
            }
        };
        #[cfg(not(test))]
        drop(worker);
        Self {
            queue,
            sender: Some(sender),
            #[cfg(test)]
            worker,
        }
    }

    fn enqueue(&self, path: PathBuf, line: Vec<u8>) -> bool {
        let line_bytes = line.len();
        let retained_bytes = line_bytes.saturating_add(path.as_os_str().len());
        if self.queue.poisoned.load(Ordering::Acquire) {
            return false;
        }
        if MAX_RETAINED_BYTES < retained_bytes {
            self.queue.note_drop();
            return false;
        }
        if !reserve_bounded(&self.queue.retained_lines, 1, MAX_RETAINED_LINES) {
            self.queue.note_drop();
            return false;
        }
        if !reserve_bounded(
            &self.queue.retained_bytes,
            retained_bytes,
            MAX_RETAINED_BYTES,
        ) {
            self.queue.retained_lines.fetch_sub(1, Ordering::AcqRel);
            self.queue.note_drop();
            return false;
        }
        let job = DebugWriteJob {
            path,
            line,
            _permit: RetainedWorkPermit {
                queue: Arc::clone(&self.queue),
                retained_bytes,
            },
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            io_fault: None,
        };
        #[cfg(test)]
        self.queue.pause_before_send();
        let Some(sender) = self.sender.as_ref() else {
            self.queue.note_drop();
            return false;
        };
        match sender.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.queue.note_drop();
                false
            }
        }
    }

    #[cfg(test)]
    fn enqueue_fault(&self, path: PathBuf, line: Vec<u8>, fault: AppendFault) -> bool {
        self.enqueue_test_fault(path, line, Some(fault), None)
    }

    #[cfg(test)]
    fn enqueue_io_fault(&self, path: PathBuf, line: Vec<u8>, fault: DebugWriterIoFault) -> bool {
        self.enqueue_test_fault(path, line, None, Some(fault))
    }

    #[cfg(test)]
    fn enqueue_test_fault(
        &self,
        path: PathBuf,
        line: Vec<u8>,
        fault: Option<AppendFault>,
        io_fault: Option<DebugWriterIoFault>,
    ) -> bool {
        let retained_bytes = line.len().saturating_add(path.as_os_str().len());
        if self.queue.poisoned.load(Ordering::Acquire)
            || MAX_RETAINED_BYTES < retained_bytes
            || !reserve_bounded(&self.queue.retained_lines, 1, MAX_RETAINED_LINES)
        {
            return false;
        }
        if !reserve_bounded(
            &self.queue.retained_bytes,
            retained_bytes,
            MAX_RETAINED_BYTES,
        ) {
            self.queue.retained_lines.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        let job = DebugWriteJob {
            path,
            line,
            _permit: RetainedWorkPermit {
                queue: Arc::clone(&self.queue),
                retained_bytes,
            },
            fault,
            io_fault,
        };
        self.queue.pause_before_send();
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(job).is_ok())
    }
}

#[cfg(test)]
impl Drop for DebugWriter {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker.join().expect("debug writer worker panicked");
        }
    }
}

impl DebugWriterQueue {
    fn note_drop(&self) {
        let mut current = self.drop_episode.load(Ordering::Acquire);
        let (dropped, first) = loop {
            let count = (current & DROP_EPISODE_COUNT_MASK)
                .saturating_add(1)
                .min(DROP_EPISODE_COUNT_MASK);
            let next = DROP_EPISODE_ACTIVE | count;
            match self.drop_episode.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break (count, current & DROP_EPISODE_ACTIVE == 0),
                Err(observed) => current = observed,
            }
        };
        if first {
            tracing::warn!(
                target: "tau_harness::debug_log",
                dropped_lines = dropped,
                "debug JSONL queue unavailable or full; dropping diagnostic lines"
            );
        }
    }

    fn note_recovered(&self) {
        let episode = self.drop_episode.swap(0, Ordering::AcqRel);
        if episode & DROP_EPISODE_ACTIVE != 0 {
            let dropped = episode & DROP_EPISODE_COUNT_MASK;
            tracing::warn!(
                target: "tau_harness::debug_log",
                dropped_lines = dropped,
                "debug JSONL queue recovered after dropped diagnostic lines"
            );
        }
        if self.io_warning_active.swap(false, Ordering::AcqRel) {
            let omitted = self.io_failures.swap(0, Ordering::AcqRel);
            tracing::warn!(
                target: "tau_harness::debug_log",
                omitted_lines = omitted,
                "debug JSONL worker recovered after I/O failures"
            );
        }
    }

    fn note_io_failure(&self, error: &std::io::Error) {
        let omitted = increment_saturating(&self.io_failures);
        if !self.io_warning_active.swap(true, Ordering::AcqRel) {
            let diagnostic = bounded_diagnostic(error);
            tracing::warn!(
                target: "tau_harness::debug_log",
                %diagnostic,
                omitted_lines = omitted,
                "debug JSONL worker omitted lines after recoverable I/O failure"
            );
        }
    }

    #[cfg(test)]
    fn pause_before_send(&self) {
        if let Some(pause) = self
            .test_pauses
            .before_send
            .lock()
            .expect("before-send test pause mutex poisoned")
            .take()
        {
            pause.wait();
        }
    }

    #[cfg(test)]
    fn pause_after_poison_drain(&self) {
        if let Some(pause) = self
            .test_pauses
            .after_poison_drain
            .lock()
            .expect("after-poison-drain test pause mutex poisoned")
            .take()
        {
            pause.wait();
        }
    }
}

impl Drop for RetainedWorkPermit {
    fn drop(&mut self) {
        self.queue.retained_lines.fetch_sub(1, Ordering::AcqRel);
        self.queue
            .retained_bytes
            .fetch_sub(self.retained_bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
impl DebugWriterTestPause {
    fn new() -> Self {
        Self {
            entered: Arc::new(path_std_sync::Barrier::new(2)),
            resume: Arc::new(path_std_sync::Barrier::new(2)),
        }
    }

    fn wait(&self) {
        self.entered.wait();
        self.resume.wait();
    }
}

fn bounded_diagnostic(error: &std::io::Error) -> String {
    let message = error.to_string();
    let mut diagnostic = message.chars().take(DIAGNOSTIC_CHARS).collect::<String>();
    if message.chars().nth(DIAGNOSTIC_CHARS).is_some() {
        diagnostic.push('…');
    }
    diagnostic
}

fn reserve_bounded(value: &AtomicUsize, amount: usize, maximum: usize) -> bool {
    let mut current = value.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount).filter(|next| *next <= maximum) else {
            return false;
        };
        match value.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn increment_saturating(value: &AtomicU64) -> u64 {
    let mut current = value.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(1);
        match value.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

fn debug_writer_worker(queue: Arc<DebugWriterQueue>, receiver: Receiver<DebugWriteJob>) {
    let mut cached_file: Option<CachedAppendFile> = None;
    while let Ok(job) = receiver.recv() {
        let result = write_debug_job(&queue, &job, &mut cached_file);
        drop(job);
        #[cfg(test)]
        queue.completed_jobs.fetch_add(1, Ordering::Release);
        match result {
            Ok(()) => queue.note_recovered(),
            Err(error) if error.rollback.is_some() => {
                queue.poisoned.store(true, Ordering::Release);
                let diagnostic = DebugLogError::Append {
                    source: error.source,
                    rollback: error.rollback,
                }
                .bounded_diagnostic();
                tracing::warn!(
                    target: "tau_harness::debug_log",
                    %diagnostic,
                    "debug JSONL worker poisoned after uncertain rollback"
                );
                for queued in receiver.try_iter() {
                    drop(queued);
                }
                #[cfg(test)]
                queue.pause_after_poison_drain();
                break;
            }
            Err(error) => queue.note_io_failure(&error.source),
        }
    }
}

fn write_debug_job(
    queue: &DebugWriterQueue,
    job: &DebugWriteJob,
    cached_file: &mut Option<CachedAppendFile>,
) -> Result<(), LineAppendError> {
    let started = Instant::now();
    let result = (|| {
        if let Some(parent) = job.path.parent() {
            #[cfg(test)]
            maybe_fail_worker_io(job, DebugWriterIoStage::Directory)?;
            std::fs::create_dir_all(parent).map_err(LineAppendError::without_timing)?;
        }
        #[cfg(test)]
        maybe_fail_worker_io(job, DebugWriterIoStage::SidecarOpen)?;
        let lock_path = job.path.with_file_name("events.jsonl.lock");
        let lock = path_std_fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(LineAppendError::without_timing)?;
        #[cfg(test)]
        queue.lock_attempts.fetch_add(1, Ordering::Release);
        #[cfg(test)]
        maybe_fail_worker_io(job, DebugWriterIoStage::Lock)?;
        lock.lock_exclusive()
            .map_err(LineAppendError::without_timing)?;
        if cached_file
            .as_ref()
            .is_none_or(|cached| cached.path != job.path)
        {
            #[cfg(test)]
            maybe_fail_worker_io(job, DebugWriterIoStage::AppendOpen)?;
            let file = path_std_fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&job.path)
                .map_err(LineAppendError::without_timing)?;
            *cached_file = Some(CachedAppendFile {
                path: job.path.clone(),
                file,
            });
        }
        let file = &mut cached_file
            .as_mut()
            .expect("append file was initialized")
            .file;
        #[cfg(test)]
        let result = if let Some(fault) = job.fault {
            append_line(
                &mut FaultInjectingFile::new(file, fault, job.line.len()),
                &job.line,
            )
        } else {
            append_line(file, &job.line)
        };
        #[cfg(not(test))]
        let result = append_line(file, &job.line);
        drop(lock);
        result
    })();
    match result {
        Ok(timing) => {
            trace_worker_cycle(
                queue,
                &timing,
                started.elapsed(),
                job.line.len(),
                DebugLogCycleResult::Appended,
            );
            Ok(())
        }
        Err(error) => {
            trace_worker_cycle(
                queue,
                &error.timing,
                started.elapsed(),
                job.line.len(),
                DebugLogCycleResult::AppendError,
            );
            Err(error)
        }
    }
}

fn trace_worker_cycle(
    _queue: &DebugWriterQueue,
    timing: &super::LineAppendTiming,
    total: std::time::Duration,
    line_bytes: usize,
    result: DebugLogCycleResult,
) {
    let trace = timing.trace_worker(total, line_bytes, result);
    #[cfg(not(test))]
    let _ = trace;
    #[cfg(test)]
    _queue
        .worker_traces
        .lock()
        .expect("worker trace mutex poisoned")
        .push(trace);
}

#[cfg(test)]
fn maybe_fail_worker_io(
    job: &DebugWriteJob,
    stage: DebugWriterIoStage,
) -> Result<(), LineAppendError> {
    let Some(fault) = job.io_fault.filter(|fault| fault.stage == stage) else {
        return Ok(());
    };
    std::thread::sleep(fault.delay);
    Err(LineAppendError::without_timing(path_std_io::Error::other(
        "injected early worker I/O failure",
    )))
}

#[cfg(test)]
mod tests;
