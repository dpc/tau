//! `grep` tool: ripgrep-backed search using `rg --json`.

use std::fmt;
use std::io::{BufReader, Read};
use std::process::Command;

use tau_proto::CborValue;

use crate::argument::{
    argument_text, optional_argument_bool, optional_argument_int_strict, optional_argument_text,
};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::isolation::apply_command_isolation;
use crate::tools::find::{escape_path_text, render_path_bytes};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_head};

pub(crate) const DEFAULT_GREP_LIMIT: usize = 100;
pub(crate) const GREP_MAX_LINE_LENGTH: usize = 500;
const MAX_GREP_LIMIT: usize = MAX_OUTPUT_LINES;
const MAX_GREP_CONTEXT: usize = 20;

pub(crate) fn run_grep(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    let options = GrepOptions::parse(arguments)?;
    let display_args = options.display_args();
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let GrepProcessOutput {
        stream,
        status,
        stderr,
    } = run_ripgrep(&options).map_err(with_args)?;

    // rg exit codes: 0=matches found, 1=no matches, 2=error.
    // Exit-2 is overloaded — ripgrep emits regex parse errors, IO
    // errors, and permission denials all under the same code. Classify
    // the stderr into a short, single-line message so the UI doesn't
    // surface a multi-line regex-parser dump in the inline tool block.
    if status == Some(2) {
        let stderr_raw = String::from_utf8_lossy(&stderr);
        return Err(with_args(ToolFailure::from(
            classify_ripgrep_stderr(stderr_raw.trim()).to_string(),
        )));
    }

    Ok(render_grep_output(
        stream,
        status,
        display_args,
        options.limit,
    ))
}

/// Parsed model-facing grep arguments after validation and defaults.
struct GrepOptions {
    /// Search pattern passed to ripgrep after the `--` separator.
    pattern: String,
    /// Optional user-supplied search root; defaults to the current directory.
    path: Option<String>,
    /// Optional ripgrep glob filter passed as `--glob`.
    glob: Option<String>,
    /// Whether matching should ignore case.
    ignore_case: bool,
    /// Whether `pattern` is a regular expression instead of a fixed string.
    regex: bool,
    /// Optional number of context lines requested around each match.
    context: Option<usize>,
    /// Maximum number of match records to render before stopping ripgrep.
    limit: usize,
}

impl GrepOptions {
    fn parse(arguments: &CborValue) -> Result<Self, ToolFailure> {
        let pattern = argument_text(arguments, "pattern")?;
        let path = optional_argument_text(arguments, "path")?;
        let glob = optional_argument_text(arguments, "glob")?;
        let ignore_case = optional_bool_argument(arguments, "ignoreCase")?;
        // Literal matching is the default. Most callers are searching for
        // an exact string and regex metacharacters in that string (`[`,
        // `(`, `.`, `?`, `+`, `*`, `|`, `{`, `\`) would otherwise either
        // fail to parse or silently match something unintended. Regex
        // users opt in explicitly with `regex: true`.
        let regex = optional_bool_argument(arguments, "regex")?;
        let context =
            optional_bounded_usize_argument(arguments, "context", 0, MAX_GREP_CONTEXT, None)?;
        let limit = optional_bounded_usize_argument(
            arguments,
            "limit",
            1,
            MAX_GREP_LIMIT,
            Some(DEFAULT_GREP_LIMIT),
        )?
        .expect("defaulted limit must be present");

        Ok(Self {
            pattern,
            path,
            glob,
            ignore_case,
            regex,
            context,
            limit,
        })
    }

    fn search_path(&self) -> &str {
        self.path.as_deref().unwrap_or(".")
    }

    fn display_args(&self) -> String {
        match self.glob.as_deref() {
            Some(g) => format!("{:?} in {} [{g}]", self.pattern, self.search_path()),
            None => format!("{:?} in {}", self.pattern, self.search_path()),
        }
    }

    fn ripgrep_args(&self) -> Vec<String> {
        // Use `--json` for structured output. This replaces the previous
        // hand-rolled `PATH:LINE:CONTENT` vs `PATH-LINE-CONTENT` line
        // classifier, which had a known misclassification mode on paths
        // like `file-12-34.txt`. The JSON envelope cleanly separates
        // match from context records.
        //
        // `--with-filename` is still needed to keep the path field
        // present when searching a single file, so the rendered output
        // continues to lead with `path:` even in that case.
        let mut args: Vec<String> = vec![
            "--json".to_owned(),
            "--hidden".to_owned(),
            "--with-filename".to_owned(),
            "--max-columns".to_owned(),
            GREP_MAX_LINE_LENGTH.to_string(),
            "--max-columns-preview".to_owned(),
        ];
        self.push_optional_ripgrep_args(&mut args);
        args.push("--".to_owned());
        args.push(self.pattern.clone());
        args.push(self.search_path().to_owned());
        args
    }

    fn push_optional_ripgrep_args(&self, args: &mut Vec<String>) {
        if self.ignore_case {
            args.push("--ignore-case".to_owned());
        }
        if !self.regex {
            args.push("--fixed-strings".to_owned());
        }
        if let Some(glob) = &self.glob {
            args.push("--glob".to_owned());
            args.push(glob.clone());
        }
        if let Some(context) = self.context {
            args.push(format!("--context={context}"));
        }
    }
}

fn optional_bool_argument(arguments: &CborValue, name: &str) -> Result<bool, ToolFailure> {
    Ok(optional_argument_bool(arguments, name)
        .map_err(ToolFailure::from)?
        .unwrap_or(false))
}

fn optional_bounded_usize_argument(
    arguments: &CborValue,
    name: &str,
    min: usize,
    max: usize,
    default: Option<usize>,
) -> Result<Option<usize>, ToolFailure> {
    let Some(value) = optional_argument_int_strict(arguments, name).map_err(ToolFailure::from)?
    else {
        return Ok(default);
    };
    let min_i64 = i64::try_from(min).expect("grep bounds fit in i64");
    if value < min_i64 {
        return Err(ToolFailure::new(format!("{name} must be >= {min}")));
    }
    let value =
        usize::try_from(value).map_err(|_| ToolFailure::new(format!("{name} is too large")))?;
    if max < value {
        return Err(ToolFailure::new(format!("{name} must be <= {max}")));
    }
    Ok(Some(value))
}

struct GrepProcessOutput {
    /// Rendered stream records and truncation/limit metadata.
    stream: GrepStreamResult,
    /// Process exit status code, or `None` if ripgrep was signal-terminated.
    status: Option<i32>,
    /// Bounded stderr bytes captured for exit-code classification.
    stderr: Vec<u8>,
}

fn run_ripgrep(options: &GrepOptions) -> Result<GrepProcessOutput, ToolFailure> {
    let mut cmd = Command::new("rg");
    cmd.args(options.ripgrep_args())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_command_isolation(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolFailure::from(format!("failed to start ripgrep: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolFailure::from("ripgrep stdout pipe missing".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolFailure::from("ripgrep stderr pipe missing".to_owned()))?;
    let stderr_handle = std::thread::spawn(move || read_limited_bytes(stderr, MAX_OUTPUT_BYTES));

    let stream = read_grep_json(stdout, options.limit);

    // If the limit fired we may have killed reading mid-stream; make
    // sure the child does not linger.
    if stream.match_limit_reached {
        let _ = child.kill();
    }

    let exit_status = child
        .wait()
        .map_err(|e| ToolFailure::from(format!("failed to wait for ripgrep: {e}")))?;
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(GrepProcessOutput {
        stream,
        status: exit_status.code(),
        stderr,
    })
}

fn render_grep_output(
    stream: GrepStreamResult,
    status: Option<i32>,
    display_args: String,
    limit: usize,
) -> ToolOutput {
    let GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    } = stream;

    if result_lines.is_empty() {
        let mut display = crate::display::ok_display(display_args.clone());
        display.stats.matches = Some(0);
        return ToolOutput {
            result: grep_result_map(status, 0, "no matches found".to_owned()),
            display,
        };
    }

    let mut output_text = result_lines.join("\n");

    // Apply byte-level truncation to the assembled output.
    let byte_truncated = truncate_head(&output_text);
    if byte_truncated.was_truncated {
        output_text = byte_truncated.content;
    }

    // Build notices.
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(limit_reached_notice(limit));
    }
    if byte_truncated.was_truncated {
        notices.push("50KB output limit reached.".to_owned());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines."
        ));
    }

    output_text = append_notices_within_cap(output_text, &notices);

    let mut display = crate::display::ok_display(display_args);
    display.stats = text_stats(&output_text);
    display.stats.matches = Some(match_count as u64);
    ToolOutput {
        result: grep_result_map(status, match_count, output_text),
        display,
    }
}

fn limit_reached_notice(limit: usize) -> String {
    if limit >= MAX_GREP_LIMIT {
        format!("{limit} matches limit reached. Maximum limit reached; refine pattern.")
    } else {
        format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern.",
            (limit * 2).min(MAX_GREP_LIMIT)
        )
    }
}

fn read_limited_bytes(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if output.len() < limit {
                    let remaining = limit - output.len();
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
        }
    }
    output
}

/// Categorized ripgrep failure (exit code 2). The variants encode the
/// kind of fault; the `Display` impl produces the short single-line
/// message we surface as the tool error. Untagged callers stringify
/// this via `to_string()`. When the unified tool-usage descriptor
/// lands, the variants can be mapped to its `status` field directly
/// instead of being flattened to a string.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RipgrepError {
    /// Bad regex / pattern from the agent. Carries ripgrep's trailing
    /// `error: <diagnostic>` line (e.g. `unclosed group`) when found.
    Usage {
        detail: String,
    },
    NotFound,
    Permission,
    /// Anything else. Carries the first non-empty stderr line so the
    /// chip stays readable but we don't lose the signal entirely.
    Runtime {
        detail: String,
    },
}

impl fmt::Display for RipgrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage { detail } if !detail.is_empty() => {
                write!(f, "regex parse error: {detail}")
            }
            Self::Usage { .. } => f.write_str("regex parse error"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::Permission => f.write_str("permission denied"),
            Self::Runtime { detail } if !detail.is_empty() => {
                write!(f, "ripgrep error: {detail}")
            }
            Self::Runtime { .. } => f.write_str("ripgrep error"),
        }
    }
}

/// Classify ripgrep's stderr (exit code 2). ripgrep prints stable,
/// well-known prefixes for each failure class — `regex parse error:`
/// for a bad pattern from the agent, and the OS-error suffix
/// (`(os error 2)` / `(os error 13)`) for not-found and
/// permission-denied — so we can label these without parsing
/// arbitrary downstream text.
pub(crate) fn classify_ripgrep_stderr(stderr: &str) -> RipgrepError {
    if stderr.contains("regex parse error")
        || stderr.contains("error parsing regex")
        || stderr.contains("unrecognized escape sequence")
    {
        // ripgrep's regex-parser output puts the human-readable
        // diagnostic on a trailing `error: <text>` line; the header
        // and pattern/caret lines aren't useful for a one-line chip.
        let detail = stderr
            .lines()
            .filter_map(|l| l.trim().strip_prefix("error:"))
            .map(str::trim)
            .next_back()
            .unwrap_or("")
            .to_owned();
        return RipgrepError::Usage { detail };
    }
    if stderr.contains("(os error 2)") || stderr.contains("No such file or directory") {
        return RipgrepError::NotFound;
    }
    if stderr.contains("(os error 13)") || stderr.contains("Permission denied") {
        return RipgrepError::Permission;
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned();
    RipgrepError::Runtime { detail }
}

/// Result of streaming and rendering rg's `--json` output.
struct GrepStreamResult {
    result_lines: Vec<String>,
    match_count: usize,
    lines_truncated: bool,
    match_limit_reached: bool,
}

/// Minimal rg `--json` envelope. Only the fields we render are
/// deserialized; everything else is dropped.
#[derive(serde::Deserialize)]
struct RgRecord {
    #[serde(rename = "type")]
    kind: String,
    data: RgData,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgData {
    path: Option<RgText>,
    lines: Option<RgText>,
    line_number: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgText {
    text: Option<String>,
    bytes: Option<String>,
}

impl RgText {
    fn render_path(&self) -> Option<String> {
        if let Some(text) = &self.text {
            return Some(escape_path_text(text));
        }
        self.decoded_bytes().map(|bytes| render_path_bytes(&bytes))
    }

    fn text_lossy(self) -> Option<String> {
        if let Some(text) = self.text {
            return Some(text);
        }
        self.decoded_bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn decoded_bytes(&self) -> Option<Vec<u8>> {
        let bytes = self.bytes.as_ref()?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, bytes).ok()
    }
}

/// Stream rg's JSON Lines output, build the legacy
/// `PATH:LINE:CONTENT` / `PATH-LINE-CONTENT` rendering, and break
/// early once the match limit is reached.
fn read_grep_json<R: Read>(stdout: R, limit: usize) -> GrepStreamResult {
    use std::io::BufRead as _;
    let reader = BufReader::new(stdout);
    let mut result_lines = Vec::new();
    let mut match_count = 0usize;
    let mut lines_truncated = false;
    let mut match_limit_reached = false;
    let mut current_path: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<RgRecord>(&line) else {
            continue;
        };
        match record.kind.as_str() {
            "begin" => {
                current_path = record.data.path.as_ref().and_then(RgText::render_path);
            }
            "match" | "context" => {
                let path = record
                    .data
                    .path
                    .as_ref()
                    .and_then(RgText::render_path)
                    .or_else(|| current_path.clone())
                    .unwrap_or_default();
                let lineno = record.data.line_number.unwrap_or(0);
                let text = record
                    .data
                    .lines
                    .and_then(RgText::text_lossy)
                    .unwrap_or_default();
                let text = strip_eol(&text);
                let is_match = record.kind == "match";
                if is_match {
                    if limit <= match_count {
                        match_limit_reached = true;
                        break;
                    }
                    match_count += 1;
                }
                let sep = if is_match { ':' } else { '-' };
                let (rendered, truncated) = render_grep_line(&path, lineno, sep, text);
                if truncated {
                    lines_truncated = true;
                }
                result_lines.push(rendered);
            }
            _ => {}
        }
    }

    GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    }
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Build the CBOR result map for `grep` without echoing request arguments.
/// Call context such as `pattern`, `path`, and `glob` is already available to
/// callers from the tool invocation; repeating it in the result wastes tokens.
pub(crate) fn grep_result_map(
    status: Option<i32>,
    matches: usize,
    output_text: String,
) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            status
                .map(|code| CborValue::Integer((code as i64).into()))
                .unwrap_or(CborValue::Null),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text.clone()),
        ),
        (
            CborValue::Text("output_lines".to_owned()),
            CborValue::Integer((output_text.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("output_bytes".to_owned()),
            CborValue::Integer((output_text.len() as i64).into()),
        ),
    ])
}

fn render_grep_line(path: &str, lineno: u64, sep: char, text: &str) -> (String, bool) {
    let prefix = format!("{path}{sep}{lineno}{sep}");
    let rendered = format!("{prefix}{text}");
    if rendered.len() <= GREP_MAX_LINE_LENGTH {
        return (rendered, false);
    }

    let ellipsis = "…";
    let Some(text_budget) = GREP_MAX_LINE_LENGTH.checked_sub(prefix.len() + ellipsis.len()) else {
        let marker = "(truncated)";
        let mut end = GREP_MAX_LINE_LENGTH
            .saturating_sub(marker.len())
            .min(prefix.len());
        while !prefix.is_char_boundary(end) {
            end -= 1;
        }
        return (format!("{}{marker}", &prefix[..end]), true);
    };
    let mut end = text_budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{prefix}{}{}", &text[..end], ellipsis), true)
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

#[cfg(test)]
mod tests {
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
        let captured =
            read_limited_bytes(std::io::Cursor::new(vec![b'x'; MAX_OUTPUT_BYTES + 100]), 32);

        assert_eq!(captured.len(), 32);
        assert!(captured.iter().all(|byte| *byte == b'x'));
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

        assert_eq!(output.result_lines, vec!["line\\nbreak.txt:7:needle"]);
    }

    /// Ensures grep handles ripgrep byte paths without silently dropping the
    /// record, marking invalid UTF-8 while preserving a lossy escaped path.
    #[test]
    fn grep_renders_invalid_utf8_byte_paths() {
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
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
            vec!["(invalid-utf8) bad�name.txt:3:needle"]
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
        assert_eq!(output.result_lines, vec!["file.txt:1:needle one"]);
    }

    /// Ensures grep long-line shortening preserves the path and line number
    /// prefix instead of replacing the whole rendered match with a marker.
    #[test]
    fn grep_long_line_truncation_preserves_location_prefix() {
        let (line, truncated) = render_grep_line("path/to/file.txt", 42, ':', &"x".repeat(1000));

        assert!(truncated);
        assert!(
            line.starts_with("path/to/file.txt:42:"),
            "line was {line:?}"
        );
        assert!(line.ends_with('…'));
        assert!(line.len() <= GREP_MAX_LINE_LENGTH);
    }

    /// Ensures very long path prefixes are capped too, while preserving as much
    /// location information as possible.
    #[test]
    fn grep_long_prefix_truncation_stays_within_line_cap() {
        let (line, truncated) = render_grep_line(&"p".repeat(1000), 42, ':', "match");

        assert!(truncated);
        assert!(line.ends_with("(truncated)"));
        assert!(line.len() <= GREP_MAX_LINE_LENGTH);
    }

    /// Ensures grep notices are included without exceeding the documented 50KB
    /// output budget.
    #[test]
    fn grep_notices_stay_within_output_cap() {
        let notice = "50KB output limit reached.".to_owned();
        let suffix_len = format!("\n\n[{notice}]").len();
        let output = append_notices_within_cap(
            format!("{}étail", "x".repeat(MAX_OUTPUT_BYTES - suffix_len - 1)),
            std::slice::from_ref(&notice),
        );

        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(output.contains(&notice));
    }
}
