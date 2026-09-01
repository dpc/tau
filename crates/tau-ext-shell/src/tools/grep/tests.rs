use std::{io as path_std_io, process as path_std_process, time as path_std_time};

use base64::engine as path_base64_engine;

use super::*;

fn args(extra: (&str, CborValue)) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("needle".to_owned()),
        ),
        (CborValue::Text(extra.0.to_owned()), extra.1),
    ])
}

/// Ensures omitted and explicit `false` preserve literal matching while `true`
/// selects regex matching, without changing display text or argument placement.
#[test]
fn grep_pattern_mode_preserves_schema_defaults_display_and_ripgrep_argv() {
    let cases = [
        (
            "omitted regex defaults to literal",
            None,
            true,
            "needle.*",
            "./search-root",
        ),
        (
            "false regex selects literal",
            Some(CborValue::Bool(false)),
            true,
            "needle.*",
            "./search-root",
        ),
        (
            "true regex selects regex",
            Some(CborValue::Bool(true)),
            false,
            "needle.*",
            "./search-root",
        ),
    ];

    for (label, regex, expects_fixed_strings, pattern, path) in cases {
        let mut entries = vec![
            (
                CborValue::Text("pattern".to_owned()),
                CborValue::Text(pattern.to_owned()),
            ),
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.to_owned()),
            ),
        ];
        if let Some(regex) = regex {
            entries.push((CborValue::Text("regex".to_owned()), regex));
        }
        let options = GrepOptions::parse(&CborValue::Map(entries))
            .unwrap_or_else(|error| panic!("{label}: parse failed: {error:?}"));
        let args = options.ripgrep_args();
        let separator = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or_else(|| panic!("{label}: missing -- separator"));
        let mut expected = vec![
            "--json",
            "--hidden",
            "--with-filename",
            "--max-columns",
            "500",
            "--max-columns-preview",
        ];
        if expects_fixed_strings {
            expected.push("--fixed-strings");
        }
        expected.extend(["--", pattern, path]);

        assert_eq!(
            matches!(&options.pattern, GrepPattern::Literal(_)),
            expects_fixed_strings,
            "{label}"
        );
        assert_eq!(
            options.display_args(),
            format!("{pattern:?} in {path}"),
            "{label}"
        );
        assert_eq!(
            args[separator + 1..],
            [pattern.to_owned(), path.to_owned()],
            "{label}: pattern and path must follow -- in order"
        );
        assert_eq!(
            args,
            expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            "{label}: ripgrep argv"
        );
    }
}

/// Ensures grep rejects wrong-typed path/glob instead of searching the
/// default directory or dropping the glob.
#[test]
fn grep_rejects_wrong_type_optional_strings() {
    let path_err = run_grep(&args(("path", CborValue::Integer(1.into()))))
        .expect_err("integer path should be rejected");
    let glob_err = run_grep(&args(("glob", CborValue::Integer(1.into()))))
        .expect_err("integer glob should be rejected");

    assert_eq!(path_err.message, "argument `path` must be a string");
    assert_eq!(glob_err.message, "argument `glob` must be a string");
}

/// Ensures grep rejects wrong-typed optional integers before spawning rg,
/// giving callers an actionable argument error.
#[test]
fn grep_rejects_wrong_type_limit() {
    let err = run_grep(&args(("limit", CborValue::Text("10".to_owned()))))
        .expect_err("string limit should be rejected");

    assert_eq!(err.message, "argument `limit` must be an integer");
}

/// Ensures grep rejects negative context instead of silently coercing it to
/// zero context lines.
#[test]
fn grep_rejects_negative_context() {
    let err = run_grep(&args(("context", CborValue::Integer((-1).into()))))
        .expect_err("negative context should be rejected");

    assert_eq!(err.message, "context must be >= 0");
}

/// Ensures grep rejects zero limits instead of silently increasing them to
/// one match.
#[test]
fn grep_rejects_zero_limit() {
    let err = run_grep(&args(("limit", CborValue::Integer(0.into()))))
        .expect_err("zero limit should be rejected");

    assert_eq!(err.message, "limit must be >= 1");
}

/// Ensures large caller limits cannot force large pre-truncation result
/// vectors beyond the documented display capacity.
#[test]
fn grep_rejects_limit_above_output_cap() {
    let err = run_grep(&args((
        "limit",
        CborValue::Integer((MAX_GREP_LIMIT as i64 + 1).into()),
    )))
    .expect_err("limit over cap");

    assert_eq!(err.message, format!("limit must be <= {MAX_GREP_LIMIT}"));
}

/// Ensures max-limit notices do not recommend rejected larger limits.
#[test]
fn grep_max_limit_notice_asks_to_refine() {
    let notice = limit_reached_notice(MAX_GREP_LIMIT);

    assert!(notice.contains("Maximum limit reached"));
    assert!(!notice.contains(&format!("limit={}", MAX_GREP_LIMIT * 2)));
}

/// Ensures large context requests cannot multiply each match into an
/// unbounded number of rendered JSON records before final truncation.
#[test]
fn grep_rejects_context_above_cap() {
    let err = run_grep(&args((
        "context",
        CborValue::Integer((MAX_GREP_CONTEXT as i64 + 1).into()),
    )))
    .expect_err("context over cap");

    assert_eq!(
        err.message,
        format!("context must be <= {MAX_GREP_CONTEXT}")
    );
}
/// Protects the stderr drain used while grep reads stdout. The capture must
/// stay bounded so a noisy ripgrep cannot trade pipe backpressure for
/// unbounded memory growth in the drain thread.
#[test]
fn grep_stderr_drain_caps_captured_bytes() {
    let captured = read_limited_bytes(
        path_std_io::Cursor::new(vec![b'x'; MAX_OUTPUT_BYTES + 100]),
        32,
    );

    assert_eq!(captured.len(), 32);
    assert!(captured.iter().all(|byte| *byte == b'x'));
}

/// Ensures an early cancellation request takes the cancellable grep path
/// and reports cancellation rather than a normal grep result.
#[test]
fn grep_cancellable_stops_on_early_cancel_request() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha.txt"), "needle").expect("write file");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("needle".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let (cancel_tx, cancel_rx) = mpsc::channel();
    cancel_tx.send(()).expect("send cancel");

    let result = run_grep_cancellable(&args, Some(cancel_rx)).expect("grep result");

    assert!(matches!(result, CancellableToolRun::Cancelled));
}

/// Ensures the ripgrep waiter can terminate an already-running child when a
/// cancellation request arrives after process start.
#[test]
fn grep_waiter_kills_running_child_on_cancel_request() {
    let child = Command::new("sh")
        .arg("-c")
        .arg("sleep 10")
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null())
        .spawn()
        .expect("spawn sleeping child");
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let (_stop_tx, stop_rx) = mpsc::channel();
    let started = path_std_time::Instant::now();

    cancel_tx.send(()).expect("send cancel");
    let (_status, cancelled) = wait_ripgrep(child, stop_rx, Some(cancel_rx)).expect("wait child");

    assert!(cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

/// Ensures the match-limit stop path kills a running child promptly without
/// reporting the run as caller-cancelled.
#[test]
fn grep_waiter_kills_running_child_on_match_limit_stop() {
    let child = Command::new("sh")
        .arg("-c")
        .arg("sleep 10")
        .stdout(path_std_process::Stdio::null())
        .stderr(path_std_process::Stdio::null())
        .spawn()
        .expect("spawn sleeping child");
    let (stop_tx, stop_rx) = mpsc::channel();
    let started = path_std_time::Instant::now();

    stop_tx.send(()).expect("send stop");
    let (_status, cancelled) = wait_ripgrep(child, stop_rx, None).expect("wait child");

    assert!(!cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

/// Protects grep output from path line injection by escaping control
/// characters in ripgrep JSON path text before rendering records.
#[test]
fn grep_escapes_control_characters_in_paths() {
    let json = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "line\nbreak.txt" },
            "lines": { "text": "needle\n" },
            "line_number": 7
        }
    });
    let output = read_grep_json(json.to_string().as_bytes(), 10);

    assert_eq!(output.result_lines, vec!["line\\nbreak.txt", "7:needle"]);
}

/// Ensures grep handles ripgrep byte paths without silently dropping the
/// record, marking invalid UTF-8 while preserving a lossy escaped path.
#[test]
fn grep_renders_invalid_utf8_byte_paths() {
    let encoded = base64::Engine::encode(
        &path_base64_engine::general_purpose::STANDARD,
        b"bad\xffname.txt",
    );
    let json = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "bytes": encoded },
            "lines": { "text": "needle\n" },
            "line_number": 3
        }
    });
    let output = read_grep_json(json.to_string().as_bytes(), 10);

    assert_eq!(
        output.result_lines,
        vec!["(invalid-utf8) bad�name.txt", "3:needle"]
    );
}

/// Ensures grep reports the number of rendered matches, not the extra
/// over-limit match used only to detect that the limit was reached.
#[test]
fn grep_limit_reports_rendered_match_count() {
    let first = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "file.txt" },
            "lines": { "text": "needle one\n" },
            "line_number": 1
        }
    });
    let second = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "file.txt" },
            "lines": { "text": "needle two\n" },
            "line_number": 2
        }
    });
    let input = format!("{first}\n{second}\n");

    let output = read_grep_json(input.as_bytes(), 1);

    assert_eq!(output.match_count, 1);
    assert!(output.match_limit_reached);
    assert_eq!(output.result_lines, vec!["file.txt", "1:needle one"]);
}

/// Ensures grep long-line shortening preserves the line number prefix
/// instead of replacing the whole rendered match with a marker.
#[test]
fn grep_long_line_truncation_preserves_location_prefix() {
    let (line, truncated) = render_grep_line(42, ':', &"x".repeat(1000));

    assert!(truncated);
    assert!(line.starts_with("42:"), "line was {line:?}");
    assert!(line.ends_with('…'));
    assert!(line.len() <= GREP_MAX_LINE_LENGTH);
}

/// Ensures read_grep_json groups match lines under a single per-file path
/// heading, using `:` for matches and `-` for context lines.
#[test]
fn grep_renders_heading_grouped_matches_and_context() {
    let match_rec = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "src/a.rs" },
            "lines": { "text": "needle here\n" },
            "line_number": 7
        }
    });
    let context_rec = serde_json::json!({
        "type": "context",
        "data": {
            "path": { "text": "src/a.rs" },
            "lines": { "text": "context line\n" },
            "line_number": 8
        }
    });
    let second_match = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "src/b.rs" },
            "lines": { "text": "needle two\n" },
            "line_number": 3
        }
    });
    let input = format!("{match_rec}\n{context_rec}\n{second_match}\n");
    let output = read_grep_json(input.as_bytes(), 10);

    assert_eq!(
        output.result_lines,
        vec![
            "src/a.rs",
            "7:needle here",
            "8-context line",
            "src/b.rs",
            "3:needle two",
        ]
    );
    assert_eq!(output.match_count, 2);
}

/// Ensures hitting the match limit on a new file's first match does not
/// leave a dangling path heading with no body line beneath it.
#[test]
fn grep_limit_break_leaves_no_dangling_heading() {
    let first = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "a.rs" },
            "lines": { "text": "needle one\n" },
            "line_number": 1
        }
    });
    let second = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": "b.rs" },
            "lines": { "text": "needle two\n" },
            "line_number": 2
        }
    });
    let input = format!("{first}\n{second}\n");

    let output = read_grep_json(input.as_bytes(), 1);

    assert!(output.match_limit_reached);
    assert_eq!(output.match_count, 1);
    // a.rs heading + its body line; b.rs must not leave a dangling heading.
    assert_eq!(output.result_lines, vec!["a.rs", "1:needle one"]);
}

/// Ensures over-long path headings are capped at the display budget with an
/// ellipsis, matching how match body lines are truncated, so every rendered
/// line stays within `GREP_MAX_LINE_LENGTH`.
#[test]
fn grep_heading_caps_overlong_path() {
    let long_path = format!("{}pad", "p".repeat(GREP_MAX_LINE_LENGTH));
    let json = serde_json::json!({
        "type": "match",
        "data": {
            "path": { "text": long_path },
            "lines": { "text": "needle\n" },
            "line_number": 7
        }
    });
    let output = read_grep_json(json.to_string().as_bytes(), 10);

    assert!(output.lines_truncated);
    assert_eq!(output.result_lines.len(), 2);
    assert!(output.result_lines[0].ends_with('…'));
    assert!(output.result_lines[0].len() <= GREP_MAX_LINE_LENGTH);
    assert_eq!(output.result_lines[1], "7:needle");
}

/// Ensures the per-file heading falls back to the `begin` record's path when
/// a match/context record omits the path field.
#[test]
fn grep_heading_uses_begin_record_path_fallback() {
    let begin = serde_json::json!({
        "type": "begin",
        "data": { "path": { "text": "src/lib.rs" } }
    });
    let match_rec = serde_json::json!({
        "type": "match",
        "data": {
            "lines": { "text": "needle\n" },
            "line_number": 4
        }
    });
    let input = format!("{begin}\n{match_rec}\n");
    let output = read_grep_json(input.as_bytes(), 10);

    assert_eq!(output.result_lines, vec!["src/lib.rs", "4:needle"]);
}

/// Ensures grep notices are included without exceeding the documented 10
/// KiB output budget.
#[test]
fn grep_notices_stay_within_output_cap() {
    let notice = "10 KiB visible output limit reached.".to_owned();
    let suffix_len = format!("\n\n[{notice}]").len();
    let output = append_notices_within_cap(
        format!("{}étail", "x".repeat(MAX_OUTPUT_BYTES - suffix_len - 1)),
        std::slice::from_ref(&notice),
    );

    assert!(output.len() <= MAX_OUTPUT_BYTES);
    assert!(output.contains(&notice));
}
