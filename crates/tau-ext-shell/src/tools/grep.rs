//! `grep` tool: in-process, ignore-aware line search.

mod search;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use tau_proto::CborValue;

use crate::argument::{
    argument_text, optional_argument_bool, optional_argument_int_strict, optional_argument_text,
};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::tools::CancellableToolRun;
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_head};

pub(crate) const DEFAULT_GREP_LIMIT: usize = 100;
pub(crate) const GREP_MAX_LINE_LENGTH: usize = 500;
const MAX_GREP_LIMIT: usize = MAX_OUTPUT_LINES;
const MAX_GREP_CONTEXT: usize = 20;

pub(crate) fn run_grep(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    match run_grep_cancellable(arguments, None)? {
        CancellableToolRun::Finished(output) => Ok(*output),
        CancellableToolRun::Cancelled => Err(ToolFailure::new("cancelled")),
    }
}

pub(crate) fn run_grep_cancellable(
    arguments: &CborValue,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CancellableToolRun, ToolFailure> {
    let options = GrepOptions::parse(arguments)?;
    let display_args = options.display_args();
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let stream = match search::search(&options, cancel_rx).map_err(with_args)? {
        Some(stream) => stream,
        None => return Ok(CancellableToolRun::Cancelled),
    };
    let status = if stream.match_count > 0 { 0 } else { 1 };
    Ok(CancellableToolRun::Finished(Box::new(render_grep_output(
        stream,
        status,
        display_args,
        options.limit,
    ))))
}

/// Parsed model-facing grep arguments after validation and defaults.
struct GrepOptions {
    /// Search-pattern mode and text.
    pattern: GrepPattern,
    /// Optional user-supplied search root; defaults to the current directory.
    path: Option<PathBuf>,
    /// Optional ignore-aware glob override.
    glob: Option<String>,
    /// Whether matching should ignore case.
    ignore_case: bool,
    /// Optional number of context lines requested around each match.
    context: Option<usize>,
    /// Maximum number of match records to render before stopping search.
    limit: usize,
}

/// A grep pattern with its explicit matching mode.
enum GrepPattern {
    /// Match the pattern text as a fixed string.
    Literal(String),
    /// Interpret the pattern text as a regular expression.
    Regex(String),
}

impl GrepPattern {
    fn text(&self) -> &str {
        match self {
            Self::Literal(text) | Self::Regex(text) => text,
        }
    }
}

impl GrepOptions {
    fn parse(arguments: &CborValue) -> Result<Self, ToolFailure> {
        let pattern = argument_text(arguments, "pattern")?;
        let path = optional_argument_text(arguments, "path")?.map(PathBuf::from);
        let glob = optional_argument_text(arguments, "glob")?;
        let ignore_case = optional_bool_argument(arguments, "ignoreCase")?;
        // Literal matching is the default. Most callers are searching for
        // an exact string and regex metacharacters in that string (`[`,
        // `(`, `.`, `?`, `+`, `*`, `|`, `{`, `\`) would otherwise either
        // fail to parse or silently match something unintended. Regex
        // users opt in explicitly with `regex: true`.
        let pattern = match optional_bool_argument(arguments, "regex")? {
            true => GrepPattern::Regex(pattern),
            false => GrepPattern::Literal(pattern),
        };
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
            context,
            limit,
        })
    }

    fn search_path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new("."))
    }

    fn display_args(&self) -> String {
        match self.glob.as_deref() {
            Some(g) => format!(
                "{:?} in {} [{g}]",
                self.pattern.text(),
                self.search_path().display()
            ),
            None => format!(
                "{:?} in {}",
                self.pattern.text(),
                self.search_path().display()
            ),
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

fn render_grep_output(
    stream: GrepStreamResult,
    status: i32,
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
            provider_content: Vec::new(),
            display,
        };
    }

    let total_output_lines = result_lines.len();
    let full_output_text = result_lines.join("\n");

    // Apply byte-level truncation to the assembled output.
    let byte_truncated = truncate_head(&full_output_text);
    let mut output_text = if byte_truncated.was_truncated {
        byte_truncated.content
    } else {
        full_output_text.clone()
    };

    // Build notices.
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(limit_reached_notice(limit));
    }
    if byte_truncated.was_truncated {
        notices.push("10 KiB visible output limit reached.".to_owned());
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
    let mut result = grep_result_map(status, match_count, output_text);
    if byte_truncated.was_truncated
        && let CborValue::Map(entries) = &mut result
    {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        entries.push((
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_output_lines as i64).into()),
        ));
        entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((full_output_text.len() as i64).into()),
        ));
        crate::shell_output_spool::append_metadata(entries, &full_output_text);
    }
    ToolOutput {
        result,
        provider_content: Vec::new(),
        display,
    }
}

fn limit_reached_notice(limit: usize) -> String {
    if MAX_GREP_LIMIT <= limit {
        format!("{limit} matches limit reached. Maximum limit reached; refine pattern.")
    } else {
        format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern.",
            (limit * 2).min(MAX_GREP_LIMIT)
        )
    }
}

/// Accumulated, bounded per-line search rendering.
struct GrepStreamResult {
    /// Path headings and line bodies.
    result_lines: Vec<String>,
    /// Number of rendered matching lines.
    match_count: usize,
    /// Whether an individual heading or body exceeded its display bound.
    lines_truncated: bool,
    /// Whether the extra match proved more results remain.
    match_limit_reached: bool,
}

/// Build the CBOR result map for `grep` without echoing request arguments.
/// Call context such as `pattern`, `path`, and `glob` is already available to
/// callers from the tool invocation; repeating it in the result wastes tokens.
pub(crate) fn grep_result_map(status: i32, matches: usize, output_text: String) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            CborValue::Integer((status as i64).into()),
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

/// Render a per-file path heading line, capping over-long paths at the
/// display budget with an ellipsis so every rendered line, heading included,
/// stays within `GREP_MAX_LINE_LENGTH`.
fn render_grep_heading(path: &str) -> (String, bool) {
    if path.len() <= GREP_MAX_LINE_LENGTH {
        return (path.to_owned(), false);
    }
    let ellipsis = "…";
    // `GREP_MAX_LINE_LENGTH` (500) far exceeds the ellipsis size, so the
    // content budget below is always positive.
    let mut end = (GREP_MAX_LINE_LENGTH - ellipsis.len()).min(path.len());
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{ellipsis}", &path[..end]), true)
}

/// Render a single match or context body line beneath a per-file heading, as
/// `LINE:CONTENT` for matches and `LINE-CONTENT` for context lines. Long
/// content is truncated at the display budget with an ellipsis.
fn render_grep_line(lineno: u64, sep: char, text: &str) -> (String, bool) {
    let prefix = format!("{lineno}{sep}");
    let available = GREP_MAX_LINE_LENGTH - prefix.len();
    if text.len() <= available {
        return (format!("{prefix}{text}"), false);
    }
    let mut end = available - "…".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{prefix}{}…", &text[..end]), true)
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
