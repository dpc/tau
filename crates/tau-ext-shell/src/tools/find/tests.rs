use std::sync as path_std_sync;

use super::*;

fn args(limit: CborValue) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*".to_owned()),
        ),
        (CborValue::Text("limit".to_owned()), limit),
    ])
}

/// Ensures find rejects wrong-typed limits instead of silently using the
/// default result cap.
#[test]
fn find_rejects_wrong_type_limit() {
    let err = run_find(&args(CborValue::Text("10".to_owned())))
        .expect_err("string limit should be rejected");

    assert_eq!(err.message, "argument `limit` must be an integer");
}

/// Ensures find rejects wrong-typed paths instead of searching the default
/// directory.
#[test]
fn find_rejects_wrong_type_path() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let err = run_find(&args).expect_err("integer path should be rejected");

    assert_eq!(err.message, "argument `path` must be a string");
}

/// Ensures find converts the decoded UTF-8 root once, preserving both the
/// current-directory default and the existing display text.
#[test]
fn find_parse_retains_typed_root_and_display() {
    let omitted = parse_find_request(&CborValue::Map(vec![(
        CborValue::Text("pattern".to_owned()),
        CborValue::Text("*.rs".to_owned()),
    )]))
    .expect("omitted path parses");
    assert_eq!(omitted.path, PathBuf::from("."));
    assert_eq!(omitted.display_args, "*.rs in .");

    let explicit_path = "./search-root/suffix";
    let explicit = parse_find_request(&CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(explicit_path.to_owned()),
        ),
    ]))
    .expect("explicit path parses");
    assert_eq!(explicit.path, PathBuf::from(explicit_path));
    assert_eq!(explicit.display_args, "*.rs in ./search-root/suffix");
}

/// Ensures find rejects non-positive limits instead of coercing them to a
/// surprising positive default.
#[test]
fn find_rejects_non_positive_limit() {
    let err =
        run_find(&args(CborValue::Integer(0.into()))).expect_err("zero limit should be rejected");

    assert_eq!(err.message, "limit must be >= 1");
}

/// Ensures find reports only the requested number of matches while still
/// detecting the sentinel match needed to add the user-visible limit
/// notice.
#[test]
fn find_limit_bounds_collected_matches() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
        std::fs::write(tempdir.path().join(name), "x").expect("write file");
    }
    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*.txt".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
        (
            CborValue::Text("limit".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let result = run_find(&args).expect("find").result;
    let CborValue::Map(entries) = result else {
        panic!("expected result map");
    };
    let output = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value)) if key == "output" => Some(value),
            _ => None,
        })
        .expect("output");
    let matches: i64 = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Integer(value)) if key == "matches" => {
                i128::from(*value).try_into().ok()
            }
            _ => None,
        })
        .expect("matches");
    let limit_reached = entries.iter().any(|(key, value)| {
        matches!(
            (key, value),
            (CborValue::Text(key), CborValue::Bool(true)) if key == "limit_reached"
        )
    });

    assert_eq!(
        output.lines().take_while(|line| !line.is_empty()).count(),
        1
    );
    assert_eq!(matches, 1);
    assert!(limit_reached);
    assert!(output.contains("1 results limit reached"));
}

/// Ensures large caller limits cannot force collection far beyond the
/// documented display cap before final output truncation.
#[test]
fn find_rejects_limit_above_output_cap() {
    let err = run_find(&args(CborValue::Integer(
        (MAX_FIND_LIMIT as i64 + 1).into(),
    )))
    .expect_err("limit over cap");

    assert_eq!(err.message, format!("limit must be <= {MAX_FIND_LIMIT}"));
}

/// Ensures max-limit notices do not suggest limits that argument parsing
/// rejects.
#[test]
fn find_max_limit_notice_asks_to_refine() {
    let notice = limit_reached_notice(MAX_FIND_LIMIT);

    assert!(notice.contains("Maximum limit reached"));
    assert!(!notice.contains(&format!("limit={}", MAX_FIND_LIMIT * 2)));
}

/// Ensures find observes cancellation before starting traversal through the
/// cancellable tool path.
#[test]
fn find_cancellable_stops_on_early_cancel_request() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha.txt"), "x").expect("write file");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*.txt".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let (cancel_tx, cancel_rx) = path_std_sync::mpsc::channel();
    cancel_tx.send(()).expect("send cancel");

    let result = run_find_cancellable(&args, Some(&cancel_rx)).expect("find result");

    assert!(matches!(result, CancellableToolRun::Cancelled));
}

/// Ensures active find traversal checks cancellation between walked
/// entries, so a running search has a deterministic path to stop before
/// the whole tree has been visited.
#[test]
fn find_collect_stops_when_cancelled_during_traversal() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("alpha.txt"), "x").expect("write alpha");
    std::fs::write(tempdir.path().join("beta.txt"), "x").expect("write beta");
    let request = FindRequest {
        pattern: "*.txt".to_owned(),
        path: tempdir.path().to_owned(),
        limit: DEFAULT_FIND_LIMIT,
        display_args: "test".to_owned(),
    };
    let search = prepare_find_search(&request).expect("search");
    let mut checks = 0usize;

    let result = collect_find_matches(&search, &mut || {
        checks += 1;
        1 < checks
    })
    .expect("find collection");

    assert!(result.is_none());
}

/// Protects find output from path line injection by escaping control
/// characters before rendering file names as one logical record per line.
#[test]
fn find_escapes_control_characters_in_paths() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("line\nbreak.txt"), "x").expect("write file");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("*.txt".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);

    let result = run_find(&args).expect("find").result;
    let CborValue::Map(entries) = result else {
        panic!("expected result map");
    };
    let output = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Text(value)) if key == "output" => Some(value),
            _ => None,
        })
        .expect("output");

    assert_eq!(output, "line\\nbreak.txt");
}
/// Ensures find notices are included without exceeding the documented 10
/// KiB output budget.
#[test]
fn find_notices_stay_within_output_cap() {
    let notice = "10 KiB/2000 line visible output limit reached.".to_owned();
    let suffix_len = format!("\n\n[{notice}]").len();
    let output = append_notices_within_cap(
        format!("{}étail", "x".repeat(MAX_OUTPUT_BYTES - suffix_len - 1)),
        std::slice::from_ref(&notice),
    );

    assert!(output.len() <= MAX_OUTPUT_BYTES);
    assert!(output.contains(&notice));
}
