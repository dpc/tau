use std::time as path_std_time;

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
    recording.record_shell_outcome(WorldShellOutcome::Cancelled);
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
            surface: path_crate_tools::ShellSurface::ChatGpt,
            call_id: "legacy-gpt-cwd",
            arguments: &arguments,
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
    assert!(matches!(outcome, CommandOutcome::Cancelled));
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
    assert!(matches!(outcome, CommandOutcome::Cancelled));
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
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(true),
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
