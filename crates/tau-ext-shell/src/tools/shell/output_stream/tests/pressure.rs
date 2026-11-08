use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::super::*;
use crate::config::ShellConfig;
use crate::tools::ShellSurface;
use crate::tools::shell::{
    CommandOutcome, ShellCommandMode, ShellRunControl,
    run_command_live_for_surface_with_authorized_cwd,
};

/// Minimal production client runtime; no fake output admission path.
struct OutputFixture;

impl tau_client::TauExtension for OutputFixture {
    type State = ();

    fn name(&self) -> &'static str {
        "output-pressure-fixture"
    }

    fn register(self, _builder: &mut tau_client::ExtensionBuilder<()>) {}
}

/// Transport gate shared with the test controller.
#[derive(Default)]
struct Gate {
    /// Whether transport writes must wait.
    closed: Mutex<bool>,
    /// Event-driven transport resumption.
    changed: Condvar,
}

impl Gate {
    fn release(&self) {
        *self.closed.lock().expect("gate") = false;
        self.changed.notify_all();
    }
}

/// Ensure a failed assertion cannot leave the production writer blocked.
struct ReleaseGate(
    /// Gate released during normal cleanup or assertion unwinding.
    Arc<Gate>,
);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Production writer whose bytes remain inspectable after runtime shutdown.
struct GatedWriter {
    /// Shared transport backpressure.
    gate: Arc<Gate>,
    /// Complete serialized protocol output.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Signals that a transport write reached the closed gate.
    entered: mpsc::Sender<()>,
}

impl Write for GatedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut closed = self.gate.closed.lock().expect("gate");
        if *closed {
            self.entered.send(()).expect("entered gate");
        }
        while *closed {
            closed = self.gate.changed.wait(closed).expect("gate wait");
        }
        self.bytes.lock().expect("bytes").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn filler() -> HarnessInputMessage {
    HarnessInputMessage::Disconnect(tau_proto::Disconnect {
        reason: Some("capacity fixture".into()),
    })
}

/// Production publisher and a panic-safe transport gate.
struct PressureFixture {
    /// Release before dropping the runtime even during assertion unwinding.
    release: ReleaseGate,
    /// Actual client writer and lifecycle owner.
    runtime: tau_client::ManualExtensionRuntime<()>,
    /// Serialized frames for externally visible ordering assertions.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Transport entry notification.
    entered: mpsc::Receiver<()>,
}

fn pressure_fixture() -> PressureFixture {
    let gate = Arc::new(Gate::default());
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let mut input = Vec::new();
    tau_proto::HarnessOutputWriter::new(&mut input)
        .write_message(&tau_proto::HarnessOutputMessage::Configure(
            tau_proto::Configure {
                purpose: tau_proto::ConfigurePurpose::Runtime,
                artifact_limits: Default::default(),
                harness_protocol_version: None,
                tool_prefix: None,
                instance_name: tau_proto::ExtensionName::parse("output-pressure-fixture")
                    .expect("extension name"),
                config: tau_proto::CborValue::Map(Vec::new()),
                state_dir: None,
                secrets: Default::default(),
                absent_optional_secrets: Default::default(),
                settings_files: Default::default(),
            },
        ))
        .expect("configure");
    let runtime = tau_client::TauExtensionRunner::new(OutputFixture)
        .start_manual_loop(
            Cursor::new(input),
            GatedWriter {
                gate: Arc::clone(&gate),
                bytes: Arc::clone(&bytes),
                entered: entered_tx,
            },
            (),
        )
        .expect("runtime");
    PressureFixture {
        release: ReleaseGate(gate),
        runtime,
        bytes,
        entered: entered_rx,
    }
}

/// Real publisher pressure must not stop pipe draining or process cancellation.
/// After capacity resumes, the sealed source flushes exactly once, behind older
/// accepted source and before the cancelled terminal's fresh diagnostic tail.
#[test]
fn saturated_publisher_drains_and_cancels_child_before_ordered_final_flush() {
    let PressureFixture {
        release: _release,
        runtime,
        bytes,
        entered: entered_rx,
    } = pressure_fixture();
    let gate = Arc::clone(&_release.0);
    let handle = runtime.handle();
    let output = Output::client(handle.clone());
    let mut capture = OutputStreamCapture::new(
        output.clone(),
        "pressure-call".into(),
        tau_proto::ToolName::new("shell"),
    )
    .expect("capture");
    let now = Instant::now();
    capture.push(OutputStream::Stdout, b"ACCEPTED_BEFORE_PRESSURE", now);
    capture.poll(now + Duration::from_secs(15));
    handle
        .send_after_detached(filler())
        .expect("flush accepted source");
    *gate.closed.lock().expect("gate") = true;
    handle.send_detached(filler()).expect("block writer");
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("writer blocked");
    for _ in 0..64 {
        handle.send_detached(filler()).expect("fill FIFO");
    }
    capture.push(
        OutputStream::Stdout,
        b"SEALED_BEFORE_CANCEL",
        now + Duration::from_secs(16),
    );
    capture.poll(now + Duration::from_secs(31));
    assert!(
        capture.deadline().is_none(),
        "capacity exhaustion must not install a retry timer"
    );

    let directory = tempfile::tempdir().expect("directory");
    let fifo = directory.path().join("ready");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("FIFO")
            .success()
    );
    let arguments = tau_proto::CborValue::Map(vec![
        (
            tau_proto::CborValue::Text("command".into()),
            tau_proto::CborValue::Text(format!(
                "head -c 262144 /dev/zero; printf RECENT_CANCEL_DIAGNOSTIC; printf '%s' $$ > '{}'; sleep 60",
                fifo.display()
            )),
        ),
        (
            tau_proto::CborValue::Text("timeout".into()),
            tau_proto::CborValue::Integer(90.into()),
        ),
    ]);
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = run_command_live_for_surface_with_authorized_cwd(
            ShellSurface::Generic,
            &arguments,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            ShellRunControl {
                cancel_rx: Some(cancel_rx),
                output_stream: Some(capture),
            },
        )
        .expect("command");
        let CommandOutcome::Cancelled { output: Some(tail) } = result else {
            panic!("cancelled tail");
        };
        output
            .send_checked(HarnessInputMessage::emit_with_persist(
                tau_proto::Event::ToolCancelledReported(tau_proto::ToolCancelled {
                    output: Some(tail.clone()),
                    captured_output: None,
                    call_id: "pressure-call".into(),
                    tool_name: tau_proto::ToolName::new("shell"),
                    tool_type: tau_proto::ToolType::Function,
                    presentation: Default::default(),
                    display: None,
                }),
                false,
            ))
            .expect("terminal flush");
        done_tx.send(tail).expect("done");
    });
    let (killed_tx, killed_rx) = mpsc::channel();
    let controller = std::thread::spawn(move || {
        let mut pid = String::new();
        File::open(fifo)
            .expect("FIFO")
            .read_to_string(&mut pid)
            .expect("child drained more than pipe capacity");
        let pid: libc::pid_t = pid.parse().expect("pid");
        #[allow(unsafe_code)]
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(raw >= 0, "pidfd_open");
        #[allow(unsafe_code)]
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        cancel_tx.send(()).expect("cancel child under pressure");
        let mut fd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        #[allow(unsafe_code)]
        let ready = unsafe { libc::poll(&mut fd, 1, 5000) };
        killed_tx.send(ready == 1).expect("killed status");
    });
    let killed = killed_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("event-driven child exit");
    assert!(killed, "child must die while transport remains blocked");
    assert!(
        done_rx.try_recv().is_err(),
        "final flush still owns sealed output"
    );
    gate.release();
    let tail = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("resumed final flush");
    assert!(tail.contains("RECENT_CANCEL_DIAGNOSTIC"));
    assert!(!tail.contains("ACCEPTED_BEFORE_PRESSURE"));
    assert!(!tail.contains("SEALED_BEFORE_CANCEL"));
    controller.join().expect("controller");
    worker.join().expect("worker");
    runtime.finish().expect("runtime finish");
    let bytes = bytes.lock().expect("bytes").clone();
    let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
    let events: Vec<_> = std::iter::from_fn(|| reader.read_message().transpose())
        .map(|frame| frame.expect("frame"))
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => Some(*emit.event),
            _ => None,
        })
        .collect();
    let reports: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolOutputReported(report) => Some(report),
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 2);
    assert_eq!((reports[0].sequence, reports[1].sequence), (0, 1));
    assert!(reports[0].text.contains("ACCEPTED_BEFORE_PRESSURE"));
    assert!(reports[1].text.contains("SEALED_BEFORE_CANCEL"));
    assert!(matches!(
        events.last(),
        Some(Event::ToolCancelledReported(_))
    ));
}

/// A capacity edge resumes the sealed head and immediately admits an already
/// overdue pending batch, without a periodic retry or an extra cadence delay.
#[test]
fn capacity_wake_flushes_sealed_head_and_overdue_pending_batch() {
    let PressureFixture {
        release: _release,
        runtime,
        bytes,
        entered,
    } = pressure_fixture();
    let gate = Arc::clone(&_release.0);
    let handle = runtime.handle();
    let mut capture = OutputStreamCapture::new(
        Output::client(handle.clone()),
        "resume-call".into(),
        tau_proto::ToolName::new("shell"),
    )
    .expect("capture");
    *gate.closed.lock().expect("gate") = true;
    handle.send_detached(filler()).expect("blocked write");
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("writer blocked");
    for _ in 0..64 {
        handle.send_detached(filler()).expect("fill FIFO");
    }
    let now = Instant::now();
    capture.push(OutputStream::Stdout, b"IMMUTABLE_HEAD", now);
    capture.poll(now + Duration::from_secs(15));
    capture.push(
        OutputStream::Stderr,
        b"PENDING_AFTER_HEAD",
        now + Duration::from_secs(16),
    );
    capture.poll(now + Duration::from_secs(45));
    assert!(
        capture.deadline().is_none(),
        "sealed head waits for an event, not a timer"
    );
    gate.release();
    let mut fd = libc::pollfd {
        fd: capture.capacity_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    #[allow(unsafe_code)]
    let ready = unsafe { libc::poll(&mut fd, 1, 2000) };
    assert_eq!(ready, 1, "capacity release must wake the command poll loop");
    handle
        .send_after_detached(filler())
        .expect("drain existing backlog");
    capture.poll(now + Duration::from_secs(46));
    handle
        .send_after_detached(filler())
        .expect("flush resumed batches");
    assert!(capture.finish().is_empty());
    runtime.finish().expect("finish");
    let bytes = bytes.lock().expect("bytes").clone();
    let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
    let reports: Vec<_> = std::iter::from_fn(|| reader.read_message().transpose())
        .map(|frame| frame.expect("frame"))
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ToolOutputReported(report) => Some(report),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 2);
    assert_eq!((reports[0].sequence, reports[1].sequence), (0, 1));
    assert!(reports[0].text.contains("IMMUTABLE_HEAD"));
    assert!(!reports[0].text.contains("PENDING_AFTER_HEAD"));
    assert!(reports[1].text.contains("PENDING_AFTER_HEAD"));
}
