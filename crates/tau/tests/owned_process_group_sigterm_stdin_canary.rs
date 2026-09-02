use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::Builder;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{SigSet, Signal as NixSignal};
use rustix_v1::process::{Pid, Signal, kill_process};

#[path = "detach_attach_cli/sigterm_worker_action.rs"]
mod sigterm_worker_action;

use sigterm_worker_action::{SigtermCompletionWorker, complete_worker_via_sigterm};

/// Runs the parent oracle or its single-threaded controlled child.
fn main() {
    if std::env::var_os("TAU_SIGTERM_STDIN_CANARY_CHILD").is_some() {
        run_controlled_child();
    } else if std::env::args().any(|argument| argument == "--list") {
        println!("owned_process_group_sigterm_retains_worker_stdin: test");
    } else {
        run_parent_oracle();
    }
}

/// Proves the normal SIGTERM action retains stdin until SIGTERM owns terminal
/// completion.
fn run_parent_oracle() {
    let mut command = Command::new(std::env::current_exe().expect("current canary binary"));
    command.env("TAU_SIGTERM_STDIN_CANARY_CHILD", "1");
    let mut worker = CanaryWorker::spawn(command).expect("spawn controlled SIGTERM child");
    let deadline = Instant::now() + Duration::from_secs(10);
    assert_eq!(
        worker
            .recv_line_until(deadline, "READY")
            .expect("read controlled child readiness"),
        Some("READY\n".to_owned())
    );

    let completion_deadline = Instant::now() + Duration::from_secs(10);
    let completion =
        complete_worker_via_sigterm(&mut worker, completion_deadline, |worker, deadline| {
            signal_pid(worker.id(), Signal::USR2).expect("request controlled child stdin sample");
            assert_eq!(
                worker
                    .recv_line_until(deadline, "STDIN_OPEN")
                    .expect("read controlled child stdin observation"),
                Some("STDIN_OPEN\n".to_owned())
            );
            Ok(())
        })
        .expect("complete controlled child through shared SIGTERM action");
    assert_eq!(
        completion.status.signal(),
        Some(Signal::TERM.as_raw()),
        "controlled child did not terminalize via SIGTERM\n{}",
        String::from_utf8_lossy(&completion.stderr)
    );
    assert!(
        completion.stdout.is_empty(),
        "controlled child published unexpected post-observation stdout: {}",
        completion.stdout
    );
}

/// Blocks both ordered signals before readiness, samples stdin after SIGUSR2,
/// then lets pending SIGTERM terminalize the process.
fn run_controlled_child() {
    let mut blocked = SigSet::empty();
    blocked.add(NixSignal::SIGTERM);
    blocked.add(NixSignal::SIGUSR2);
    blocked
        .thread_block()
        .expect("block controlled child signals");
    println!("READY");
    std::io::stdout()
        .flush()
        .expect("flush controlled child readiness");

    let mut sample = SigSet::empty();
    sample.add(NixSignal::SIGUSR2);
    assert_eq!(
        sample.wait().expect("wait for stdin sample signal"),
        NixSignal::SIGUSR2
    );
    assert!(
        signal_is_pending(NixSignal::SIGTERM),
        "SIGUSR2 arrived without the preceding SIGTERM pending"
    );
    set_stdin_nonblocking();
    let mut release = [0_u8; 1];
    match std::io::stdin().read(&mut release) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Ok(0) => panic!("parent closed the controlled child's stdin release pipe"),
        Ok(_) => panic!("parent wrote to the controlled child's stdin release pipe"),
        Err(error) => panic!("inspect controlled child stdin release pipe: {error}"),
    }
    println!("STDIN_OPEN");
    std::io::stdout()
        .flush()
        .expect("flush controlled child stdin observation");

    let mut term = SigSet::empty();
    term.add(NixSignal::SIGTERM);
    term.thread_unblock()
        .expect("unblock pending controlled child SIGTERM");
    panic!("pending SIGTERM did not terminalize controlled child");
}

/// Reports whether one signal is pending in either the thread or shared Linux
/// process signal queue.
fn signal_is_pending(signal: NixSignal) -> bool {
    let status =
        std::fs::read_to_string("/proc/self/status").expect("read controlled child status");
    let pending = ["SigPnd:", "ShdPnd:"]
        .into_iter()
        .map(|field| {
            let value = status
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .expect("find pending-signal field")
                .trim();
            u64::from_str_radix(value, 16).expect("parse pending-signal mask")
        })
        .fold(0_u64, |combined, mask| combined | mask);
    pending & (1_u64 << (signal as u32 - 1)) != 0
}

/// Marks controlled-child stdin nonblocking for one timing-independent pipe
/// state observation.
fn set_stdin_nonblocking() {
    let flags = fcntl(0, FcntlArg::F_GETFL).expect("read controlled child stdin flags");
    let flags = OFlag::from_bits_retain(flags);
    fcntl(0, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .expect("mark controlled child stdin nonblocking");
}

/// Sends one signal to an exact positive worker PID.
fn signal_pid(pid: u32, signal: Signal) -> io::Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).expect("worker PID fits i32"))
        .expect("positive worker PID");
    kill_process(pid, signal).map_err(Into::into)
}

/// Owns the controlled child and closes/kills/reaps it only on failed parent
/// completion.
struct CanaryWorker {
    /// Direct controlled child while it remains unreaped.
    child: Option<Child>,
    /// Stdin release barrier retained through successful terminal completion.
    stdin: Option<ChildStdin>,
    /// Complete-line observations from controlled-child stdout.
    stdout: mpsc::Receiver<io::Result<Option<String>>>,
    /// Complete captured stderr delivered by its dedicated bounded reader.
    stderr: Option<mpsc::Receiver<(io::Result<usize>, Vec<u8>)>>,
}

impl CanaryWorker {
    /// Spawns the controlled child and installs bounded failure ownership.
    fn spawn(mut command: Command) -> io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let mut owner = Self {
            child: Some(child),
            stdin: None,
            stdout: mpsc::channel().1,
            stderr: None,
        };
        owner.stdin = owner
            .child
            .as_mut()
            .expect("controlled child provisionally owned")
            .stdin
            .take();
        let stdout = owner
            .child
            .as_mut()
            .expect("controlled child provisionally owned")
            .stdout
            .take()
            .expect("capture controlled child stdout");
        let stderr = owner
            .child
            .as_mut()
            .expect("controlled child provisionally owned")
            .stderr
            .take()
            .expect("capture controlled child stderr");
        let (sender, receiver) = mpsc::channel();
        Builder::new()
            .name("sigterm-stdin-canary-stdout".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    let observation = match stdout.read_line(&mut line) {
                        Ok(0) => Ok(None),
                        Ok(_) => Ok(Some(line)),
                        Err(error) => Err(error),
                    };
                    let terminal = !matches!(observation, Ok(Some(_)));
                    if sender.send(observation).is_err() || terminal {
                        break;
                    }
                }
            })?;
        owner.stdout = receiver;

        let (stderr_sender, stderr_receiver) = mpsc::channel();
        Builder::new()
            .name("sigterm-stdin-canary-stderr".into())
            .spawn(move || {
                let mut stderr = stderr;
                let mut bytes = Vec::new();
                let result = stderr.read_to_end(&mut bytes);
                let _ = stderr_sender.send((result, bytes));
            })?;
        owner.stderr = Some(stderr_receiver);
        Ok(owner)
    }

    /// Returns the exact controlled-child PID.
    fn id(&self) -> u32 {
        self.child.as_ref().expect("controlled child owned").id()
    }

    /// Receives one stdout line or EOF before the local failure deadline.
    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>> {
        self.stdout
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("controlled child missed {boundary} deadline: {error}"),
                )
            })?
    }

    /// Polls and reaps the controlled child only within the failure deadline.
    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        let child = self.child.as_mut().expect("controlled child owned");
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok(Some(status));
            }
            if deadline <= Instant::now() {
                return Ok(None);
            }
            std::thread::yield_now();
        }
    }

    /// Collects captured stderr only before the supplied local deadline.
    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        let receiver = self.stderr.as_ref().expect("controlled child stderr owned");
        let (result, bytes) = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("controlled child stderr missed deadline: {error}"),
                )
            })?;
        self.stderr.take();
        result?;
        Ok(bytes)
    }

    /// Releases stdin, kills, reaps, and drains the controlled child after a
    /// failed assertion or deadline.
    fn cleanup(&mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.as_mut() {
            let _ = signal_pid(child.id(), Signal::KILL);
            let deadline = Instant::now() + Duration::from_secs(2);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            if child.try_wait().ok().flatten().is_some() {
                self.child.take();
            }
        }
        let reader_deadline = Instant::now() + Duration::from_secs(2);
        while self
            .recv_line_until(reader_deadline, "stdout EOF during cleanup")
            .ok()
            .flatten()
            .is_some()
        {}
        if self.stderr.is_some() {
            let _ = self.collect_stderr_until(reader_deadline);
        }
    }
}

impl SigtermCompletionWorker for CanaryWorker {
    fn id(&self) -> u32 {
        self.id()
    }

    fn recv_line_until(&self, deadline: Instant, boundary: &str) -> io::Result<Option<String>> {
        self.recv_line_until(deadline, boundary)
    }

    fn wait_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        self.wait_until(deadline)
    }

    fn collect_stderr_until(&mut self, deadline: Instant) -> io::Result<Vec<u8>> {
        self.collect_stderr_until(deadline)
    }
}

impl Drop for CanaryWorker {
    fn drop(&mut self) {
        self.cleanup();
    }
}
