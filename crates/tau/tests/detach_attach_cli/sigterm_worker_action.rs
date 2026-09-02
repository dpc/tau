use std::io;
use std::process::ExitStatus;
use std::time::Instant;

use rustix_v1::process::{Pid, Signal, kill_process};

/// Pipe and process operations needed by the shared SIGTERM completion action.
pub trait SigtermCompletionWorker {
    /// Returns the exact direct worker PID.
    fn id(&self) -> u32;

    /// Receives one stdout line or EOF before the supplied deadline.
    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>>;

    /// Polls and reaps the direct worker before the supplied deadline.
    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>>;

    /// Collects complete worker stderr before the supplied deadline.
    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>>;
}

/// Terminal evidence returned by the shared SIGTERM completion action.
pub struct SigtermCompletion {
    /// Direct worker terminal status.
    pub status: ExitStatus,
    /// Worker stdout after its caller-owned readiness boundary.
    pub stdout: String,
    /// Complete worker and inherited-watchdog stderr.
    pub stderr: Vec<u8>,
}

/// Requests SIGTERM and owns every subsequent success-path operation while the
/// worker's stdin failure barrier remains retained.
pub fn complete_worker_via_sigterm<W>(
    worker: &mut W,
    completion_deadline: Instant,
    after_sigterm: impl FnOnce(&mut W, Instant) -> io::Result<()>,
) -> io::Result<SigtermCompletion>
where
    W: SigtermCompletionWorker,
{
    let pid = Pid::from_raw(i32::try_from(worker.id()).expect("worker PID fits i32"))
        .expect("positive worker PID");
    kill_process(pid, Signal::TERM).map_err(io::Error::from)?;
    after_sigterm(worker, completion_deadline)?;

    let mut stdout = String::new();
    while let Some(line) =
        worker.recv_line_until(completion_deadline, "stdout EOF after SIGTERM")?
    {
        stdout.push_str(&line);
    }
    let status = worker
        .wait_until(completion_deadline)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "reap worker after SIGTERM"))?;
    let stderr = worker.collect_stderr_until(completion_deadline)?;
    Ok(SigtermCompletion {
        status,
        stdout,
        stderr,
    })
}
