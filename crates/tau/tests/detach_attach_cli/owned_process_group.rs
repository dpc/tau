use std::ffi::OsString;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix_v1::io::Errno;
use rustix_v1::process::{
    Pid, PidfdFlags, Signal, kill_process, kill_process_group, pidfd_open, pidfd_send_signal,
    test_kill_process_group,
};

/// Owns one test-only process group and its parent-liveness watchdog.
pub(crate) struct OwnedProcessGroup {
    /// Process-group identifier, equal to the direct Tau child's PID.
    pgid: u32,
    /// Exact temporary root removed before normal watchdog disarm.
    temp_root: PathBuf,
    /// Direct Tau child, retained so normal and unwind cleanup reap it.
    child: Option<Child>,
    /// Reaped direct-child status retained after explicit termination.
    child_status: Option<ExitStatus>,
    /// Writer whose EOF tells the watchdog that the test process died.
    liveness: Option<UnixStream>,
    /// Watchdog outside the Tau group, retained for normal-path reaping.
    watchdog: Option<Child>,
    /// Whether the Tau group is extinct and its direct child is reaped.
    group_cleaned: bool,
    /// Whether the watchdog committed root-only cleanup before PGID release.
    watchdog_root_only: bool,
    /// Whether root removal and watchdog teardown already completed.
    finalized: bool,
}

impl OwnedProcessGroup {
    /// Starts a fully specified command in a dedicated process group, then arms
    /// an external watchdog that kills only that group if this process dies.
    pub(crate) fn spawn_piped_stderr(command: &Command, temp_root: &Path) -> io::Result<Self> {
        let program = command.get_program().to_owned();
        let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
        let environment = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsString::from)))
            .collect::<Vec<_>>();
        let current_dir = command.get_current_dir().map(Path::to_owned);
        let watchdog_exe = std::env::current_exe()?;
        let (mut arm_writer, arm_reader) = UnixStream::pair()?;
        let (liveness_writer, liveness_reader) = UnixStream::pair()?;

        let mut launcher = Command::new("/bin/sh");
        launcher
            .env_clear()
            .envs(
                environment
                    .iter()
                    .filter_map(|(key, value)| value.as_ref().map(|value| (key, value))),
            )
            .args([
                "-c",
                "IFS= read -r arm || exit 125\n\
                 [ \"$arm\" = arm ] || exit 126\n\
                 exec \"$@\" </dev/null",
                "tau-owned-process-group-launcher",
            ])
            .arg(&program)
            .args(&args)
            .stdin(Stdio::from(OwnedFd::from(arm_reader)))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(current_dir) = current_dir {
            launcher.current_dir(current_dir);
        }
        let child = launcher.spawn()?;
        let pgid = child.id();

        let mut watchdog = Command::new(watchdog_exe);
        watchdog
            .args([
                "--exact",
                "owned_process_group_watchdog_worker",
                "--nocapture",
            ])
            .env("TAU_OWNED_PROCESS_GROUP_WATCHDOG_PGID", pgid.to_string())
            .env("TAU_OWNED_PROCESS_GROUP_WATCHDOG_ROOT", temp_root)
            .stdin(Stdio::from(OwnedFd::from(liveness_reader)))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0);
        let mut watchdog = match watchdog.spawn() {
            Ok(watchdog) => watchdog,
            Err(error) => {
                drop(arm_writer);
                let mut child = child;
                reap_or_kill_spawn_failure(&mut child, pgid);
                return Err(error);
            }
        };
        let watchdog_stdout = watchdog.stdout.take().expect("capture watchdog readiness");
        let (watchdog_ready_tx, watchdog_ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(watchdog_stdout);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) if line == b"WATCHDOG_READY\n" => {
                        let _ = watchdog_ready_tx.send(Ok(()));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = watchdog_ready_tx.send(Err(error));
                        return;
                    }
                }
            }
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        });
        match watchdog_ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                drop(arm_writer);
                drop(liveness_writer);
                let mut child = child;
                reap_or_kill_spawn_failure(&mut child, pgid);
                reap_or_kill_watchdog(&mut watchdog);
                return Err(error);
            }
            Err(error) => {
                drop(arm_writer);
                drop(liveness_writer);
                let mut child = child;
                reap_or_kill_spawn_failure(&mut child, pgid);
                reap_or_kill_watchdog(&mut watchdog);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("watchdog did not initialize before launcher arm: {error}"),
                ));
            }
        }

        if let Err(error) = arm_writer.write_all(b"arm\n") {
            drop(arm_writer);
            drop(liveness_writer);
            let mut child = child;
            let mut watchdog = watchdog;
            reap_or_kill_spawn_failure(&mut child, pgid);
            reap_or_kill_watchdog(&mut watchdog);
            return Err(error);
        }
        drop(arm_writer);

        Ok(Self {
            pgid,
            temp_root: temp_root.to_owned(),
            child: Some(child),
            child_status: None,
            liveness: Some(liveness_writer),
            watchdog: Some(watchdog),
            group_cleaned: false,
            watchdog_root_only: false,
            finalized: false,
        })
    }

    /// Returns the owned Tau process-group identifier.
    pub(crate) fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Returns the watchdog PID used by external-cancellation cleanup.
    pub(crate) fn watchdog_pid(&self) -> u32 {
        self.watchdog.as_ref().expect("watchdog remains owned").id()
    }

    /// Takes the direct Tau child's piped stderr.
    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    /// Publishes the exact fixture PIDs that the external watchdog must observe
    /// reaped before it reports cleanup completion.
    pub(crate) fn track_pids(
        &mut self,
        identities: impl IntoIterator<Item = (u32, u64)>,
    ) -> io::Result<()> {
        let mut message = String::from("track");
        for (pid, start_time) in identities {
            message.push(' ');
            message.push_str(&pid.to_string());
            message.push(':');
            message.push_str(&start_time.to_string());
        }
        message.push('\n');
        self.liveness
            .as_mut()
            .expect("watchdog liveness writer remains owned")
            .write_all(message.as_bytes())
    }

    /// Publishes one deliberately incomplete tracking frame for the
    /// cancellation-safety oracle.
    pub(crate) fn publish_incomplete_track_frame(&mut self) -> io::Result<()> {
        self.liveness
            .as_mut()
            .expect("watchdog liveness writer remains owned")
            .write_all(b"track 1:")
    }

    /// Terminates the owned group and reaps its direct child while retaining
    /// the armed watchdog until owner drop removes the exact root.
    pub(crate) fn terminate(&mut self) -> io::Result<ExitStatus> {
        self.cleanup_group(true)
    }

    /// Performs bounded group cleanup while retaining root/watchdog ownership.
    fn cleanup_group(&mut self, strict: bool) -> io::Result<ExitStatus> {
        if self.group_cleaned {
            return self
                .child_status
                .ok_or_else(|| io::Error::other("cleaned process group has no child status"));
        }
        let mut first_error: Option<io::Error> = None;
        if let Err(error) = signal_group(self.pgid, Signal::TERM)
            && error != Errno::SRCH
        {
            first_error = Some(error.into());
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reap_deadline = deadline;
        while !owned_group_ready_to_reap(self.pgid)? {
            if deadline <= Instant::now() {
                if let Err(error) = signal_group(self.pgid, Signal::KILL)
                    && error != Errno::SRCH
                    && first_error.is_none()
                {
                    first_error = Some(error.into());
                }
                reap_deadline = Instant::now() + Duration::from_secs(2);
                break;
            }
            std::thread::yield_now();
        }

        if !owned_group_ready_to_reap(self.pgid)? {
            while !owned_group_ready_to_reap(self.pgid)? && Instant::now() < reap_deadline {
                std::thread::yield_now();
            }
            if !owned_group_ready_to_reap(self.pgid)? && first_error.is_none() {
                first_error = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("owned process group {} survived SIGKILL", self.pgid),
                ));
            }
        }

        let group_ready_to_reap = owned_group_ready_to_reap(self.pgid)?;
        if group_ready_to_reap {
            let root_only_result = self
                .liveness
                .as_mut()
                .expect("watchdog liveness writer remains owned")
                .write_all(b"root-only\n");
            match root_only_result {
                Ok(()) => self.watchdog_root_only = true,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    drop(self.liveness.take());
                    if let Some(mut watchdog) = self.watchdog.take() {
                        reap_or_kill_watchdog(&mut watchdog);
                    }
                }
            }
            if self.watchdog_root_only || self.watchdog.is_none() {
                let status = wait_child_until(
                    self.child.as_mut().expect("direct child remains owned"),
                    reap_deadline,
                )?;
                if let Some(status) = status {
                    self.child.take();
                    self.child_status = Some(status);
                } else if first_error.is_none() {
                    first_error = Some(io::Error::other(
                        "zombie group leader was not reaped before cleanup deadline",
                    ));
                }
            }
        }
        self.group_cleaned = group_ready_to_reap && self.child_status.is_some();

        if strict && let Some(error) = first_error {
            return Err(error);
        }
        self.child_status
            .ok_or_else(|| io::Error::other("owned process group child was not reaped"))
    }

    /// Removes the exact root before disarming and boundedly reaping the
    /// watchdog.
    fn finalize(&mut self) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        let mut first_error = self.cleanup_group(false).err();
        let mut watchdog_is_delegated = !self.group_cleaned;
        if self.group_cleaned {
            let root_removed = match std::fs::remove_dir_all(&self.temp_root) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
            if root_removed {
                if let Some(mut liveness) = self.liveness.take()
                    && let Err(error) = liveness.write_all(b"disarm\n")
                {
                    watchdog_is_delegated = true;
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            } else {
                watchdog_is_delegated = true;
                drop(self.liveness.take());
            }
        } else {
            drop(self.liveness.take());
        }
        if let Some(mut watchdog) = self.watchdog.take()
            && let Err(error) = reap_watchdog_bounded(&mut watchdog, watchdog_is_delegated)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.finalized = true;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for OwnedProcessGroup {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = self.finalize();
        }
    }
}

/// Reaps one child only while the real deadline remains.
fn wait_child_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    poll_until(deadline, || child.try_wait())
}

/// Polls one bounded completion condition without treating a transient miss as
/// terminal.
fn poll_until<T>(
    deadline: Instant,
    mut poll: impl FnMut() -> io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    loop {
        if let Some(value) = poll()? {
            return Ok(Some(value));
        }
        if deadline <= Instant::now() {
            return Ok(None);
        }
        std::thread::yield_now();
    }
}

/// Bounds launcher cleanup when construction fails before an owner exists.
fn reap_or_kill_spawn_failure(child: &mut Child, pgid: u32) {
    if wait_child_until(child, Instant::now() + Duration::from_secs(2))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let _ = signal_group(pgid, Signal::KILL);
    let _ = wait_child_until(child, Instant::now() + Duration::from_secs(2));
}

/// Bounds watchdog cleanup when launcher arming fails.
fn reap_or_kill_watchdog(watchdog: &mut Child) {
    if wait_child_until(watchdog, Instant::now() + Duration::from_secs(2))
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    if let Some(pid) = Pid::from_raw(i32::try_from(watchdog.id()).unwrap_or(i32::MAX)) {
        let _ = kill_process(pid, Signal::KILL);
    }
    let _ = wait_child_until(watchdog, Instant::now() + Duration::from_secs(2));
}

/// Reaps the watchdog within either its disarm or delegated-cleanup deadline,
/// escalating only its exact PID when needed.
fn reap_watchdog_bounded(watchdog: &mut Child, delegated_cleanup: bool) -> io::Result<()> {
    let grace = if delegated_cleanup {
        Duration::from_secs(12)
    } else {
        Duration::from_secs(2)
    };
    if let Some(status) = wait_child_until(watchdog, Instant::now() + grace)? {
        return if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "owned process watchdog failed: {status}"
            )))
        };
    }
    let pid =
        Pid::from_raw(i32::try_from(watchdog.id()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "watchdog PID exceeds i32")
        })?)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "watchdog PID must be positive")
        })?;
    match kill_process(pid, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => {}
        Err(error) => return Err(error.into()),
    }
    match wait_child_until(watchdog, Instant::now() + Duration::from_secs(2))? {
        Some(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "watchdog required SIGKILL after disarm",
        )),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "watchdog was not reaped after SIGKILL",
        )),
    }
}

/// Runs the hidden external watchdog worker when its exact environment is set.
pub(crate) fn run_watchdog_worker_from_env() {
    let Ok(pgid) = std::env::var("TAU_OWNED_PROCESS_GROUP_WATCHDOG_PGID") else {
        return;
    };
    let temp_root =
        std::env::var_os("TAU_OWNED_PROCESS_GROUP_WATCHDOG_ROOT").expect("watchdog temporary root");
    let pgid = pgid.parse::<u32>().expect("watchdog numeric PGID");
    let leader = rustix_pid(pgid).expect("watchdog valid leader PID");
    let leader_pidfd = pidfd_open(leader, PidfdFlags::empty()).expect("open watchdog leader pidfd");
    println!("WATCHDOG_READY");
    io::stdout().flush().expect("flush watchdog readiness");
    watchdog_cleanup(
        pgid,
        Path::new(&temp_root),
        &leader_pidfd,
        io::stdin().lock(),
        WatchdogWait {
            group_exit: true,
            tracked_exit: true,
        },
        |_| Ok(()),
        || Ok(()),
    )
    .expect("watchdog exact cleanup");
}

/// Selects the bounded completion evidence a watchdog invocation must observe.
struct WatchdogWait {
    /// Require absence of the still-identity-safe process group.
    group_exit: bool,
    /// Require stable absence of every committed PID/start identity.
    tracked_exit: bool,
}

/// Applies committed watchdog control frames and performs only identity-safe
/// group signaling before exact root removal.
fn watchdog_cleanup(
    pgid: u32,
    temp_root: &Path,
    leader_pidfd: &OwnedFd,
    mut input: impl io::BufRead,
    wait: WatchdogWait,
    mut on_leader_stopped: impl FnMut(u32) -> io::Result<()>,
    mut on_identity_wait_started: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut tracked = Vec::new();
    let mut disarmed = false;
    let mut root_only = false;
    loop {
        let mut frame = Vec::new();
        let read = input
            .read_until(b'\n', &mut frame)
            .expect("read watchdog control frame");
        if read == 0 {
            break;
        }
        if !frame.ends_with(b"\n") {
            break;
        }
        frame.pop();
        let Ok(line) = std::str::from_utf8(&frame) else {
            continue;
        };
        if line == "disarm" {
            disarmed = true;
        } else if line == "root-only" {
            root_only = true;
        } else if let Some(identities) = line.strip_prefix("track ") {
            let parsed = identities
                .split_ascii_whitespace()
                .map(|identity| {
                    let (pid, start_time) = identity.split_once(':')?;
                    Some((pid.parse::<u32>().ok()?, start_time.parse::<u64>().ok()?))
                })
                .collect::<Option<Vec<_>>>();
            if let Some(parsed) = parsed {
                tracked = parsed;
            }
        }
    }
    if disarmed {
        return Ok(());
    }

    let mut signaled_group = false;
    if !root_only {
        let anchor = tracked
            .iter()
            .find_map(|&(pid, start_time)| (pid == pgid).then_some(start_time));
        let anchor_matches =
            anchor.is_none_or(|start_time| process_start_time(pgid) == Some(start_time));
        if anchor_matches {
            match pidfd_send_signal(leader_pidfd, Signal::STOP) {
                Ok(()) => {
                    let stop_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        match process_state_and_start_time(pgid) {
                            Some((state, start_time))
                                if matches!(state, "T" | "t")
                                    && anchor.is_none_or(|anchor| anchor == start_time) =>
                            {
                                on_leader_stopped(pgid)?;
                                match signal_group(pgid, Signal::KILL) {
                                    Ok(()) | Err(Errno::SRCH) => signaled_group = true,
                                    Err(error) => return Err(error.into()),
                                }
                                break;
                            }
                            Some((_, start_time))
                                if anchor.is_some_and(|anchor| anchor != start_time) =>
                            {
                                let _ = pidfd_send_signal(leader_pidfd, Signal::CONT);
                                break;
                            }
                            None => break,
                            Some(_) if Instant::now() < stop_deadline => {
                                std::thread::yield_now();
                            }
                            Some(_) => {
                                let _ = pidfd_send_signal(leader_pidfd, Signal::CONT);
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    format!("owned group leader {pgid} did not enter SIGSTOP"),
                                ));
                            }
                        }
                    }
                }
                Err(Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    let require_group_absence = signaled_group && wait.group_exit;
    let require_identity_absence = wait.tracked_exit && (signaled_group || root_only);
    if require_group_absence || require_identity_absence {
        on_identity_wait_started()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut gone_since = None;
        loop {
            let group_gone = if require_group_absence {
                !group_exists(pgid)?
            } else {
                true
            };
            let identities_gone = !require_identity_absence
                || tracked
                    .iter()
                    .all(|&(pid, start_time)| process_start_time(pid) != Some(start_time));
            if group_gone && identities_gone {
                let gone_since = gone_since.get_or_insert_with(Instant::now);
                if gone_since.elapsed() >= Duration::from_millis(10) {
                    break;
                }
            } else {
                gone_since = None;
            }
            if deadline <= Instant::now() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("owned group {pgid} or tracked members survived watchdog cleanup"),
                ));
            }
            std::thread::yield_now();
        }
    }

    match std::fs::remove_dir_all(temp_root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Proves the watchdog refuses to signal a live group whose leader identity
/// does not match the committed owned identity.
pub(crate) fn watchdog_rejects_mismatched_anchor(temp_root: &Path) -> io::Result<bool> {
    let mut canary = BoundedCanary::spawn()?;
    let result = (|| {
        let pgid = canary.pgid();
        let leader = rustix_pid(pgid)?;
        let start_time = process_start_time(pgid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "canary identity"))?;
        let leader_pidfd = pidfd_open(leader, PidfdFlags::empty())?;
        let control = format!("track {pgid}:{}\n", start_time.saturating_add(1));
        watchdog_cleanup(
            pgid,
            temp_root,
            &leader_pidfd,
            io::Cursor::new(control.into_bytes()),
            WatchdogWait {
                group_exit: false,
                tracked_exit: false,
            },
            |_| Ok(()),
            || Ok(()),
        )?;
        canary.is_running()
    })();
    let cleanup = canary.cleanup();
    cleanup?;
    result
}

/// Proves the watchdog observes the exact matching leader stopped before it
/// uses the numeric process-group signal.
pub(crate) fn watchdog_confirms_matching_anchor_stopped(temp_root: &Path) -> io::Result<bool> {
    let mut canary = BoundedCanary::spawn()?;
    let result = (|| {
        let pgid = canary.pgid();
        let leader = rustix_pid(pgid)?;
        let start_time = process_start_time(pgid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "canary identity"))?;
        let leader_pidfd = pidfd_open(leader, PidfdFlags::empty())?;
        let control = format!("track {pgid}:{start_time}\n");
        let mut observed_stopped_match = false;
        watchdog_cleanup(
            pgid,
            temp_root,
            &leader_pidfd,
            io::Cursor::new(control.into_bytes()),
            WatchdogWait {
                group_exit: false,
                tracked_exit: false,
            },
            |stopped_pid| {
                observed_stopped_match = process_state_and_start_time(stopped_pid).is_some_and(
                    |(state, observed_start)| {
                        matches!(state, "T" | "t") && observed_start == start_time
                    },
                );
                Ok(())
            },
            || Ok(()),
        )?;
        Ok(observed_stopped_match)
    })();
    let cleanup = canary.cleanup();
    cleanup?;
    result
}

/// Proves one transient non-ready reap poll is retried within the existing
/// deadline rather than converted into an immediate lifecycle failure.
pub(crate) fn child_reap_poll_survives_transient_none() -> io::Result<bool> {
    let mut polls = 0_u8;
    let result = poll_until(Instant::now() + Duration::from_secs(2), || {
        polls += 1;
        Ok((1 < polls).then_some(()))
    })?;
    Ok(result.is_some() && polls == 2)
}

/// Proves root-only cleanup waits for exact tracked identity disappearance
/// without signaling or querying the now-stale numeric process group.
pub(crate) fn watchdog_waits_for_root_only_tracked_identity(temp_root: &Path) -> io::Result<bool> {
    let mut canary = BoundedCanary::spawn()?;
    let result = (|| {
        let pgid = canary.pgid();
        let leader = rustix_pid(pgid)?;
        let start_time = process_start_time(pgid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "canary identity"))?;
        let leader_pidfd = pidfd_open(leader, PidfdFlags::empty())?;
        let control = format!("track {pgid}:{start_time}\nroot-only\n");
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (done_tx, done_rx) = mpsc::sync_channel(1);

        std::thread::scope(|scope| -> io::Result<bool> {
            scope.spawn(|| {
                let result = watchdog_cleanup(
                    pgid,
                    temp_root,
                    &leader_pidfd,
                    io::Cursor::new(control.into_bytes()),
                    WatchdogWait {
                        group_exit: true,
                        tracked_exit: true,
                    },
                    |_| Ok(()),
                    || {
                        entered_tx.send(()).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "root-only wait barrier receiver closed",
                            )
                        })
                    },
                );
                let _ = done_tx.send(result);
            });

            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("watchdog did not enter root-only identity wait: {error}"),
                    )
                })?;
            let canary_survived = canary.is_running();
            let early_result = match done_rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "root-only watchdog result sender disconnected",
                ))),
            };
            let cleanup = canary.cleanup();
            let returned_early = early_result.is_some();
            let watchdog_result = match early_result {
                Some(result) => result,
                None => done_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("watchdog did not finish after tracked identity exit: {error}"),
                        )
                    })?,
            };
            canary_survived?;
            cleanup?;
            watchdog_result?;
            Ok(!returned_early && !temp_root.exists())
        })
    })();
    let cleanup = canary.cleanup();
    cleanup?;
    result
}

/// One process-group canary whose Drop always performs exact bounded cleanup.
struct BoundedCanary {
    /// Dedicated canary process-group identifier.
    pgid: u32,
    /// Direct canary child retained for bounded reaping.
    child: Option<Child>,
}

impl BoundedCanary {
    /// Starts one shell blocked on stdin in its own process group.
    fn spawn() -> io::Result<Self> {
        let child = Command::new("/bin/sh")
            .args(["-c", "IFS= read -r release"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(Self {
            pgid: child.id(),
            child: Some(child),
        })
    }

    /// Returns the dedicated canary process-group identifier.
    fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Reports whether the direct canary child remains alive.
    fn is_running(&mut self) -> io::Result<bool> {
        Ok(self
            .child
            .as_mut()
            .expect("canary child remains owned")
            .try_wait()?
            .is_none())
    }

    /// Kills only the canary group and boundedly reaps its direct child.
    fn cleanup(&mut self) -> io::Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        match signal_group(self.pgid, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
        let status = wait_child_until(
            self.child.as_mut().expect("canary child remains owned"),
            Instant::now() + Duration::from_secs(2),
        )?;
        if status.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "canary child was not reaped after SIGKILL",
            ));
        }
        self.child.take();
        Ok(())
    }
}

impl Drop for BoundedCanary {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Reads one Linux process start time, rejecting PID reuse after cleanup.
fn process_start_time(pid: u32) -> Option<u64> {
    process_state_and_start_time(pid).map(|(_, start_time)| start_time)
}

/// Reads one Linux process state and start time.
fn process_state_and_start_time(pid: u32) -> Option<(&'static str, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let mut fields = after_name.split_ascii_whitespace();
    let state = match fields.next()? {
        "T" => "T",
        "t" => "t",
        _ => "other",
    };
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

/// Reports when the unreaped group leader is a zombie and no descendant still
/// occupies its reserved process group.
fn owned_group_ready_to_reap(pgid: u32) -> io::Result<bool> {
    let leader_is_zombie = process_state_and_group(pgid)
        .is_some_and(|(state, process_group)| state == "Z" && process_group == pgid);
    if !leader_is_zombie {
        return Ok(false);
    }
    let has_other_member = std::fs::read_dir("/proc")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != pgid)
        .filter_map(process_state_and_group)
        .any(|(_, process_group)| process_group == pgid);
    Ok(!has_other_member)
}

/// Reads one Linux process state and process-group identifier.
fn process_state_and_group(pid: u32) -> Option<(&'static str, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let mut fields = after_name.split_ascii_whitespace();
    let state = match fields.next()? {
        "Z" => "Z",
        _ => "other",
    };
    fields.next()?;
    let process_group = fields.next()?.parse().ok()?;
    Some((state, process_group))
}

/// Signals exactly one owned process group.
fn signal_group(pgid: u32, signal: Signal) -> rustix_v1::io::Result<()> {
    let pgid = rustix_pid(pgid)?;
    kill_process_group(pgid, signal)
}

/// Converts one positive Rust child identifier into rustix's PID type.
fn rustix_pid(pid: u32) -> rustix_v1::io::Result<Pid> {
    Pid::from_raw(i32::try_from(pid).map_err(|_| Errno::INVAL)?).ok_or(Errno::INVAL)
}

/// Reports whether any process still occupies one process group.
pub(crate) fn group_exists(pgid: u32) -> io::Result<bool> {
    let pgid = Pid::from_raw(
        i32::try_from(pgid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PGID exceeds i32"))?,
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "PGID must be positive"))?;
    match test_kill_process_group(pgid) {
        Ok(()) => Ok(true),
        Err(Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}
