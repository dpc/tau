use std::time as path_std_time;
#[cfg(unix)]
use std::{fs::File, process::Command};

use super::*;
use crate::tools as path_crate_tools;

fn shell_args(command: &str, timeout: i64) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command.to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(timeout.into()),
        ),
    ])
}

/// Keeps the omitted model-shell timeout aligned with its advertised default
/// without waiting for a real command to time out.
#[test]
fn omitted_timeout_uses_default_and_explicit_timeout_overrides_it() {
    let omitted = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf ok".to_owned()),
    )]);

    assert_eq!(
        parse_timeout(&omitted),
        Ok(path_std_time::Duration::from_secs(300))
    );
    assert_eq!(
        parse_timeout(&shell_args("printf ok", 17)),
        Ok(path_std_time::Duration::from_secs(17))
    );
}

fn output_text(result: &CborValue) -> &str {
    let CborValue::Map(entries) = result else {
        panic!("expected result map");
    };
    entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value)) if key == "output" => {
                Some(value.as_str())
            }
            _ => None,
        })
        .expect("output field")
}

/// Cancellation before the first flush keeps available pipe diagnostics, and
/// cancellation after an accepted batch keeps only the new tail.
#[cfg(unix)]
#[test]
fn cancellation_retains_available_tail_without_repeating_accepted_batches() {
    use std::io::Read;

    for earlier_batch in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let fifo = directory.path().join("ready");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("create readiness FIFO")
                .success()
        );
        let (output_tx, output_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let mut stream = OutputStreamCapture::new(
            crate::Output::channel(output_tx),
            "cancel-tail".into(),
            tau_proto::ToolName::new("shell"),
        )
        .expect("stream");
        if earlier_batch {
            let now = path_std_time::Instant::now();
            stream.push(OutputStream::Stdout, b"EARLIER_ACCEPTED", now);
            stream.poll(now + path_std_time::Duration::from_secs(15));
            output_rx
                .recv_timeout(path_std_time::Duration::from_secs(1))
                .expect("accepted batch");
        }
        let arguments = shell_args(
            &format!(
                "printf FINAL_CANCEL_DIAGNOSTIC; printf R > '{}'; sleep 60",
                fifo.display()
            ),
            90,
        );
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = run_command_live_for_surface_with_authorized_cwd(
                path_crate_tools::ShellSurface::Generic,
                &arguments,
                &ShellConfig::default(),
                ShellCommandMode::READ_WRITE_HIDDEN,
                false,
                None,
                ShellRunControl {
                    cancel_rx: Some(cancel_rx),
                    output_stream: Some(stream),
                },
            );
            done_tx.send(result).expect("outcome");
        });
        let controller = std::thread::spawn(move || {
            let mut ready = File::open(fifo).expect("readiness FIFO");
            ready
                .read_exact(&mut [0u8])
                .expect("child wrote its diagnostics");
            cancel_tx.send(()).expect("cancel");
        });
        let outcome = done_rx
            .recv_timeout(path_std_time::Duration::from_secs(10))
            .expect("prompt cancellation")
            .expect("shell");
        controller.join().expect("controller");
        worker.join().expect("worker");
        let CommandOutcome::Cancelled { output: Some(tail) } = outcome else {
            panic!("expected cancellation with available tail");
        };
        assert!(tail.contains("FINAL_CANCEL_DIAGNOSTIC"));
        assert!(!tail.contains("EARLIER_ACCEPTED"));
        assert!(
            output_rx.try_recv().is_err(),
            "no empty or duplicate update"
        );
    }
}

/// A small command that emitted a live batch must expose its complete ordered
/// shell file at natural completion even though neither native body was
/// clipped.
#[cfg(unix)]
#[test]
fn live_batch_then_natural_completion_exposes_complete_shell_file() {
    use std::fs::OpenOptions;
    use std::io::Write;
    let directory = tempfile::tempdir().expect("directory");
    let fifo = directory.path().join("release");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("FIFO")
            .success()
    );
    let arguments = shell_args(
        &format!(
            "printf 'EARLY\\n'; read ignored < '{}'; printf 'FINAL\\n' >&2",
            fifo.display()
        ),
        45,
    );
    let (output_tx, output_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let stream = OutputStreamCapture::new(
            crate::Output::channel(output_tx),
            "natural-stream".into(),
            tau_proto::ToolName::new("shell"),
        )
        .expect("stream capture");
        let outcome = run_command_live_for_surface_with_authorized_cwd(
            path_crate_tools::ShellSurface::Generic,
            &arguments,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            ShellRunControl {
                cancel_rx: None,
                output_stream: Some(stream),
            },
        );
        done_tx.send(outcome).expect("outcome");
    });
    let message = output_rx
        .recv_timeout(path_std_time::Duration::from_secs(30))
        .expect("live batch before command completes");
    let tau_proto::HarnessInputMessage::Emit(emit) = message else {
        panic!("emitted batch");
    };
    let Event::ToolOutputReported(report) = *emit.event else {
        panic!("output report");
    };
    assert!(report.text.contains("EARLY"));
    assert!(
        done_rx.try_recv().is_err(),
        "source must precede completion"
    );
    OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("release writer")
        .write_all(b"go\n")
        .expect("release child");
    let CommandOutcome::Finished(output) = done_rx
        .recv_timeout(path_std_time::Duration::from_secs(10))
        .expect("completion")
        .expect("shell")
    else {
        panic!("natural completion");
    };
    worker.join().expect("worker");
    assert_eq!(output_text(&output.result), "err FINAL");
    assert!(tau_proto::cbor_field(&output.result, "truncated").is_none());
    let path =
        tau_proto::cbor_text_field(&output.result, "full_output_path").expect("complete path");
    let captured = tau_proto::ToolFinalOutput::capture(
        &output.result,
        tau_proto::ProviderToolResultStatus::Success,
    );
    assert!(captured.text.contains(&format!("full_output_path: {path}")));
    assert_eq!(
        std::fs::read_to_string(path).expect("ordinary file read"),
        "out EARLY\nerr FINAL"
    );
}

/// If the optional lifecycle wake pipe is unavailable, the pre-effect capacity
/// endpoint still wakes closed-output waits for a later child exit or cancel.
#[cfg(unix)]
#[test]
fn missing_lifecycle_pipe_does_not_strand_closed_output_exit_or_cancellation() {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    for cancel in [false, true] {
        let (output_tx, _output_rx) = mpsc::channel();
        let stream = OutputStreamCapture::new(
            crate::Output::channel(output_tx),
            "closed-output".into(),
            tau_proto::ToolName::new("shell"),
        )
        .expect("pre-effect wake endpoint");
        let mut child = Command::new("sh")
            .args(["-c", "read ignored"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("controlled child");
        let mut input = child.stdin.take().expect("child input");
        let process = ShellProcess {
            child,
            stdout: None,
            stderr: None,
            #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
            output_users: None,
        };
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (poll_tx, poll_rx) = mpsc::sync_channel(1);
        // Inject failure/absence of the optional child wake allocation. No
        // stdout/stderr descriptor can accidentally deliver the terminal wake.
        let mut state = ShellWaitState::start_with_wake_pipe(
            process,
            Some(cancel_rx),
            Some(stream.lifecycle_waker()),
            None,
        );
        state.closed_output_poll = Some(poll_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx
                .send(state.wait(path_std_time::Duration::from_secs(30), Some(stream)))
                .expect("wait result");
        });
        poll_rx
            .recv_timeout(path_std_time::Duration::from_secs(2))
            .expect("closed-output poll boundary");
        if cancel {
            cancel_tx.send(()).expect("late cancellation");
        } else {
            input.write_all(b"exit\n").expect("release child to exit");
        }
        let outcome = done_rx
            .recv_timeout(path_std_time::Duration::from_secs(5))
            .expect("terminal must wake before deadline");
        assert_eq!(outcome.cancelled, cancel);
        assert!(!outcome.timed_out);
        if !cancel {
            assert!(outcome.success);
        }
        worker.join().expect("worker");
    }
}

/// Exercise the real pipe/PTY coordinator, not only the capture clock: a
/// nonterminal report arrives while the process is alive, cancellation reaps
/// it, and VCR replay retains the same output without executing it again.
#[test]
fn live_stream_can_cancel_running_shell_and_replay_preserves_batches() {
    let cassette_dir = tempfile::tempdir().expect("cassette directory");
    let marker = cassette_dir.path().join("executed");
    let arguments = shell_args(
        &format!(
            "printf ran > '{}'; printf STREAM_SENTINEL; sleep 60; printf NATURAL_EXIT",
            marker.display()
        ),
        90,
    );
    for mode in [
        tau_vcr::VcrMode::RecordIfMissing,
        tau_vcr::VcrMode::ReplayOnly,
    ] {
        let mut world = ShellWorld::for_tool(
            "shell",
            "stream-vcr",
            &arguments,
            Some(tau_vcr::VcrConfig::new(mode, cassette_dir.path())),
        )
        .expect("shell world");
        let (output_tx, output_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let arguments = arguments.clone();
        let worker = std::thread::spawn(move || {
            let outcome = run_command_cancellable_for_tool(
                ShellInvocation {
                    surface: path_crate_tools::ShellSurface::Generic,
                    call_id: "stream-vcr",
                    arguments: &arguments,
                    authorized_cwd: None,
                    output_stream: Some(
                        OutputStreamCapture::new(
                            crate::Output::channel(output_tx),
                            "stream-vcr".into(),
                            tau_proto::ToolName::new("shell"),
                        )
                        .expect("stream capture"),
                    ),
                },
                &ShellConfig::default(),
                ShellCommandMode::READ_WRITE_HIDDEN,
                false,
                Some(cancel_rx),
                &mut world,
            );
            world.finish().expect("finish shell world");
            done_tx.send(outcome).expect("send outcome");
        });
        let message = output_rx.recv_timeout(path_std_time::Duration::from_secs(30));
        let still_running = done_rx.try_recv().is_err();
        cancel_tx.send(()).expect("cancel running invocation");
        let outcome = done_rx
            .recv_timeout(path_std_time::Duration::from_secs(10))
            .expect("prompt cancellation")
            .expect("shell invocation");
        worker.join().expect("worker");
        assert!(still_running, "output must precede terminal completion");
        assert!(matches!(outcome, CommandOutcome::Cancelled { .. }));
        let tau_proto::HarnessInputMessage::Emit(emit) = message.expect("streamed output") else {
            panic!("expected output emission");
        };
        let tau_proto::Event::ToolOutputReported(report) = *emit.event else {
            panic!("expected output report");
        };
        assert_eq!(report.sequence, 0);
        assert!(report.text.contains("STREAM_SENTINEL"));
        assert!(!report.text.contains("NATURAL_EXIT"));
        assert_eq!(report.omitted_bytes, 0);
        assert!(
            output_rx.try_recv().is_err(),
            "terminal cannot repeat the batch"
        );
        if mode == tau_vcr::VcrMode::RecordIfMissing {
            assert!(marker.exists(), "recording must execute");
            std::fs::remove_file(&marker).expect("remove live marker");
        } else {
            assert!(!marker.exists(), "replay must not execute the command");
        }
    }
}

/// GPT surface validation must run before VCR replay so a cassette cannot
/// turn the removed legacy `cwd` spelling into a successful invocation.
#[test]
fn replay_rejects_legacy_gpt_cwd_without_consuming_outcome() {
    let cassette_dir = tempfile::tempdir().expect("cassette directory");
    let arguments = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("pwd".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text("/tmp".to_owned()),
        ),
    ]);
    let record_config =
        tau_vcr::VcrConfig::new(tau_vcr::VcrMode::RecordIfMissing, cassette_dir.path());
    let mut recording = ShellWorld::for_tool(
        crate::tools::GPT_SHELL_TOOL_NAME,
        "legacy-gpt-cwd",
        &arguments,
        Some(record_config),
    )
    .expect("recording world");
    recording.record_shell_outcome(WorldShellOutcome::Cancelled { output: None });
    recording.finish().expect("finish recording");

    let replay_config = tau_vcr::VcrConfig::new(tau_vcr::VcrMode::ReplayOnly, cassette_dir.path());
    let mut replay = ShellWorld::for_tool(
        crate::tools::GPT_SHELL_TOOL_NAME,
        "legacy-gpt-cwd",
        &arguments,
        Some(replay_config),
    )
    .expect("replay world");
    let error = run_command_cancellable_for_tool(
        ShellInvocation {
            output_stream: None,
            surface: path_crate_tools::ShellSurface::ChatGpt,
            call_id: "legacy-gpt-cwd",
            arguments: &arguments,
            authorized_cwd: None,
        },
        &ShellConfig::default(),
        ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
        &mut replay,
    )
    .expect_err("legacy cwd must fail before replay");
    assert_eq!(
        error.message,
        "argument `cwd` is not supported by `shell_command`; use call-local `workdir`"
    );
    assert!(
        replay.finish().is_err(),
        "validation failure must not consume the recorded shell outcome"
    );
}

/// Protects user shell clipping feedback for a single huge no-newline
/// stdout stream. The final tail truncation must not drop the explicit
/// marker that tells session history the captured context is incomplete.
#[test]
fn clipped_user_shell_output_marker_survives_tail_truncation() {
    let mut output = "x".repeat(MAX_OUTPUT_BYTES);

    append_guaranteed_output_truncated_marker(&mut output);
    let truncated = crate::truncate::truncate_tail(&output);

    assert!(truncated.content.ends_with(USER_OUTPUT_TRUNCATED_MARKER));
    assert!(truncated.content.len() <= MAX_OUTPUT_BYTES);
}

/// Ensures user shell progress streaming shares one bounded budget while
/// output capture can keep draining both child pipes.
#[test]
fn user_shell_progress_stream_is_bounded() {
    let mut progress = UserProgressBudget::default();
    let content_limit = MAX_OUTPUT_BYTES - USER_OUTPUT_TRUNCATED_MARKER.len();
    let first = progress
        .chunk(&"x".repeat(content_limit - 1))
        .expect("first progress chunk");
    let marker = progress.chunk("abcdef").expect("truncation marker");

    assert_eq!(first.len(), content_limit - 1);
    assert_eq!(marker, format!("a{USER_OUTPUT_TRUNCATED_MARKER}"));
    assert_eq!(first.len() + marker.len(), MAX_OUTPUT_BYTES);
    assert!(progress.chunk("more").is_none());
}

/// Ensures merged stdout/stderr, metadata, and saved rendering share the
/// final 10 KiB/16 MiB bounds rather than multiplying them per stream.
#[test]
fn merged_user_shell_output_uses_shared_budgets() {
    let mut stdout = UserStreamCapture::default();
    let mut stderr = UserStreamCapture::default();
    let mut saved = UserSavedCapture::default();
    let stdout_chunk = "o".repeat(6 * 1024);
    let stderr_chunk = "e".repeat(6 * 1024);
    stdout.push_chunk(&stdout_chunk);
    saved.push(tau_proto::ShellStream::Stdout, &stdout_chunk);
    stderr.push_chunk(&stderr_chunk);
    saved.push(tau_proto::ShellStream::Stderr, &stderr_chunk);

    let output = merged_user_shell_output(stdout, stderr, saved, None);
    assert!(output.len() <= MAX_OUTPUT_BYTES);
    assert!(output.contains("[tau-output-metadata]"));
    assert!(output.contains("total_bytes: 12298"));
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("full_output_path: "))
        .expect("saved output path");
    let saved = std::fs::read_to_string(path).expect("saved merged output");
    assert!(saved.starts_with('o'));
    assert!(saved.contains("[stderr]\n"));
    assert!(saved.ends_with('e'));
}

/// Ensures one stream can use the full shared 16 MiB saved budget rather
/// than an artificial per-stream partition.
#[test]
fn merged_user_shell_output_keeps_large_single_stream_complete() {
    let chunk = "x".repeat(12 * 1024 * 1024);
    let mut stdout = UserStreamCapture::default();
    let mut saved = UserSavedCapture::default();
    stdout.push_chunk(&chunk);
    saved.push(tau_proto::ShellStream::Stdout, &chunk);

    let output = merged_user_shell_output(stdout, UserStreamCapture::default(), saved, None);
    assert!(output.len() <= MAX_OUTPUT_BYTES);
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("full_output_path: "))
        .expect("complete saved output path");
    assert_eq!(
        std::fs::metadata(path).expect("saved metadata").len(),
        chunk.len() as u64
    );
}

/// Ensures native user-shell totals include the blank separator created
/// when newline-terminated stdout precedes stderr.
#[test]
fn merged_user_shell_output_counts_trailing_newline_separator() {
    let stdout_chunk = "o\n".repeat(6_000);
    let stderr_chunk = "e";
    let mut stdout = UserStreamCapture::default();
    let mut stderr = UserStreamCapture::default();
    let mut saved = UserSavedCapture::default();
    stdout.push_chunk(&stdout_chunk);
    saved.push(tau_proto::ShellStream::Stdout, &stdout_chunk);
    stderr.push_chunk(stderr_chunk);
    saved.push(tau_proto::ShellStream::Stderr, stderr_chunk);

    let output = merged_user_shell_output(stdout, stderr, saved, None);
    assert!(output.contains("total_lines: 6003"));
    assert!(output.contains("total_bytes: 12011"));
    assert!(output.len() <= MAX_OUTPUT_BYTES);
}

/// Ensures output beyond the shared 16 MiB retained budget exposes honest
/// partial-artifact metadata within the final visible cap.
#[test]
fn merged_user_shell_output_reports_partial_saved_artifact() {
    let chunk = "x".repeat(MAX_SAVED_OUTPUT_BYTES + 1);
    let mut stdout = UserStreamCapture::default();
    let mut saved = UserSavedCapture::default();
    stdout.push_chunk(&chunk);
    saved.push(tau_proto::ShellStream::Stdout, &chunk);

    let output = merged_user_shell_output(stdout, UserStreamCapture::default(), saved, None);
    assert!(output.contains("saved_output_path: "));
    assert!(output.contains("saved_output_truncated: true"));
    assert!(output.contains(&format!("saved_output_bytes: {MAX_SAVED_OUTPUT_BYTES}")));
    assert!(output.len() <= MAX_OUTPUT_BYTES);
}

/// Ensures the fixed Vec arena preserves distinct native-order UTF-8
/// sections and never changes capacity during stderr reclamation.
#[test]
fn user_saved_capture_bounds_capacity_after_stderr_reclaim() {
    let stderr_first = "é".repeat(1024 * 1024);
    let stderr_second = "z".repeat(2 * 1024 * 1024);
    let stdout_chunk = "o".repeat(12 * 1024 * 1024);
    let mut saved = UserSavedCapture::default();
    let fixed_capacity = saved.bytes.capacity();
    saved.push(tau_proto::ShellStream::Stderr, &stderr_first);
    saved.push(tau_proto::ShellStream::Stderr, &stderr_second);
    saved.push(tau_proto::ShellStream::Stdout, &stdout_chunk);

    assert_eq!(saved.bytes.capacity(), fixed_capacity);
    assert_eq!(saved.stdout(), stdout_chunk);
    let rendering = saved.rendering_parts().concat();
    let expected = format!(
        "{stdout_chunk}\n{}{stderr_first}",
        UserSavedCapture::STDERR_LABEL
    );
    assert_eq!(rendering, expected);
    let stderr = saved.stderr_parts().collect::<String>();
    assert_eq!(stderr, stderr_first);
    assert!(
        stdout_chunk.len() + 1 + UserSavedCapture::STDERR_LABEL.len() + stderr.len()
            <= MAX_SAVED_OUTPUT_BYTES
    );
    assert!(std::str::from_utf8(&saved.bytes).is_ok());
    assert_eq!(saved.stdout_len, stdout_chunk.len());
    assert_eq!(saved.stderr_bytes, stderr.len());
    assert_eq!(saved.stderr_cursor, saved.stderr_chunks[0].start);
    assert_eq!(
        saved
            .stderr_chunks
            .iter()
            .map(std::ops::Range::len)
            .sum::<usize>(),
        saved.stderr_bytes
    );
    assert!(saved.stdout_len <= saved.stderr_cursor);
    assert!(saved.stderr_incomplete);
    assert!(saved.incomplete);
}

/// Ensures UTF-8 backoff that omits stdout also removes later stderr so the
/// retained artifact remains a native-order prefix without holes.
#[test]
fn user_saved_capture_drops_stderr_after_stdout_utf8_backoff() {
    let mut saved = UserSavedCapture::default();
    saved.push(tau_proto::ShellStream::Stderr, "S");
    saved.push(
        tau_proto::ShellStream::Stdout,
        &"x".repeat(MAX_SAVED_OUTPUT_BYTES - 12),
    );
    saved.push(tau_proto::ShellStream::Stdout, "€");

    assert_eq!(saved.stderr_bytes, 0);
    assert!(saved.stderr_parts().next().is_none());
    assert!(saved.incomplete);
    assert!(saved.stdout_incomplete);
    assert_eq!(
        saved.rendering_parts().concat(),
        "x".repeat(MAX_SAVED_OUTPUT_BYTES - 12)
    );
    assert_eq!(saved.stderr_cursor, saved.bytes.len());
}

/// Ensures byte-at-a-time stderr cannot create unbounded descriptors or
/// final write parts outside the fixed arena budget.
#[test]
fn user_saved_capture_bounds_stderr_descriptors() {
    let mut saved = UserSavedCapture::default();
    for _ in 0..=MAX_USER_STDERR_CHUNKS {
        saved.push(tau_proto::ShellStream::Stderr, "x");
    }

    assert_eq!(saved.stderr_chunks.len(), MAX_USER_STDERR_CHUNKS);
    assert!(saved.stderr_incomplete);
    assert!(saved.incomplete);
    assert_eq!(saved.rendering_parts().len(), MAX_USER_STDERR_CHUNKS + 2);
    assert_eq!(
        saved.rendering_parts().concat(),
        format!(
            "{}{}",
            UserSavedCapture::STDERR_LABEL,
            "x".repeat(MAX_USER_STDERR_CHUNKS)
        )
    );
    assert_eq!(
        saved.stderr_cursor,
        saved.stderr_chunks.last().expect("range").start
    );
    assert_eq!(
        saved
            .stderr_chunks
            .iter()
            .map(std::ops::Range::len)
            .sum::<usize>(),
        saved.stderr_bytes
    );
}

/// Ensures multiple retained stderr descriptors render in arrival order.
#[test]
fn user_saved_capture_preserves_stderr_descriptor_order() {
    let mut saved = UserSavedCapture::default();
    saved.push(tau_proto::ShellStream::Stderr, "first-");
    saved.push(tau_proto::ShellStream::Stderr, "second");

    assert_eq!(
        saved.rendering_parts().concat(),
        format!("{}first-second", UserSavedCapture::STDERR_LABEL)
    );
}

fn record_cancelled_shell(
    cassette_dir: &std::path::Path,
    call_id: &str,
    timeout: i64,
) -> CborValue {
    let args = shell_args("sleep 10", timeout);
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let cassette_path = cassette_dir.to_owned();
    let args_for_thread = args.clone();
    let call_id = call_id.to_owned();
    let handle = std::thread::spawn(move || {
        let mut world = ShellWorld::for_tool(
            "shell",
            &call_id,
            &args_for_thread,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                &cassette_path,
            )),
        )?;
        let outcome = run_command_cancellable(
            &call_id,
            &args_for_thread,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            Some(cancel_rx),
            &mut world,
        );
        world.finish()?;
        outcome
    });
    std::thread::sleep(path_std_time::Duration::from_millis(25));
    cancel_tx.send(()).expect("send cancel");
    let outcome = handle
        .join()
        .expect("join recording")
        .expect("record shell");
    assert!(matches!(outcome, CommandOutcome::Cancelled { .. }));
    args
}

#[test]
fn shell_vcr_replays_finished_result_without_running_command() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let data_dir = tempfile::TempDir::new().expect("data dir");
    let file = data_dir.path().join("value.txt");
    std::fs::write(&file, "recorded-output").expect("write recorded value");
    let args = shell_args(&format!("cat {}", file.display()), 1);
    let mut world = ShellWorld::for_tool(
        "shell",
        "call_shell",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("record world");
    let recorded = run_command_cancellable(
        "call_shell",
        &args,
        &ShellConfig::default(),
        ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
        &mut world,
    )
    .expect("record shell");
    world.finish().expect("finish recording");
    assert!(matches!(recorded, CommandOutcome::Finished(_)));
    let cassette = std::fs::read_to_string(cassette_dir.path().join("call_shell.yaml"))
        .expect("read cassette");
    assert!(cassette.contains("op: shell"));
    std::fs::write(&file, "live-output").expect("write live value");

    let mut world = ShellWorld::for_tool(
        "shell",
        "call_shell",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let outcome = run_command_cancellable(
        "call_shell",
        &args,
        &ShellConfig::default(),
        ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
        &mut world,
    )
    .expect("replay shell");
    world.finish().expect("finish replay");

    let CommandOutcome::Finished(output) = outcome else {
        panic!("expected finished outcome");
    };
    assert_eq!(output_text(&output.result), "out(no_nl) recorded-output");
}

#[test]
fn shell_vcr_finished_replay_sleeps_at_scaled_recorded_duration() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = shell_args("printf live-output", 1);
    let mut world = ShellWorld::for_tool(
        "shell",
        "call_slow_finished_shell",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("record world");
    world.record_shell_outcome(WorldShellOutcome::Finished {
        result: CborValue::Map(vec![(
            CborValue::Text("output".to_owned()),
            CborValue::Text("out(no_nl) recorded-output".to_owned()),
        )]),
        display: Box::new(ok_display("recorded")),
        elapsed_ms: 5_000,
        saved_output: None,
    });
    world.finish().expect("finish recording");

    let mut world = ShellWorld::for_tool(
        "shell",
        "call_slow_finished_shell",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let started = path_std_time::Instant::now();
    let outcome = run_command_cancellable(
        "call_slow_finished_shell",
        &args,
        &ShellConfig::default(),
        ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
        &mut world,
    )
    .expect("replay shell");
    let elapsed = started.elapsed();
    world.finish().expect("finish replay");

    assert!(
        elapsed >= std::time::Duration::from_millis(40),
        "replay should preserve scaled shell timing, elapsed={elapsed:?}"
    );
    let CommandOutcome::Finished(output) = outcome else {
        panic!("expected finished outcome");
    };
    assert_eq!(output_text(&output.result), "out(no_nl) recorded-output");
}

#[test]
fn shell_vcr_cancelled_replay_requires_cancel_request() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = record_cancelled_shell(cassette_dir.path(), "call_cancelled_shell", 1);
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let cassette_path = cassette_dir.path().to_owned();
    let args_for_thread = args.clone();
    let handle = std::thread::spawn(move || {
        let mut world = ShellWorld::for_tool(
            "shell",
            "call_cancelled_shell",
            &args_for_thread,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                &cassette_path,
            )),
        )?;
        let outcome = run_command_cancellable(
            "call_cancelled_shell",
            &args_for_thread,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            Some(cancel_rx),
            &mut world,
        );
        world.finish()?;
        outcome
    });
    std::thread::sleep(path_std_time::Duration::from_millis(25));
    cancel_tx.send(()).expect("send cancel");

    let outcome = handle.join().expect("join replay").expect("replay shell");
    assert!(matches!(outcome, CommandOutcome::Cancelled { .. }));
}

#[test]
fn shell_vcr_records_cancelled_outcome() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    record_cancelled_shell(cassette_dir.path(), "call_record_cancelled_shell", 5);
    let cassette =
        std::fs::read_to_string(cassette_dir.path().join("call_record_cancelled_shell.yaml"))
            .expect("read cassette");
    assert!(cassette.contains("op: shell"));
    assert!(cassette.contains("kind: cancelled"));
}

#[test]
fn shell_vcr_cancelled_replay_errors_without_cancel_request() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let args = record_cancelled_shell(cassette_dir.path(), "call_cancelled_shell", 1);
    let (_cancel_tx, cancel_rx) = mpsc::channel();
    let mut world = ShellWorld::for_tool(
        "shell",
        "call_cancelled_shell",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");

    let error = run_command_cancellable(
        "call_cancelled_shell",
        &args,
        &ShellConfig::default(),
        ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        Some(cancel_rx),
        &mut world,
    )
    .expect_err("missing cancel should fail");

    assert!(error.message.contains("expected shell cancellation"));
}

/// Ensures VCR stores truncated shell rendering in its bounded side
/// artifact and replay creates a fresh ephemeral path instead of
/// persisting one.
#[test]
fn shell_vcr_regenerates_saved_output_from_side_artifact() {
    let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
    let source_dir = tempfile::TempDir::new().expect("source dir");
    let source_path = source_dir.path().join("output");
    let saved = "out x\n".repeat(2_000);
    std::fs::write(&source_path, &saved).expect("write source");
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf replay".to_owned()),
    )]);
    let mut world = ShellWorld::for_tool(
        "shell",
        "saved_vcr",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::RecordIfMissing,
            cassette_dir.path(),
        )),
    )
    .expect("record world");
    world.record_shell_outcome(WorldShellOutcome::Finished {
        result: CborValue::Map(vec![
            (
                CborValue::Text("output".to_owned()),
                CborValue::Text("out x".to_owned()),
            ),
            (
                CborValue::Text("saved_output_path".to_owned()),
                CborValue::Text(source_path.to_string_lossy().into_owned()),
            ),
            (
                CborValue::Text("saved_output_truncated".to_owned()),
                CborValue::Bool(true),
            ),
            (
                CborValue::Text("saved_output_bytes".to_owned()),
                CborValue::Integer((saved.len() as i64).into()),
            ),
        ]),
        display: Box::new(ok_display("recorded")),
        elapsed_ms: 1,
        saved_output: None,
    });
    world.finish().expect("finish recording");
    let yaml = std::fs::read_to_string(cassette_dir.path().join("saved_vcr.yaml")).expect("yaml");
    assert!(!yaml.contains(source_path.to_string_lossy().as_ref()));
    assert!(!yaml.contains(&saved));
    assert_eq!(
        std::fs::read(cassette_dir.path().join("saved_vcr.shell-output")).expect("side artifact"),
        saved.as_bytes()
    );

    let mut replay = ShellWorld::for_tool(
        "shell",
        "saved_vcr",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("replay world");
    let WorldShellOutcome::Finished { result, .. } = replay
        .replay_shell_outcome()
        .expect("replay")
        .expect("outcome")
    else {
        panic!("finished outcome");
    };
    let CborValue::Map(entries) = result else {
        panic!("result map");
    };
    let replay_path = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(path)) if key == "saved_output_path" => {
                Some(path)
            }
            _ => None,
        })
        .expect("fresh output path");
    assert!(entries.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Bool(true))
            if key == "saved_output_truncated"
    )));
    assert!(entries.iter().any(|(key, value)| {
        matches!(
            (key, value),
            (CborValue::Text(key), CborValue::Integer(bytes))
                if key == "saved_output_bytes"
                    && i128::from(*bytes) == saved.len() as i128
        )
    }));
    assert_ne!(replay_path, &source_path.to_string_lossy());
    assert_eq!(
        std::fs::read_to_string(replay_path).expect("fresh artifact"),
        saved
    );
    std::fs::remove_file(cassette_dir.path().join("saved_vcr.shell-output"))
        .expect("remove recorded side artifact");
    let mut replay = ShellWorld::for_tool(
        "shell",
        "saved_vcr",
        &args,
        Some(tau_vcr::VcrConfig::new(
            tau_vcr::VcrMode::ReplayOnly,
            cassette_dir.path(),
        )),
    )
    .expect("missing-side replay world");
    let WorldShellOutcome::Finished { result, .. } = replay
        .replay_shell_outcome()
        .expect("replay missing side")
        .expect("outcome")
    else {
        panic!("finished");
    };
    assert_eq!(
        tau_proto::cbor_bool_field(&result, "saved_output_unavailable"),
        Some(true)
    );
    assert!(tau_proto::cbor_field(&result, "truncated").is_none());
}

/// Replay must retain an original spool failure even without native truncation,
/// rather than silently dropping the only recovery-state header.
#[test]
fn shell_vcr_preserves_originally_unavailable_saved_output() {
    let cassette = tempfile::tempdir().expect("cassette");
    let args = shell_args("printf short", 30);
    for mode in [
        tau_vcr::VcrMode::RecordIfMissing,
        tau_vcr::VcrMode::ReplayOnly,
    ] {
        let mut world = ShellWorld::for_tool(
            "shell",
            "unavailable_vcr",
            &args,
            Some(tau_vcr::VcrConfig::new(mode, cassette.path())),
        )
        .expect("world");
        if mode == tau_vcr::VcrMode::RecordIfMissing {
            world.record_shell_outcome(WorldShellOutcome::Finished {
                result: CborValue::Map(vec![
                    (
                        CborValue::Text("output".into()),
                        CborValue::Text("out short".into()),
                    ),
                    (
                        CborValue::Text("saved_output_unavailable".into()),
                        CborValue::Bool(true),
                    ),
                ]),
                display: Box::new(ok_display("recorded unavailable spool")),
                elapsed_ms: 1,
                saved_output: None,
            });
            world.finish().expect("record unavailable");
        } else {
            let WorldShellOutcome::Finished { result, .. } = world
                .replay_shell_outcome()
                .expect("replay")
                .expect("recorded result")
            else {
                panic!("finished");
            };
            let captured = tau_proto::ToolFinalOutput::capture(
                &result,
                tau_proto::ProviderToolResultStatus::Success,
            );
            assert!(captured.text.contains("saved_output_unavailable: true"));
            assert!(tau_proto::cbor_field(&result, "truncated").is_none());
        }
    }
}

/// Earlier live batches need a visible file header even when the native total
/// fits its cap; complete, capped, and unavailable spools must remain truthful.
#[test]
fn earlier_stream_batches_keep_truthful_saved_headers_without_native_truncation() {
    let mut overflow = CapturedOutput::default();
    overflow.push_bytes(
        OutputStream::Stdout,
        "é".repeat(MAX_SAVED_OUTPUT_BYTES / 2 + 100).as_bytes(),
    );
    overflow.finish();
    assert!(overflow.saved_output_incomplete);
    for saved in [
        SavedArtifact::Available(
            crate::shell_output_spool::save("out first\nerr second", false)
                .expect("complete spool"),
        ),
        SavedArtifact::Available(
            crate::shell_output_spool::save(
                &overflow.saved_output,
                overflow.saved_output_incomplete,
            )
            .expect("capped spool"),
        ),
        SavedArtifact::Unavailable,
    ] {
        let expected = match &saved {
            SavedArtifact::Available(saved) => {
                Some((saved.path.clone(), saved.incomplete, saved.saved_bytes))
            }
            SavedArtifact::Unavailable => None,
        };
        let result = command_details_value(CommandDetails {
            status: Some(0),
            signal: None,
            duration_seconds: None,
            termination_reason: "exit",
            total_lines: None,
            total_bytes: None,
            output: "out final".into(),
            truncated: false,
            valid_utf8: true,
            saved_output: Some(saved),
        });
        assert!(tau_proto::cbor_field(&result, "truncated").is_none());
        let captured = tau_proto::ToolFinalOutput::capture(
            &result,
            tau_proto::ProviderToolResultStatus::Success,
        );
        match expected {
            Some((path, incomplete, saved_bytes)) => {
                let key = if incomplete {
                    "saved_output_path"
                } else {
                    "full_output_path"
                };
                assert!(
                    captured
                        .text
                        .contains(&format!("{key}: {}", path.display()))
                );
                let bytes = std::fs::read(&path).expect("ordinary shell spool access");
                assert_eq!(bytes.len(), saved_bytes);
                assert!(std::str::from_utf8(&bytes).is_ok());
                if incomplete {
                    assert!(bytes.len() <= MAX_SAVED_OUTPUT_BYTES);
                    assert!(captured.text.contains("saved_output_truncated: true"));
                    assert!(!captured.text.contains("full_output_path"));
                } else {
                    assert_eq!(bytes, b"out first\nerr second");
                }
            }
            None => {
                assert!(captured.text.contains("saved_output_unavailable: true"))
            }
        }
    }
}

/// Ensures a line beyond the saved hard cap becomes an honestly incomplete
/// saved artifact and a marker-only model-visible line.
#[test]
fn captured_output_bounds_no_newline_lines() {
    let mut output = CapturedOutput::default();
    output.push_bytes(
        OutputStream::Stdout,
        &vec![b'x'; MAX_CAPTURED_LINE_BYTES + 128],
    );
    output.finish();

    let truncated = output.truncate();
    assert_eq!(truncated.content, "out(no_nl,truncated) ");
    assert!(truncated.was_truncated);
    assert!(MAX_MODEL_SHELL_OUTPUT_BYTES < truncated.total_bytes);
    assert!(output.saved_output_incomplete);
    assert!(output.saved_output.len() <= MAX_SAVED_OUTPUT_BYTES);
    assert!(output.saved_output.starts_with("out(no_nl,truncated) xxx"));
}

/// Ensures shell totals and saved artifacts count the complete rendered UTF-8
/// records rather than the child process's raw stdout/stderr payload bytes.
#[test]
fn captured_output_totals_match_rendered_records() {
    let stdout = b"a\nb\r\n";
    let stderr = b"c\rd\xff";
    let mut output = CapturedOutput::default();
    output.push_bytes(OutputStream::Stdout, stdout);
    output.push_bytes(OutputStream::Stderr, stderr);
    output.finish();

    let expected = "out a\nout(crlf) b\nerr(cr) c\nerr(invalid-utf8,no_nl) d\u{fffd}";
    assert_eq!(output.saved_output, expected);
    assert_eq!(output.total_lines, 4);
    assert_eq!(output.total_bytes, expected.len());
    assert_ne!(output.total_bytes, stdout.len() + stderr.len());
    let visible = output.truncate();
    assert_eq!(visible.content, expected);
    assert_eq!(visible.total_bytes, expected.len());
}

/// Ensures many complete prefixed lines stop the saved rendering at its
/// hard cap and later lines cannot grow the retained buffer.
#[test]
fn captured_output_stops_saved_rendering_after_hard_cap() {
    let mut output = CapturedOutput::default();
    let line = OutputContent::Text {
        text: "é".repeat(4096),
        ending: Some(LineEndingKind::Lf),
    };
    while !output.saved_output_incomplete {
        output.push_line(OutputStream::Stdout, line.clone());
    }
    let capped_len = output.saved_output.len();
    output.push_line(OutputStream::Stderr, line);

    assert!(output.saved_output_incomplete);
    assert!(capped_len <= MAX_SAVED_OUTPUT_BYTES);
    assert_eq!(output.saved_output.len(), capped_len);
    assert!(
        output
            .saved_output
            .lines()
            .all(|line| { line == "...(saved output truncated)" || line.starts_with("out ") })
    );
    assert!(std::str::from_utf8(output.saved_output.as_bytes()).is_ok());
}
