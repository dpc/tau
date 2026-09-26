use std::path::Path;

use super::*;

fn search_file(
    bytes: &[u8],
    pattern: &str,
    extra: &[(&str, CborValue)],
) -> Result<ToolOutput, ToolFailure> {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file = tempdir.path().join("input.txt");
    std::fs::write(&file, bytes).expect("write");
    let mut entries = vec![
        (
            CborValue::Text("pattern".into()),
            CborValue::Text(pattern.into()),
        ),
        (
            CborValue::Text("path".into()),
            CborValue::Text(file.to_string_lossy().into_owned()),
        ),
    ];
    entries.extend(
        extra
            .iter()
            .map(|(key, value)| (CborValue::Text((*key).into()), value.clone())),
    );
    run_grep(&CborValue::Map(entries))
}

fn result_text(output: &ToolOutput) -> &str {
    let CborValue::Map(entries) = &output.result else {
        panic!("expected map")
    };
    let (_, CborValue::Text(text)) = entries
        .iter()
        .find(|(key, _)| key == &CborValue::Text("output".into()))
        .expect("output field")
    else {
        panic!("expected output text")
    };
    text
}

fn search_dir(
    path: &Path,
    pattern: &str,
    extra: &[(&str, CborValue)],
) -> Result<ToolOutput, ToolFailure> {
    let mut entries = vec![
        (
            CborValue::Text("pattern".into()),
            CborValue::Text(pattern.into()),
        ),
        (
            CborValue::Text("path".into()),
            CborValue::Text(path.to_string_lossy().into_owned()),
        ),
    ];
    entries.extend(
        extra
            .iter()
            .map(|(key, value)| (CborValue::Text((*key).into()), value.clone())),
    );
    run_grep(&CborValue::Map(entries))
}

/// Recursion includes hidden files, respects ignore rules and permits a
/// positive override to unignore a specific file.
#[test]
fn grep_library_ignore_hidden_and_override() {
    let td = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(td.path().join(".hidden"), b"needle\n").expect("hidden");
    std::fs::write(td.path().join(".ignore"), b"ignored.txt\n").expect("ignore rule");
    std::fs::write(td.path().join("ignored.txt"), b"needle\n").expect("ignored");
    let default = search_dir(td.path(), "needle", &[]).expect("default search");
    assert_eq!(default.display.stats.matches, Some(1));
    let overridden = search_dir(
        td.path(),
        "needle",
        &[("glob", CborValue::Text("**/ignored.txt".into()))],
    )
    .expect("override");
    assert_eq!(overridden.display.stats.matches, Some(1));
    assert!(result_text(&overridden).contains("ignored.txt"));
    let excluded = search_dir(
        td.path(),
        "needle",
        &[("glob", CborValue::Text("!**/.hidden".into()))],
    )
    .expect("negative override");
    assert_eq!(excluded.display.stats.matches, Some(0));
    let explicit =
        search_dir(&td.path().join("ignored.txt"), "needle", &[]).expect("explicit ignored file");
    assert_eq!(explicit.display.stats.matches, Some(1));
}

/// Ripgrep-specific ignore files outrank ordinary `.ignore` and a positive
/// override can still explicitly select a path excluded by `.rgignore`.
#[test]
fn grep_library_rgignore_precedence() {
    let td = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(td.path().join(".ignore"), b"allowed.txt\n").expect(".ignore");
    std::fs::write(td.path().join(".rgignore"), b"!allowed.txt\nblocked.txt\n").expect(".rgignore");
    std::fs::write(td.path().join("allowed.txt"), b"needle\n").expect("allowed");
    std::fs::write(td.path().join("blocked.txt"), b"needle\n").expect("blocked");
    let default = search_dir(td.path(), "needle", &[]).expect("default");
    assert_eq!(default.display.stats.matches, Some(1));
    assert!(result_text(&default).contains("allowed.txt"));
    let overridden = search_dir(
        td.path(),
        "needle",
        &[("glob", CborValue::Text("**/blocked.txt".into()))],
    )
    .expect("override .rgignore");
    assert_eq!(overridden.display.stats.matches, Some(1));
    assert!(result_text(&overridden).contains("blocked.txt"));
}

/// Reject special roots before opening them, but allow explicit regular-file
/// symlinks.
#[cfg(unix)]
#[test]
fn grep_library_regular_file_policy() {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    let td = tempfile::TempDir::new().expect("tempdir");
    let file = td.path().join("file");
    std::fs::write(&file, b"needle\n").expect("file");
    let link = td.path().join("link");
    symlink(&file, &link).expect("symlink");
    assert_eq!(
        search_dir(&link, "needle", &[])
            .expect("explicit symlink")
            .display
            .stats
            .matches,
        Some(1)
    );
    let dir = search_dir(td.path(), "needle", &[]).expect("recursive");
    assert_eq!(dir.display.stats.matches, Some(1));
    let socket = td.path().join("socket");
    let _listener = UnixListener::bind(&socket).expect("socket");
    let error = search_dir(&socket, "needle", &[]).expect_err("reject socket");
    assert!(error.message.contains("regular file or directory"));
}

/// Keeps fixed-string defaults distinct from opt-in regex and Unicode folding.
#[test]
fn grep_library_pattern_modes_and_unicode() {
    let literal = search_file(b"a.*\naZZ\n", "a.*", &[]).expect("literal");
    assert_eq!(literal.display.stats.matches, Some(1));
    assert!(result_text(&literal).contains("1:a.*"));
    let regex =
        search_file(b"a.*\naZZ\n", "^a.+$", &[("regex", CborValue::Bool(true))]).expect("regex");
    assert_eq!(regex.display.stats.matches, Some(2));
    let fold = search_file(
        "ÉTÉ\n".as_bytes(),
        "été",
        &[("ignoreCase", CborValue::Bool(true))],
    )
    .expect("unicode fold");
    assert_eq!(fold.display.stats.matches, Some(1));
    assert!(
        search_file(b"a\n", "(?=a)", &[("regex", CborValue::Bool(true))]).is_err(),
        "unsupported regex syntax must fail, not report no matches"
    );
}

/// The extra match proves truncation; exactly the requested count does not.
#[test]
fn grep_library_limit_uses_extra_match_sentinel() {
    let limit = [("limit", CborValue::Integer(1.into()))];
    let exact = search_file(b"x\n", "x", &limit).expect("exact");
    assert!(!result_text(&exact).contains("matches limit reached"));
    let extra = search_file(b"x\ncontext\nx\n", "x", &limit).expect("extra");
    assert_eq!(extra.display.stats.matches, Some(1));
    assert!(result_text(&extra).contains("matches limit reached"));
    assert!(!result_text(&extra).contains("3:x"));
}

/// A global limit crosses file boundaries without rendering a dangling heading
/// for a second file's unrendered sentinel match.
#[test]
fn grep_library_limit_across_files() {
    let td = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(td.path().join("first"), b"x\n").expect("first");
    std::fs::write(td.path().join("second"), b"x\n").expect("second");
    let output =
        search_dir(td.path(), "x", &[("limit", CborValue::Integer(1.into()))]).expect("search");
    assert_eq!(output.display.stats.matches, Some(1));
    assert!(result_text(&output).contains("matches limit reached"));
    let headings = result_text(&output)
        .lines()
        .filter(|line| line.ends_with("/first") || line.ends_with("/second"))
        .count();
    assert_eq!(headings, 1);
}

/// BOM decoding, CRLF display, invalid text, and explicit-file NUL conversion
/// must preserve the old line-oriented presentation without JSON transport.
#[test]
fn grep_library_decoding_and_binary() {
    let utf16 =
        search_file(&[0xff, 0xfe, b'x', 0, b'\r', 0, b'\n', 0], "x", &[]).expect("UTF-16 BOM");
    assert!(
        result_text(&utf16).contains("1:x"),
        "{:?}",
        result_text(&utf16)
    );
    assert!(!result_text(&utf16).contains("1:x\r"));
    let invalid = search_file(b"x\xff\n", "x", &[]).expect("invalid bytes");
    assert!(result_text(&invalid).contains("1:x�"));
    let binary = search_file(b"a\0x\n", "x", &[]).expect("explicit binary file");
    assert_eq!(binary.display.stats.matches, Some(1));
    let bare_cr = search_file(b"x\r", "x", &[]).expect("lone CR");
    assert!(
        result_text(&bare_cr).contains("1:x\r"),
        "{:?}",
        result_text(&bare_cr)
    );
}

/// Invalid globs and over-budget lines fail rather than silently producing
/// partial results.
#[test]
fn grep_library_invalid_glob_and_heap_limit() {
    assert!(search_file(b"x\n", "x", &[("glob", CborValue::Text("[".into()))]).is_err());
    let enormous = vec![b'x'; 16 * 1024 * 1024 + 1];
    let err = search_file(&enormous, "x", &[]).expect_err("search heap bound");
    assert!(
        err.message.contains("resource") || err.message.contains("heap"),
        "{err:?}"
    );
}

fn args(extra: (&str, CborValue)) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("needle".to_owned()),
        ),
        (CborValue::Text(extra.0.to_owned()), extra.1),
    ])
}

/// Defaults to a literal pattern and current-directory search.
#[test]
fn grep_pattern_defaults() {
    let options = GrepOptions::parse(&CborValue::Map(vec![(
        CborValue::Text("pattern".into()),
        CborValue::Text("a.*".into()),
    )]))
    .expect("parse");
    assert!(matches!(options.pattern, GrepPattern::Literal(_)));
    assert_eq!(options.search_path(), Path::new("."));
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

/// Ensures over-long path headings are capped at the display budget with an
/// ellipsis, matching how match body lines are truncated, so every rendered
/// line stays within `GREP_MAX_LINE_LENGTH`.
#[test]
fn grep_heading_caps_overlong_path() {
    let long_path = format!("{}pad", "p".repeat(GREP_MAX_LINE_LENGTH));
    let (heading, truncated) = render_grep_heading(&long_path);
    assert!(truncated);
    assert!(heading.ends_with('…'));
    assert!(heading.len() <= GREP_MAX_LINE_LENGTH);
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
