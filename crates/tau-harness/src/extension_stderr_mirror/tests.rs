use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::*;

/// Any worker-thread setup failure disables only the optional mirror.
#[test]
fn worker_spawn_failure_disables_mirror_setup() {
    let mirror = ExtensionStderrMirror::try_with_writer_and_spawner(io::sink(), 1, |_task| {
        Err(io::Error::other("injected spawn failure"))
    });
    assert!(mirror.is_none());
}

/// Inherited-stderr descriptor duplication failure disables only the mirror.
#[test]
fn stderr_duplication_failure_disables_mirror_setup() {
    assert!(ExtensionStderrMirror::from_stderr_duplicate(Err(rustix::io::Errno::BADF)).is_none());
}

/// Returns framed boundary and raw payload pairs for compact assertions.
fn frame(parts: &[&[u8]], finish: bool) -> Vec<(RecordBoundary, Vec<u8>)> {
    let mut framer = StderrFramer::default();
    let mut records = Vec::new();
    for part in parts {
        records.extend(
            framer
                .feed(part)
                .into_iter()
                .map(|record| (record.boundary, record.raw)),
        );
    }
    if finish {
        records.extend(
            framer
                .finish()
                .into_iter()
                .map(|record| (record.boundary, record.raw)),
        );
    }
    records
}

/// An incomplete scalar at a read boundary waits for lookahead, while EOF
/// treats an incomplete sequence as invalid bytes without exceeding the cap.
#[test]
fn framer_waits_for_incomplete_utf8_lookahead_and_bounds_invalid_eof() {
    let mut prefix = vec![b'a'; 4095];
    prefix.extend_from_slice(&[0xF0, 0x9F]);
    let mut framer = StderrFramer::default();
    assert!(framer.feed(&prefix).is_empty());
    let completed = framer.feed(&[0xA6, 0x80, b'\n']);
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0].boundary, RecordBoundary::Chunk);
    assert_eq!(completed[0].raw.len(), 4099);
    assert_eq!(completed[1].boundary, RecordBoundary::Line);
    assert!(completed[1].raw.is_empty());

    let mut framer = StderrFramer::default();
    assert!(framer.feed(&prefix).is_empty());
    let eof = framer.finish();
    assert_eq!(
        eof.iter()
            .map(|record| record.raw.len())
            .collect::<Vec<_>>(),
        vec![4096, 1]
    );
    assert_eq!(eof[0].boundary, RecordBoundary::Chunk);
    assert_eq!(eof[1].boundary, RecordBoundary::Eof);
}

/// An earlier LF is consumed before later incomplete UTF-8 lookahead, so EOF
/// can never place an unescaped child newline inside a rendered chunk.
#[test]
fn framer_consumes_lf_before_later_incomplete_utf8_at_eof() {
    let mut bytes = vec![b'a'; 4000];
    bytes.push(b'\n');
    bytes.extend(std::iter::repeat_n(b'b', 94));
    bytes.extend_from_slice(&[0xF0, 0x9F]);
    let records = frame(&[&bytes], true);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], (RecordBoundary::Line, vec![b'a'; 4000]));
    assert_eq!(records[1].0, RecordBoundary::Eof);
    assert_eq!(records[1].1.len(), 96);
    assert!(records.iter().all(|(_, raw)| !raw.contains(&b'\n')));
}

/// A completed scalar extended across the cap is one chunk followed by the
/// required empty EOF boundary when no LF terminates the logical line.
#[test]
fn completed_cross_cap_scalar_at_eof_keeps_final_eof_boundary() {
    let mut bytes = vec![b'a'; 4095];
    bytes.extend_from_slice("🦀".as_bytes());
    let records = frame(&[&bytes], true);
    assert_eq!(
        records,
        vec![
            (RecordBoundary::Chunk, bytes),
            (RecordBoundary::Eof, Vec::new())
        ]
    );
}

/// LF and EOF boundaries preserve exact zero and near-cap payload sizes.
#[test]
fn framer_covers_empty_and_near_cap_boundaries() {
    for size in [0, 1, 4095, 4096, 4097] {
        let payload = vec![b'a'; size];
        let mut terminated = payload.clone();
        terminated.push(b'\n');
        let records = frame(&[&terminated], true);
        if size <= 4096 {
            assert_eq!(records, vec![(RecordBoundary::Line, payload.clone())]);
        } else {
            assert_eq!(
                records,
                vec![
                    (RecordBoundary::Chunk, vec![b'a'; 4096]),
                    (RecordBoundary::Line, vec![b'a'])
                ]
            );
        }
        let records = frame(&[&payload], true);
        if size == 0 {
            assert!(records.is_empty());
        } else if size <= 4096 {
            assert_eq!(records, vec![(RecordBoundary::Eof, payload)]);
        } else {
            assert_eq!(
                records,
                vec![
                    (RecordBoundary::Chunk, vec![b'a'; 4096]),
                    (RecordBoundary::Eof, vec![b'a'])
                ]
            );
        }
    }
}

/// Every possible read split through multi-byte scalars retains valid UTF-8,
/// including a scalar that crosses the ordinary 4096-byte fragment boundary.
#[test]
fn framer_never_splits_valid_utf8() {
    let text = format!("{}é€🦀z\n", "a".repeat(4095));
    for split in 0..=text.len() {
        let records = frame(
            &[&text.as_bytes()[..split], &text.as_bytes()[split..]],
            true,
        );
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].0, RecordBoundary::Chunk);
        assert_eq!(records[1].0, RecordBoundary::Line);
        for (_, payload) in &records {
            std::str::from_utf8(payload).expect("valid input stays valid per record");
        }
        assert_eq!(
            records
                .iter()
                .flat_map(|(_, payload)| payload.iter().copied())
                .collect::<Vec<_>>(),
            &text.as_bytes()[..text.len() - 1]
        );
    }
}

/// Canonical escaping contains arbitrary bytes and direction-changing Unicode
/// in one unambiguous journal line.
#[test]
fn canonical_escaping_covers_controls_invalid_utf8_and_bidi() {
    let identity =
        ExtensionStderrIdentity::new(ExtensionName::parse("safe-ext").expect("valid name"), 7, 42);
    let rendered = render_record(
        &identity,
        FramedRecord {
            boundary: RecordBoundary::Line,
            raw: b"\\\"\t\r\x00\x1B\x7F\xC2\x80\xFF ok \xE2\x80\xA8\xD8\x9C\xE2\x80\xAE".to_vec(),
        },
    );
    assert_eq!(
        String::from_utf8(rendered).expect("render is UTF-8"),
        "tau: extension stderr: extension=safe-ext generation=7 pid=42 boundary=line message=\"\\\\\\\"\\t\\r\\x00\\x1B\\x7F\\xC2\\x80\\xFF ok \\u{2028}\\u{61C}\\u{202E}\"\n"
    );
}

/// The single worker performs one write operation per complete rendered record,
/// preventing byte interleaving across extension loggers.
#[test]
fn worker_serializes_whole_records() {
    #[derive(Clone)]
    struct Writes {
        output: Arc<Mutex<Vec<Vec<u8>>>>,
        completed: mpsc::Sender<()>,
    }
    impl Write for Writes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output
                .lock()
                .expect("writes lock")
                .push(bytes.to_vec());
            self.completed.send(()).expect("announce complete write");
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (completed_tx, completed_rx) = mpsc::channel();
    let writes = Writes {
        output: observed.clone(),
        completed: completed_tx,
    };
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(writes, 8);
    let mut a = mirror.logger(ExtensionStderrIdentity::new(
        ExtensionName::parse("a").expect("valid"),
        0,
        10,
    ));
    let mut b = mirror.logger(ExtensionStderrIdentity::new(
        ExtensionName::parse("b").expect("valid"),
        1,
        11,
    ));
    a.feed(b"one\n");
    b.feed(b"two\n");
    drop(a);
    drop(b);
    drop(mirror);
    for _ in 0..2 {
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker completed record");
    }
    let writes = observed.lock().expect("writes lock");
    assert_eq!(writes.len(), 2);
    assert!(writes.iter().all(|write| write.ends_with(b"\n")));
}

/// Old and new children with the same validated extension name retain distinct
/// immutable generation and PID attribution while their logger lifetimes
/// overlap.
#[test]
fn same_name_overlapping_generations_keep_distinct_attribution() {
    #[derive(Clone)]
    struct Output {
        bytes: Arc<Mutex<Vec<u8>>>,
        completed: mpsc::Sender<()>,
    }
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .expect("output lock")
                .extend_from_slice(bytes);
            self.completed.send(()).expect("announce complete write");
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    let (completed_tx, completed_rx) = mpsc::channel();
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(
        Output {
            bytes: output.clone(),
            completed: completed_tx,
        },
        8,
    );
    let name = ExtensionName::parse("overlap").expect("valid name");
    let mut old = mirror.logger(ExtensionStderrIdentity::new(name.clone(), 2, 40));
    let mut new = mirror.logger(ExtensionStderrIdentity::new(name, 3, 41));
    old.feed(b"old-start\n");
    new.feed(b"new\n");
    old.feed(b"old-end\n");
    drop(old);
    drop(new);
    drop(mirror);
    for _ in 0..3 {
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("overlap record completed");
    }
    let output = String::from_utf8(output.lock().expect("output lock").clone()).expect("UTF-8");
    assert!(output.contains("extension=overlap generation=2 pid=40"));
    assert!(output.contains("extension=overlap generation=3 pid=41"));
    assert!(
        output.find("old-start").expect("old start") < output.find("old-end").expect("old end"),
        "per-generation order changed"
    );
}

/// A full queue drops mirror-only records, keeps exact per-logger counters, and
/// emits one notice before the first later content admitted after capacity
/// returns.
#[test]
fn queue_saturation_reports_exact_loss_without_blocking_admission() {
    struct GateWriter {
        entered: mpsc::SyncSender<()>,
        wrote: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        output: Arc<Mutex<Vec<u8>>>,
        first: bool,
    }
    impl Write for GateWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                self.entered.send(()).expect("announce blocked write");
                self.release.recv().expect("release blocked write");
            }
            self.output
                .lock()
                .expect("output lock")
                .extend_from_slice(bytes);
            self.wrote.send(()).expect("announce completed write");
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (wrote_tx, wrote_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let output = Arc::new(Mutex::new(Vec::new()));
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(
        GateWriter {
            entered: entered_tx,
            wrote: wrote_tx,
            release: release_rx,
            output: output.clone(),
            first: true,
        },
        1,
    );
    let mut logger = mirror.logger(ExtensionStderrIdentity::new(
        ExtensionName::parse("blocked").expect("valid"),
        3,
        99,
    ));
    logger.feed(b"first\n");
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker reached sink");
    logger.feed(b"queued\n");
    logger.feed(b"drop-a\n");
    logger.feed(b"drop-bb\n");
    assert_eq!(logger.dropped_records, 2);
    assert_eq!(logger.dropped_raw_bytes, 15);
    release_tx.send(()).expect("release worker");
    wrote_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first record written");
    wrote_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("queued record written");
    logger.feed(b"later\n");
    wrote_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("drop notice and later content written");
    drop(logger);
    drop(mirror);
    let output = String::from_utf8(output.lock().expect("output lock").clone()).expect("UTF-8");
    assert!(output.contains("boundary=dropped message=\"records=2 raw_bytes=15\"\n"));
    assert!(
        output.find("boundary=dropped").expect("drop notice")
            < output.find("message=\"later\"").expect("later content")
    );
}

/// One sink failure disables the shared mirror for every logger without
/// converting subsequent admissions into blocking work.
#[test]
fn sink_failure_disables_process_wide_mirror() {
    struct FailWriter {
        attempted: mpsc::SyncSender<()>,
        failed: mpsc::Sender<()>,
    }
    impl Write for FailWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            self.attempted.send(()).expect("announce failure");
            Err(io::Error::other("injected"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Drop for FailWriter {
        fn drop(&mut self) {
            let _ = self.failed.send(());
        }
    }
    let (attempted_tx, attempted_rx) = mpsc::sync_channel(0);
    let (failed_tx, failed_rx) = mpsc::channel();
    let mirror = ExtensionStderrMirror::with_writer_and_capacity(
        FailWriter {
            attempted: attempted_tx,
            failed: failed_tx,
        },
        2,
    );
    let identity =
        |name| ExtensionStderrIdentity::new(ExtensionName::parse(name).expect("valid"), 0, 1);
    let mut first = mirror.logger(identity("first"));
    let mut second = mirror.logger(identity("second"));
    first.feed(b"fail\n");
    attempted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("sink attempted");
    failed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker disabled mirror and exited");
    second.feed(b"ignored\n");
    assert!(!second.enabled);
}
