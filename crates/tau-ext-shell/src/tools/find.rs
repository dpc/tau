//! `find` tool: glob-based file search rooted at a directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use tau_proto::CborValue;

use crate::argument::{argument_text, optional_argument_int_strict, optional_argument_text};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::tools::CancellableToolRun;
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_line_oriented};

pub(crate) const DEFAULT_FIND_LIMIT: usize = 1000;
const MAX_FIND_LIMIT: usize = MAX_OUTPUT_LINES;

pub(crate) fn run_find(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    match run_find_cancellable(arguments, None)? {
        CancellableToolRun::Finished(output) => Ok(*output),
        CancellableToolRun::Cancelled => Err(ToolFailure::new("cancelled")),
    }
}

pub(crate) fn run_find_cancellable(
    arguments: &CborValue,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<CancellableToolRun, ToolFailure> {
    let request = parse_find_request(arguments)?;
    let search = prepare_find_search(&request)?;
    let mut cancelled = || cancel_rx.is_some_and(|rx| rx.try_recv().is_ok());
    let Some(matches) = collect_find_matches(&search, &mut cancelled)? else {
        return Ok(CancellableToolRun::Cancelled);
    };

    Ok(CancellableToolRun::Finished(Box::new(render_find_output(
        request, matches,
    ))))
}

/// Parsed and validated user request for the find tool.
struct FindRequest {
    /// Glob pattern used to match paths relative to the search root.
    pattern: String,
    /// Directory path supplied by the caller, defaulting to the current path.
    path: String,
    /// Maximum number of matches returned to the caller.
    limit: usize,
    /// Short argument summary rendered in UI state for successes and failures.
    display_args: String,
}

/// Prepared filesystem search parameters for the find tool.
struct FindSearch {
    /// Root directory walked by the ignore-aware iterator.
    path: PathBuf,
    /// Compiled glob matcher applied to paths relative to `path`.
    glob: GlobSet,
    /// Number of matching paths to collect, including the sentinel past
    /// `limit`.
    collection_cap: usize,
    /// Short argument summary rendered in UI state for failures.
    display_args: String,
}

fn parse_find_request(arguments: &CborValue) -> Result<FindRequest, ToolFailure> {
    let pattern = argument_text(arguments, "pattern").map_err(ToolFailure::from)?;
    let path = optional_argument_text(arguments, "path")
        .map_err(ToolFailure::from)?
        .unwrap_or_else(|| ".".to_owned());
    let limit = parse_find_limit(arguments)?;
    let display_args = format!("{pattern} in {}", PathBuf::from(&path).display());

    Ok(FindRequest {
        pattern,
        path,
        limit,
        display_args,
    })
}

fn parse_find_limit(arguments: &CborValue) -> Result<usize, ToolFailure> {
    let Some(value) =
        optional_argument_int_strict(arguments, "limit").map_err(ToolFailure::from)?
    else {
        return Ok(DEFAULT_FIND_LIMIT);
    };

    if value < 1 {
        return Err(ToolFailure::new("limit must be >= 1"));
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let limit = usize::try_from(value).map_err(|_| ToolFailure::new("limit is too large"))?;
    if MAX_FIND_LIMIT < limit {
        return Err(ToolFailure::new(format!(
            "limit must be <= {MAX_FIND_LIMIT}"
        )));
    }
    Ok(limit)
}

fn prepare_find_search(request: &FindRequest) -> Result<FindSearch, ToolFailure> {
    let path = PathBuf::from(&request.path);
    let metadata = fs::metadata(&path).map_err(|e| {
        find_failure_with_args(
            &request.display_args,
            format!("failed to access {}: {e}", path.display()),
        )
    })?;
    if !metadata.is_dir() {
        return Err(find_failure_with_args(
            &request.display_args,
            format!("not a directory: {}", path.display()),
        ));
    }

    let glob = compile_find_glob(&request.pattern)
        .map_err(|e| ToolFailure::from(e).with_args(request.display_args.clone()))?;

    Ok(FindSearch {
        path,
        glob,
        collection_cap: request.limit.saturating_add(1),
        display_args: request.display_args.clone(),
    })
}

fn find_failure_with_args(args: &str, message: impl Into<String>) -> ToolFailure {
    ToolFailure::new(message).with_args(args.to_owned())
}

fn collect_find_matches(
    search: &FindSearch,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<Vec<String>>, ToolFailure> {
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(&search.path)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        if cancelled() {
            return Ok(None);
        }
        let entry = entry.map_err(|e| {
            find_failure_with_args(
                &search.display_args,
                format!("failed to walk {}: {e}", search.path.display()),
            )
        })?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: match-option-verbose
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let Ok(relative_path) = entry.path().strip_prefix(&search.path) else {
            continue;
        };
        if search.glob.is_match(relative_path) {
            matches.push(path_to_slash(relative_path));
            if search.collection_cap <= matches.len() {
                break;
            }
        }
    }
    matches.sort_by_key(|entry| entry.to_lowercase());

    Ok(Some(matches))
}

fn render_find_output(request: FindRequest, matches: Vec<String>) -> ToolOutput {
    if matches.is_empty() {
        let mut display = crate::display::ok_display(request.display_args);
        display.stats.matches = Some(0);
        return ToolOutput {
            result: CborValue::Map(vec![
                (
                    CborValue::Text("matches".to_owned()),
                    CborValue::Integer(0.into()),
                ),
                (
                    CborValue::Text("output".to_owned()),
                    CborValue::Text("no files found matching pattern".to_owned()),
                ),
            ]),
            provider_content: Vec::new(),
            display,
        };
    }

    let observed_matches = matches.len();
    let displayed: Vec<String> = matches.into_iter().take(request.limit).collect();
    let limit_reached = observed_matches > displayed.len();
    let full_output_text = displayed.join("\n");
    let truncated = truncate_line_oriented(&full_output_text);
    let mut output_text = if truncated.was_truncated {
        truncated.content
    } else {
        full_output_text.clone()
    };

    let mut notices = Vec::new();
    if limit_reached {
        notices.push(limit_reached_notice(request.limit));
    }
    if truncated.was_truncated {
        notices.push("10 KiB/2000 line visible output limit reached.".to_owned());
    }

    output_text = append_notices_within_cap(output_text, &notices);

    let mut display = crate::display::ok_display(request.display_args);
    display.stats = text_stats(&output_text);
    let mut result_entries = vec![
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((displayed.len() as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text),
        ),
    ];
    if truncated.was_truncated {
        result_entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        result_entries.push((
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((displayed.len() as i64).into()),
        ));
        result_entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((full_output_text.len() as i64).into()),
        ));
        crate::shell_output_spool::append_metadata(&mut result_entries, &full_output_text);
    }
    if limit_reached {
        result_entries.push((
            CborValue::Text("limit_reached".to_owned()),
            CborValue::Bool(true),
        ));
    }
    ToolOutput {
        result: CborValue::Map(result_entries),
        provider_content: Vec::new(),
        display,
    }
}

fn limit_reached_notice(limit: usize) -> String {
    if MAX_FIND_LIMIT <= limit {
        format!("{limit} results limit reached. Maximum limit reached; refine pattern/path.")
    } else {
        format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern.",
            (limit * 2).min(MAX_FIND_LIMIT)
        )
    }
}

fn append_notices_within_cap(mut output_text: String, notices: &[String]) -> String {
    if notices.is_empty() {
        return output_text;
    }
    let notice = format!("\n\n[{}]", notices.join(" "));
    if output_text.len().saturating_add(notice.len()) <= MAX_OUTPUT_BYTES {
        output_text.push_str(&notice);
        return output_text;
    }
    let Some(budget) = MAX_OUTPUT_BYTES.checked_sub(notice.len()) else {
        return notice.chars().take(MAX_OUTPUT_BYTES).collect();
    };
    let mut end = budget.min(output_text.len());
    while !output_text.is_char_boundary(end) {
        end -= 1;
    }
    output_text.truncate(end);
    output_text.push_str(&notice);
    output_text
}

fn compile_find_glob(pattern: &str) -> Result<GlobSet, String> {
    let glob = Glob::new(pattern).map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder
        .build()
        .map_err(|e| format!("failed to compile glob pattern {pattern:?}: {e}"))
}

fn path_to_slash(path: &Path) -> String {
    render_path(path)
}

fn render_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        render_path_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        escape_path_text(&path.to_string_lossy())
    }
}

pub(crate) fn render_path_bytes(bytes: &[u8]) -> String {
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: match-result-verbose
    match std::str::from_utf8(bytes) {
        Ok(text) => escape_path_text(text),
        Err(_) => format!(
            "(invalid-utf8) {}",
            escape_path_text(&String::from_utf8_lossy(bytes))
        ),
    }
}

pub(crate) fn escape_path_text(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.extend(ch.escape_default()),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
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

    /// Ensures find rejects non-positive limits instead of coercing them to a
    /// surprising positive default.
    #[test]
    fn find_rejects_non_positive_limit() {
        let err = run_find(&args(CborValue::Integer(0.into())))
            .expect_err("zero limit should be rejected");

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
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
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
            path: tempdir.path().display().to_string(),
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
}
