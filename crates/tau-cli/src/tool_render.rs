//! Theming and block rendering for tool calls and other transcript
//! elements. Pure functions over [`tau_proto`] payloads — no
//! [`tau_cli_term`] state lives here.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStatus, cbor_field};

#[cfg(test)]
pub(crate) fn format_turn_stats_line(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> String {
    turn_stats_parts(usage, previous_usage, turn_latency, total_latency)
        .into_iter()
        .map(|part| part.text)
        .collect()
}

pub(crate) fn render_turn_stats_block(
    theme: &tau_themes::Theme,
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let mut themed = ThemedText::new();
    let root = themed.add_style(names::TOKEN_STATS);
    let mut children = Vec::new();
    for part in turn_stats_parts(usage, previous_usage, turn_latency, total_latency) {
        let style = themed.add_style(part.style_name);
        children.push(SpanTree::span(style, vec![SpanTree::text(part.text)]));
    }
    themed.push_tree(SpanTree::span(root, children));
    tau_cli_term::StyledBlock::new(themed_text(theme, &themed))
}

const CACHE_HIT_WARNING_PERCENT: u8 = 90;
// Prompt-cache hits only accrue in coarse provider cache blocks; allow
// the last partial block to miss without flagging the turn.
const CACHE_GRANULARITY_TOKENS: u64 = 512;

struct TurnStatsPart {
    text: String,
    style_name: &'static str,
}

impl TurnStatsPart {
    fn new(text: impl Into<String>, style_name: &'static str) -> Self {
        Self {
            text: text.into(),
            style_name,
        }
    }
}

fn turn_stats_parts(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> Vec<TurnStatsPart> {
    use tau_themes::names;

    let previous_sent_tokens = previous_usage.map_or(0, |usage| usage.prompt_sent_tokens);
    let previous_received_tokens = previous_usage.map_or(0, |usage| usage.response_received_tokens);
    let turn_cache_possible = previous_sent_tokens.saturating_add(previous_received_tokens);
    let new_prompt_tokens = usage.prompt_sent_tokens.saturating_sub(turn_cache_possible);
    let mut parts = Vec::new();

    parts.push(TurnStatsPart::new("Δ", names::TOKEN_STATS_DELTA));
    let turn_cache_hit_percent =
        cache_hit_percent(Some(turn_cache_possible), Some(usage.prompt_cached_tokens)).unwrap_or(0);
    parts.push(TurnStatsPart::new(
        format!(
            "{turn_cache_hit_percent}% {}/{}",
            format_token_count(usage.prompt_cached_tokens),
            format_token_count(turn_cache_possible),
        ),
        cache_hit_style_name(turn_cache_possible, usage.prompt_cached_tokens),
    ));
    parts.push(TurnStatsPart::new(" ↑", names::TOKEN_STATS_UP));
    parts.push(TurnStatsPart::new(
        format_token_count(new_prompt_tokens),
        names::TOKEN_STATS_INPUT,
    ));
    parts.push(TurnStatsPart::new(" ↓", names::TOKEN_STATS_DOWN));
    parts.push(TurnStatsPart::new(
        format_token_count(usage.response_received_tokens),
        names::TOKEN_STATS_OUTPUT,
    ));
    if let Some(latency) = turn_latency {
        parts.push(TurnStatsPart::new(
            format!(" {}", StatusBarDuration(latency)),
            names::TOKEN_STATS_LATENCY,
        ));
    }

    parts.push(TurnStatsPart::new(" Σ", names::TOKEN_STATS_SIGMA));
    parts.push(TurnStatsPart::new("↑", names::TOKEN_STATS_UP));
    parts.push(TurnStatsPart::new(
        format!(
            "{}/{}",
            format_token_count(usage.stats.total.cached_tokens),
            format_token_count(usage.stats.total.sent_tokens),
        ),
        names::TOKEN_STATS_INPUT,
    ));
    parts.push(TurnStatsPart::new(" ↓", names::TOKEN_STATS_DOWN));
    parts.push(TurnStatsPart::new(
        format_token_count(usage.stats.total.received_tokens),
        names::TOKEN_STATS_OUTPUT,
    ));
    if let Some(latency) = total_latency {
        parts.push(TurnStatsPart::new(
            format!(" {}", StatusBarDuration(latency)),
            names::TOKEN_STATS_LATENCY,
        ));
    }

    parts
}

struct StatusBarDuration(Duration);

impl fmt::Display for StatusBarDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MILLIS_MAX: Duration = Duration::from_secs(5);
        const SECONDS_MAX: Duration = Duration::from_secs(5 * 60);

        if self.0 < MILLIS_MAX {
            write!(f, "{}ms", self.0.as_millis())
        } else if self.0 < SECONDS_MAX {
            write!(f, "{}s", self.0.as_secs())
        } else {
            write!(f, "{}m", self.0.as_secs() / 60)
        }
    }
}

fn cache_hit_style_name(possible_cached_tokens: u64, cached_tokens: u64) -> &'static str {
    use tau_themes::names;

    let cacheable_prefix_floor =
        possible_cached_tokens / CACHE_GRANULARITY_TOKENS * CACHE_GRANULARITY_TOKENS;
    if cacheable_prefix_floor <= cached_tokens {
        names::TOKEN_STATS_CACHE_HIT
    } else if cache_hit_percent(Some(possible_cached_tokens), Some(cached_tokens))
        .is_some_and(|percent| CACHE_HIT_WARNING_PERCENT < percent)
    {
        names::TOKEN_STATS_CACHE_WARN
    } else {
        names::TOKEN_STATS_CACHE_MISS
    }
}

pub(crate) fn cache_hit_percent(
    possible_cached_tokens: Option<u64>,
    cached_tokens: Option<u64>,
) -> Option<u8> {
    let possible_cached_tokens = possible_cached_tokens?;
    let cached_tokens = cached_tokens?;
    if possible_cached_tokens == 0 {
        return Some(0);
    }
    let clamped_cached_tokens = cached_tokens.min(possible_cached_tokens);
    let percent = clamped_cached_tokens.saturating_mul(100) / possible_cached_tokens;
    Some(percent.min(100) as u8)
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenth = (tokens % 1_000) / 100;
        if tenth == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenth}k");
    }
    let whole = tokens / 1_000_000;
    let tenth = (tokens % 1_000_000) / 100_000;
    if tenth == 0 {
        return format!("{whole}m");
    }
    format!("{whole}.{tenth}m")
}

/// Format the `+N/-M` chip from a `DiffSummary` sub-tree on a tool
/// result as themed suffix segments. `+N` is painted with the
/// diff-added style and `-M` with the diff-removed style, matching
/// `git diff --shortstat`. The parens and slash stay in the muted info
/// Decode a `DiffSummary` sub-tree from a tool result, if present and
/// non-empty. Round-trips the CBOR sub-value through ciborium.
pub(crate) fn extract_diff(details: &CborValue) -> Option<tau_proto::DiffSummary> {
    let diff = cbor_field(details, "diff")?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(diff, &mut buf).ok()?;
    let summary: tau_proto::DiffSummary = ciborium::de::from_reader(buf.as_slice()).ok()?;
    if summary.added == 0 && summary.removed == 0 {
        return None;
    }
    Some(summary)
}

/// Which status-suffix style the completion block should use.
#[derive(Clone, Copy)]
pub(crate) enum ToolStatus {
    Success,
    Warning,
    Error,
    Pending,
    Info,
    Progress,
    DiffAdded,
    DiffRemoved,
    /// Agent id or legacy role suffix, painted like the status-bar role chip.
    Role,
    Context,
    Tools,
    Time,
}

/// Status variant for completed compaction lines. Kept separate from
/// tool-call display state because compaction is not a model-visible tool
/// invocation.
#[derive(Clone, Copy)]
pub(crate) enum CompactionStatus {
    Success,
    Progress,
}

#[derive(Clone)]
pub(crate) struct ToolSuffixSegment {
    pub(crate) text: String,
    pub(crate) status: ToolStatus,
    /// When true, suppress the implicit space the renderer normally
    /// inserts before this segment. Used to glue parts of a multi-span
    /// chip (e.g. the colored `+N/-M` diff stat) into one continuous
    /// run.
    pub(crate) no_leading_space: bool,
}

/// Decomposed tool-call label, painted as themed spans:
/// `<tool_name> <mode> <args> <range> <suffix...>`.
#[derive(Clone)]
pub(crate) struct ToolCallDisplay {
    pub(crate) tool_name: String,
    pub(crate) mode: String,
    pub(crate) args: String,
    pub(crate) range: Option<String>,
    pub(crate) suffixes: Vec<ToolSuffixSegment>,
    pub(crate) payload: Option<ToolUsePayload>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ToolSummaryDisplay {
    pub(crate) total: u64,
    pub(crate) completed: u64,
    pub(crate) ok: u64,
    pub(crate) err: u64,
    pub(crate) matches: u64,
    pub(crate) lines: u64,
    pub(crate) bytes: u64,
    pub(crate) added: u64,
    pub(crate) removed: u64,
}

/// Build the completion descriptor for a finished `agent_start` call by
/// carrying the cached progress (args + counters from the latest
/// [`tau_proto::DelegateProgress`]) and replacing the trailing
/// in-progress chip with output stats + the final `ok`/`err: message`
/// status. The input stats stay as a marked chip so delegate rendering
/// can show input first, then output, then progress counters.
pub(crate) fn build_delegate_completion_display(
    cached: Option<&ToolUseState>,
    details: &CborValue,
    error: Option<&str>,
) -> ToolUseState {
    let response_text = delegate_response_text(details);
    let mut display = cached.cloned().unwrap_or_else(|| ToolUseState {
        args: String::new(),
        ..Default::default()
    });
    let input_stats = display.stats;
    display.stats = tau_proto::ToolUseStats::for_text(response_text);
    if !input_stats.is_empty() {
        display
            .info_chips
            .push(format!("↘︎{}", format_tool_use_state_stats(&input_stats)));
    }
    match error {
        Some(msg) if !msg.is_empty() => {
            display.status = ToolUseStatus::Error;
            display.status_text = first_error_line(msg);
        }
        _ => {
            display.status = ToolUseStatus::Success;
            display.status_text = "ok".to_owned();
        }
    }
    display
}

fn delegate_response_text(details: &CborValue) -> &str {
    match details {
        CborValue::Text(text) => text.as_str(),
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (CborValue::Text(key), CborValue::Text(text)) if key == "output" => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .unwrap_or_default(),
        _ => "",
    }
}

fn tool_suffix(text: String, status: ToolStatus) -> ToolSuffixSegment {
    ToolSuffixSegment {
        text,
        status,
        no_leading_space: false,
    }
}

pub(crate) fn pending_tool_call_display(tool_name: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        mode: String::new(),
        args: String::new(),
        range: None,
        suffixes: vec![tool_suffix("pending".to_owned(), ToolStatus::Pending)],
        payload: None,
    }
}
fn info_suffix(text: String) -> ToolSuffixSegment {
    tool_suffix(text, ToolStatus::Info)
}

/// Build a streaming block whose body uses `body_name` styling and
/// whose trailing `…` indicator uses [`names::PROGRESS_INDICATOR`], so
/// the indicator can be themed independently. The leading space before
/// the indicator is skipped when the body is empty or already ends in
/// whitespace, so the `…` doesn't double up whitespace or land one
/// column off the left margin on a fresh line.
pub(crate) fn streaming_block(
    theme: &tau_themes::Theme,
    body_name: &str,
    body_text: impl Into<String>,
) -> tau_cli_term::StyledBlock {
    streaming_block_with_indicator_suffix(theme, body_name, body_text, "")
}

pub(crate) fn streaming_block_with_indicator_suffix(
    theme: &tau_themes::Theme,
    body_name: &str,
    body_text: impl Into<String>,
    indicator_suffix: impl Into<String>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::{convert_color, resolve};
    use tau_cli_term::{Span, Style, StyledBlock, StyledText};
    use tau_themes::{StyleName, names};

    let body_text = body_text.into();
    let indicator_suffix = indicator_suffix.into();
    let needs_space = body_text
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());

    let body_ts = theme.resolve_style(&StyleName::new(body_name));
    let body_span_style = Style {
        fg: body_ts.fg.map(convert_color),
        bg: None,
        bold: body_ts.bold,
        underline: body_ts.underline,
        italic: body_ts.italic,
        strikethrough: body_ts.strikethrough,
    };
    let progress_style = resolve(theme, names::PROGRESS_INDICATOR);

    let mut spans = Vec::with_capacity(3);
    if !body_text.is_empty() {
        spans.push(Span::new(body_text, body_span_style));
    }
    if needs_space {
        spans.push(Span::new(" ", body_span_style));
    }
    spans.push(Span::new(
        tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        progress_style,
    ));
    if !indicator_suffix.is_empty() {
        spans.push(Span::new(indicator_suffix, progress_style));
    }

    let mut block = StyledBlock::new(StyledText::from(spans));
    if let Some(bg) = body_ts.bg {
        block = block.bg(convert_color(bg));
    }
    block
}

pub(crate) fn tool_duration_suffix(duration: Duration) -> ToolSuffixSegment {
    tool_suffix(format_tool_duration(duration), ToolStatus::Time)
}

pub(crate) fn format_tool_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

fn output_stats_suffix(text: &str) -> ToolSuffixSegment {
    info_suffix(format_stats(
        None,
        Some(text.lines().count() as u64),
        Some(text.len() as u64),
    ))
}

fn abbreviate_inline_text(text: &str) -> String {
    const EDGE_CHARS: usize = 20;

    let one_line = text.lines().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return one_line;
    }

    let head: String = chars.iter().take(EDGE_CHARS).copied().collect();
    let tail: String = chars
        .iter()
        .skip(chars.len() - EDGE_CHARS)
        .copied()
        .collect();
    format!("{head}┄{tail}")
}

/// Render an `agent_start` display with a dedicated suffix for the delegated
/// agent. Completed delegates show input stats (`↘︎`) before output stats (`↖︎`),
/// then progress counters and the final status. Cached descriptors may still
/// have ` +role` embedded in `args`; strip that legacy copy so the line does
/// not render both the old role chip and the new agent chip.
pub(crate) fn render_delegate_display(
    display: &ToolUseState,
    agent_id: Option<&str>,
    legacy_role: Option<&str>,
) -> ToolCallDisplay {
    let mut rendered = render_tool_use_state("agent_start", display);
    let stats_chip = format_tool_use_state_stats(&display.stats);
    if !stats_chip.is_empty() {
        let marker = match display.status {
            ToolUseStatus::InProgress => "↘︎",
            ToolUseStatus::Success | ToolUseStatus::Warning | ToolUseStatus::Error => "↖︎",
        };
        if let Some(suffix) = rendered
            .suffixes
            .iter_mut()
            .find(|suffix| suffix.text == stats_chip)
        {
            suffix.text = format!("{marker}{}", suffix.text);
        }
    }
    for suffix in &mut rendered.suffixes {
        normalize_delegate_input_stats_suffix(suffix);
    }
    if !matches!(display.status, ToolUseStatus::InProgress) {
        move_delegate_completion_stats_first(&mut rendered.suffixes, &stats_chip);
    }

    if let Some(role) = legacy_role.filter(|role| !role.is_empty()) {
        let legacy_suffix = format!(" +{role}");
        if let Some(args) = rendered.args.strip_suffix(&legacy_suffix) {
            rendered.args = args.to_owned();
        }
    }

    if let Some(agent_id) = agent_id.filter(|agent_id| !agent_id.is_empty()) {
        rendered
            .suffixes
            .insert(0, tool_suffix(format!("@{agent_id}"), ToolStatus::Role));
    } else if let Some(role) = legacy_role.filter(|role| !role.is_empty()) {
        rendered
            .suffixes
            .insert(0, tool_suffix(format!("+{role}"), ToolStatus::Role));
    }
    rendered
}

fn normalize_delegate_input_stats_suffix(suffix: &mut ToolSuffixSegment) {
    if !matches!(suffix.status, ToolStatus::Info) {
        return;
    }
    let normalized = suffix
        .text
        .strip_prefix("↘︎ ")
        .or_else(|| suffix.text.strip_prefix("↘︎"))
        .filter(|stats| !stats.is_empty())
        .map(|stats| format!("↘︎{stats}"));
    if let Some(normalized) = normalized {
        suffix.text = normalized;
    }
}

fn is_delegate_input_stats_suffix(suffix: &ToolSuffixSegment) -> bool {
    matches!(suffix.status, ToolStatus::Info)
        && suffix.text.starts_with("↘︎")
        && suffix.text.len() > "↘︎".len()
}

fn move_delegate_completion_stats_first(
    suffixes: &mut Vec<ToolSuffixSegment>,
    output_stats_chip: &str,
) {
    let mut input_stats = Vec::new();
    let mut rest = Vec::with_capacity(suffixes.len());
    for suffix in suffixes.drain(..) {
        if is_delegate_input_stats_suffix(&suffix) {
            input_stats.push(suffix);
        } else {
            rest.push(suffix);
        }
    }
    if input_stats.is_empty() {
        *suffixes = rest;
        return;
    }

    let output_stats_text =
        (!output_stats_chip.is_empty()).then(|| format!("↖︎{output_stats_chip}"));
    let insert_at = rest
        .iter()
        .position(|suffix| {
            output_stats_text
                .as_deref()
                .is_some_and(|text| suffix.text == text)
                || matches!(
                    suffix.status,
                    ToolStatus::Tools
                        | ToolStatus::Context
                        | ToolStatus::Success
                        | ToolStatus::Warning
                        | ToolStatus::Error
                        | ToolStatus::Progress
                )
        })
        .unwrap_or(rest.len());
    rest.splice(insert_at..insert_at, input_stats);
    *suffixes = rest;
}

/// Render a [`ToolUseState`] descriptor directly to a
/// [`ToolCallDisplay`]. The generic path the renderer takes when the
/// tool side attached a display descriptor to its result/error event —
/// no `match tool_name` arms needed. Falls back to
/// [`format_tool_completion`] for older events that didn't carry a
/// descriptor.
pub(crate) fn render_tool_use_state(tool_name: &str, display: &ToolUseState) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    let (added, removed) = display
        .payload
        .as_ref()
        .map(diff_payload_counts)
        .unwrap_or_default();
    if 0 < added {
        suffixes.push(tool_suffix(format!("+{added}"), ToolStatus::DiffAdded));
    }
    if 0 < removed {
        suffixes.push(ToolSuffixSegment {
            text: format!("-{removed}"),
            status: ToolStatus::DiffRemoved,
            no_leading_space: 0 < added,
        });
    }
    let stats_chip = format_tool_use_state_stats(&display.stats);
    if !stats_chip.is_empty() {
        suffixes.push(info_suffix(stats_chip));
    }
    for counter in &display.progress_counters {
        suffixes.push(format_progress_counter(counter));
    }
    for chip in &display.info_chips {
        suffixes.push(info_suffix(chip.clone()));
    }
    let status_kind = match display.status {
        ToolUseStatus::Success => ToolStatus::Success,
        ToolUseStatus::Warning => ToolStatus::Warning,
        ToolUseStatus::Error => ToolStatus::Error,
        ToolUseStatus::InProgress => ToolStatus::Progress,
    };
    let mut status_text =
        if display.status_text.is_empty() && matches!(display.status, ToolUseStatus::InProgress) {
            tau_proto::PROGRESS_INDICATOR_TEXT.to_owned()
        } else {
            display.status_text.clone()
        };
    if matches!(display.status, ToolUseStatus::Error) {
        status_text = error_status_text(&status_text);
    }
    suffixes.push(tool_suffix(status_text, status_kind));
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        mode: display.mode.clone(),
        args: display.args.clone(),
        range: display.range.as_ref().and_then(format_tool_use_range),
        suffixes,
        payload: display.payload.clone(),
    }
}

fn format_progress_counter(counter: &tau_proto::ProgressCounter) -> ToolSuffixSegment {
    let body = match counter.unit {
        tau_proto::ProgressUnit::Count => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!("{c}/{t}"),
            (Some(c), None) => c.to_string(),
            (None, Some(t)) => format!("-/{t}"),
            (None, None) => "-".to_owned(),
        },
        tau_proto::ProgressUnit::Percent => match (counter.complete, counter.total) {
            (Some(p), Some(t)) => format!("{p}%/{}", format_token_count(t)),
            (Some(p), None) => format!("{p}%"),
            (None, Some(t)) => format!("-%/{}", format_token_count(t)),
            (None, None) => "-%".to_owned(),
        },
        tau_proto::ProgressUnit::Tokens => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!("{}/{}", format_token_count(c), format_token_count(t)),
            (Some(c), None) => format_token_count(c),
            (None, Some(t)) => format!("-/{}", format_token_count(t)),
            (None, None) => "-".to_owned(),
        },
    };
    match counter.label.as_deref() {
        Some("ctx") => tool_suffix(format!("#{body}"), ToolStatus::Context),
        Some("tools") => tool_suffix(format!("%{body}"), ToolStatus::Tools),
        Some(label) => info_suffix(format!("{label}: {body}")),
        None => info_suffix(body),
    }
}

fn format_tool_use_range(range: &tau_proto::ToolUseRange) -> Option<String> {
    match (range.start.as_deref(), range.end.as_deref()) {
        (Some(start), Some(end)) => Some(format!("{start}..{end}")),
        (Some(start), None) => Some(format!("{start}..")),
        (None, Some(end)) => Some(format!("..{end}")),
        (None, None) => None,
    }
}

fn format_tool_use_state_stats(stats: &tau_proto::ToolUseStats) -> String {
    format_stats(stats.matches, stats.lines, stats.bytes)
}

fn format_stats(matches: Option<u64>, lines: Option<u64>, bytes: Option<u64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = matches {
        parts.push(m.to_string());
    }
    if let Some(l) = lines {
        parts.push(format!("{l}L"));
    }
    if let Some(b) = bytes {
        parts.push(format_tool_use_state_bytes(b));
    }
    parts.join(", ")
}

fn format_tool_use_state_bytes(bytes: u64) -> String {
    if bytes >= 1024 {
        let k = bytes as f64 / 1024.0;
        if k >= 100.0 {
            format!("{k:.0}kB")
        } else {
            format!("{k:.1}kB")
        }
    } else {
        format!("{bytes}B")
    }
}

/// Minimal display for events that didn't ship a [`ToolUseState`]
/// (old logs and any extension that hasn't migrated). Renders just
/// `<tool_name> ok` or `<tool_name> err: <short message>` — the chip
/// shape is intentionally generic so future tool names render without
/// touching this code.
pub(crate) fn synthesize_fallback_display(tool_name: &str, error: Option<&str>) -> ToolUseState {
    let (status, status_text) = match error {
        Some(msg) if !msg.is_empty() => (ToolUseStatus::Error, first_error_line(msg)),
        _ => (ToolUseStatus::Success, "ok".to_owned()),
    };
    let _ = tool_name;
    ToolUseState {
        args: String::new(),
        status,
        status_text,
        ..Default::default()
    }
}

fn first_error_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn error_status_text(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() || label == "err" {
        return "err".to_owned();
    }
    if label.starts_with("err:") {
        return label.to_owned();
    }
    format!("err: {label}")
}

pub(crate) fn build_tool_summary_display(summary: &ToolSummaryDisplay) -> ToolCallDisplay {
    let mut suffixes = Vec::new();
    if 0 < summary.added {
        suffixes.push(tool_suffix(
            format!("+{}", summary.added),
            ToolStatus::DiffAdded,
        ));
    }
    if 0 < summary.removed {
        suffixes.push(ToolSuffixSegment {
            text: format!("-{}", summary.removed),
            status: ToolStatus::DiffRemoved,
            no_leading_space: 0 < summary.added,
        });
    }
    let stats = format_stats(
        (0 < summary.matches).then_some(summary.matches),
        (0 < summary.lines).then_some(summary.lines),
        (0 < summary.bytes).then_some(summary.bytes),
    );
    if !stats.is_empty() {
        suffixes.push(info_suffix(stats));
    }
    if 0 < summary.ok {
        suffixes.push(tool_suffix(
            format!("ok: {}", summary.ok),
            ToolStatus::Success,
        ));
    }
    if 0 < summary.err {
        suffixes.push(tool_suffix(
            format!("err: {}", summary.err),
            ToolStatus::Error,
        ));
    }
    if summary.completed < summary.total {
        suffixes.push(tool_suffix(
            tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ToolStatus::Progress,
        ));
    }
    ToolCallDisplay {
        tool_name: "tools".to_owned(),
        mode: String::new(),
        args: format!("{}/{}", summary.completed, summary.total),
        range: None,
        suffixes,
        payload: None,
    }
}

/// Render a completed provider-side compaction item as a compact session
/// status line. Compaction is not a model-visible tool invocation, so this
/// paints the small lifecycle line directly instead of fabricating a
/// `ToolUseState`.
pub(crate) fn render_compaction_block(
    theme: &tau_themes::Theme,
    status_text: impl Into<String>,
    status: CompactionStatus,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let status_text = status_text.into();
    let mut themed = ThemedText::new();
    let output = themed.add_style(names::TOOL_OUTPUT);
    let name = themed.add_style(names::TOOL_NAME);
    let spacer = themed.add_style(names::TOOL_ARGS);
    let status_style = themed.add_style(match status {
        CompactionStatus::Success => names::TOOL_STATUS_SUCCESS,
        CompactionStatus::Progress => names::PROGRESS_INDICATOR,
    });
    let context_style = themed.add_style(names::STATUS_CONTEXT);
    let mut children = vec![
        SpanTree::span(name, vec![SpanTree::text("compact")]),
        SpanTree::span(spacer, vec![SpanTree::text(" ")]),
    ];
    for (index, part) in status_text.split(' ').enumerate() {
        if 0 < index {
            children.push(SpanTree::span(status_style, vec![SpanTree::text(" ")]));
        }
        let style = if part.starts_with('#') {
            context_style
        } else {
            status_style
        };
        children.push(SpanTree::span(style, vec![SpanTree::text(part.to_owned())]));
    }
    themed.push_tree(SpanTree::span(output, children));
    tau_cli_term::StyledBlock::new(themed_text(theme, &themed))
}

/// Paints a [`ToolCallDisplay`] onto a themed block.
pub(crate) fn render_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::StyledBlock;
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let mut themed = ThemedText::new();
    let output = themed.add_style(names::TOOL_OUTPUT);
    let name = themed.add_style(names::TOOL_NAME);
    let mode = themed.add_style(names::TOOL_MODE);
    let args = themed.add_style(names::TOOL_ARGS);

    let mut children = vec![SpanTree::span(
        name,
        vec![SpanTree::text(display.tool_name.clone())],
    )];
    if !display.mode.is_empty() {
        children.push(SpanTree::span(args, vec![SpanTree::text(" ")]));
        children.push(SpanTree::span(
            mode,
            vec![SpanTree::text(abbreviate_inline_text(&display.mode))],
        ));
    }
    if !display.args.is_empty() {
        children.push(SpanTree::span(
            args,
            vec![
                SpanTree::text(" "),
                SpanTree::text(abbreviate_inline_text(&display.args)),
            ],
        ));
    }
    if let Some(range) = &display.range {
        children.push(SpanTree::span(
            args,
            vec![
                SpanTree::text(" "),
                SpanTree::text(abbreviate_inline_text(range)),
            ],
        ));
    }
    for suffix in &display.suffixes {
        let status_name = match suffix.status {
            ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
            // Warning/Pending have no dedicated tokens yet — share the info
            // colour so the chip still reads as "non-error" without a
            // theme migration.
            ToolStatus::Warning | ToolStatus::Pending | ToolStatus::Info => names::TOOL_STATUS_INFO,
            ToolStatus::Error => names::TOOL_STATUS_ERROR,
            ToolStatus::Progress => names::PROGRESS_INDICATOR,
            ToolStatus::DiffAdded => names::DIFF_ADDED,
            ToolStatus::DiffRemoved => names::DIFF_REMOVED,
            ToolStatus::Role => names::STATUS_ROLE,
            ToolStatus::Context => names::STATUS_CONTEXT,
            ToolStatus::Tools => names::STATUS_TOOLS,
            ToolStatus::Time => names::TOOL_STATUS_TIME,
        };
        let status = themed.add_style(status_name);
        if !suffix.no_leading_space && !suffix.text.starts_with(':') {
            children.push(SpanTree::span(args, vec![SpanTree::text(" ")]));
        }
        children.push(SpanTree::span(
            status,
            vec![SpanTree::text(abbreviate_inline_text(&suffix.text))],
        ));
    }
    if let Some(ToolUsePayload::Text { text }) = &display.payload {
        children.push(SpanTree::span(args, vec![SpanTree::text("\n")]));
        children.push(SpanTree::span(args, vec![SpanTree::text(text.clone())]));
    }
    themed.push_tree(SpanTree::span(output, children));

    StyledBlock::new(themed_text(theme, &themed))
}

pub(crate) fn diff_payload_counts(payload: &ToolUsePayload) -> (u32, u32) {
    match payload {
        ToolUsePayload::Diff(summary) => (summary.added, summary.removed),
        ToolUsePayload::Diffs { files } => {
            files.iter().fold((0u32, 0u32), |(added, removed), file| {
                (
                    added.saturating_add(file.diff.added),
                    removed.saturating_add(file.diff.removed),
                )
            })
        }
        ToolUsePayload::Text { .. } => (0, 0),
    }
}

/// Like [`render_tool_block`] but appends an expanded unified-diff
/// body when `expanded` is true and `diff` has hunks. The first line
/// is the themed tool header (with `+N/-M` chip); the body, if
/// rendered, comes after a `\n` so `layout_lines` wraps each diff line
/// independently.
pub(crate) fn render_diff_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    diff: &tau_proto::DiffSummary,
    expanded: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    // Reuse the header from render_tool_block, then keep its spans so
    // we can append diff lines below it.
    let header = render_tool_block(theme, display);
    let mut spans: Vec<Span> = header.content.spans().to_vec();

    if !expanded || diff.hunks.is_empty() {
        return StyledBlock::new(StyledText::from(spans));
    }

    let added_style = resolve(theme, names::DIFF_ADDED);
    let removed_style = resolve(theme, names::DIFF_REMOVED);
    let context_style = resolve(theme, names::DIFF_CONTEXT);
    let header_style = resolve(theme, names::DIFF_HUNK_HEADER);
    let added_inline_style = overlay_style(added_style, resolve(theme, names::DIFF_ADDED_INLINE));
    let removed_inline_style =
        overlay_style(removed_style, resolve(theme, names::DIFF_REMOVED_INLINE));

    for hunk in &diff.hunks {
        spans.push(Span::new("\n", context_style));
        spans.push(Span::new(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
            header_style,
        ));
        for line in &hunk.lines {
            spans.push(Span::new("\n", context_style));
            match line {
                tau_proto::DiffLine::Equal { text } => {
                    spans.push(Span::new(format!(" {text}"), context_style));
                }
                tau_proto::DiffLine::Add { text } => {
                    spans.push(Span::new(format!("+{text}"), added_style));
                }
                tau_proto::DiffLine::Remove { text } => {
                    spans.push(Span::new(format!("-{text}"), removed_style));
                }
                tau_proto::DiffLine::Modify { old, new } => {
                    spans.push(Span::new("-".to_owned(), removed_style));
                    push_segments(&mut spans, old, removed_style, removed_inline_style);
                    spans.push(Span::new("\n".to_owned(), context_style));
                    spans.push(Span::new("+".to_owned(), added_style));
                    push_segments(&mut spans, new, added_style, added_inline_style);
                }
            }
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

/// Like [`render_diff_tool_block`] but keeps file boundaries for multi-file
/// mutation payloads by rendering each display path before its hunks.
pub(crate) fn render_multi_diff_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    files: &[tau_proto::FileDiffSummary],
    expanded: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let header = render_tool_block(theme, display);
    let mut spans: Vec<Span> = header.content.spans().to_vec();

    if !expanded || files.iter().all(|file| file.diff.hunks.is_empty()) {
        return StyledBlock::new(StyledText::from(spans));
    }

    let added_style = resolve(theme, names::DIFF_ADDED);
    let removed_style = resolve(theme, names::DIFF_REMOVED);
    let context_style = resolve(theme, names::DIFF_CONTEXT);
    let header_style = resolve(theme, names::DIFF_HUNK_HEADER);
    let added_inline_style = overlay_style(added_style, resolve(theme, names::DIFF_ADDED_INLINE));
    let removed_inline_style =
        overlay_style(removed_style, resolve(theme, names::DIFF_REMOVED_INLINE));

    for file in files {
        if file.diff.hunks.is_empty() {
            continue;
        }
        spans.push(Span::new("\n", context_style));
        spans.push(Span::new(format!("--- {}", file.path), header_style));
        for hunk in &file.diff.hunks {
            spans.push(Span::new("\n", context_style));
            spans.push(Span::new(
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
                ),
                header_style,
            ));
            for line in &hunk.lines {
                spans.push(Span::new("\n", context_style));
                match line {
                    tau_proto::DiffLine::Equal { text } => {
                        spans.push(Span::new(format!(" {text}"), context_style));
                    }
                    tau_proto::DiffLine::Add { text } => {
                        spans.push(Span::new(format!("+{text}"), added_style));
                    }
                    tau_proto::DiffLine::Remove { text } => {
                        spans.push(Span::new(format!("-{text}"), removed_style));
                    }
                    tau_proto::DiffLine::Modify { old, new } => {
                        spans.push(Span::new("-".to_owned(), removed_style));
                        push_segments(&mut spans, old, removed_style, removed_inline_style);
                        spans.push(Span::new("\n".to_owned(), context_style));
                        spans.push(Span::new("+".to_owned(), added_style));
                        push_segments(&mut spans, new, added_style, added_inline_style);
                    }
                }
            }
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

fn push_segments(
    spans: &mut Vec<tau_cli_term::Span>,
    segments: &[tau_proto::DiffSegment],
    base: tau_cli_term::Style,
    inline: tau_cli_term::Style,
) {
    use tau_cli_term::Span;
    for seg in segments {
        match seg {
            tau_proto::DiffSegment::Equal { text } => {
                spans.push(Span::new(text.clone(), base));
            }
            // Within a Modify line, only the *changed* sub-slice on
            // each side is meaningful. Hide the *other* side's slice
            // so we don't double up (e.g. the - line shouldn't show
            // the new tokens, only the old).
            tau_proto::DiffSegment::Remove { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
            tau_proto::DiffSegment::Add { text } => {
                spans.push(Span::new(text.clone(), inline));
            }
        }
    }
}

fn overlay_style(base: tau_cli_term::Style, overlay: tau_cli_term::Style) -> tau_cli_term::Style {
    tau_cli_term::Style {
        fg: overlay.fg.or(base.fg),
        bg: overlay.bg.or(base.bg),
        bold: base.bold || overlay.bold,
        underline: base.underline || overlay.underline,
        italic: base.italic || overlay.italic,
        strikethrough: base.strikethrough || overlay.strikethrough,
    }
}

/// Render a user `!`/`!!` shell block: a `shell <cmd>` header in the
/// same three-span theme used for tool calls, with streaming output
/// below in the default style.
///
/// `status_suffix`:
///   - `Some("running")` while the command is in-flight (info style),
///   - `Some("[0]")` / `Some("[N]")` on completion (success / error style,
///     keyed off exit code),
///   - `Some("cancelled")` on cancel (info style).
pub(crate) fn render_shell_block(
    theme: &tau_themes::Theme,
    command: &str,
    output: &str,
    status_suffix: Option<&str>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let name_style = resolve(theme, names::TOOL_NAME);
    let args_style = resolve(theme, names::TOOL_ARGS);
    let status_name = match status_suffix {
        Some(s) if s.starts_with("[0]") => names::TOOL_STATUS_SUCCESS,
        Some(s) if s.starts_with('[') => names::TOOL_STATUS_ERROR,
        _ => names::TOOL_STATUS_INFO,
    };
    let status_style = resolve(theme, status_name);

    let mut spans = vec![
        Span::new("shell", name_style),
        Span::new(" ", args_style),
        Span::new(abbreviate_inline_text(command), args_style),
    ];
    if let Some(suffix) = status_suffix {
        spans.push(Span::new(" ", args_style));
        spans.push(Span::new(abbreviate_inline_text(suffix), status_style));
    }
    if !output.is_empty() {
        spans.push(Span::new("\n", args_style));
        spans.push(Span::new(output.to_owned(), args_style));
    }
    StyledBlock::new(StyledText::from(spans))
}

pub(crate) fn render_action_output_block(
    theme: &tau_themes::Theme,
    text: &str,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    let styles = ActionStyles {
        output: resolve(theme, names::ACTION_OUTPUT),
        label: resolve(theme, names::ACTION_LABEL),
        value: resolve(theme, names::ACTION_VALUE),
        id: resolve(theme, names::ACTION_ID),
    };
    let mut spans = Vec::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        push_action_line(&mut spans, body, styles);
        if line.ends_with('\n') {
            spans.push(Span::new("\n", styles.output));
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

pub(crate) fn render_action_error_block(
    theme: &tau_themes::Theme,
    action_id: &str,
    message: &str,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledBlock, StyledText};
    use tau_themes::names;

    StyledBlock::new(StyledText::from(vec![
        Span::new(action_id.to_owned(), resolve(theme, names::ACTION_ID)),
        Span::new(": ", resolve(theme, names::ACTION_OUTPUT)),
        Span::new(message.to_owned(), resolve(theme, names::ACTION_ERROR)),
    ]))
}

#[derive(Clone, Copy)]
struct ActionStyles {
    output: tau_cli_term::Style,
    label: tau_cli_term::Style,
    value: tau_cli_term::Style,
    id: tau_cli_term::Style,
}

fn push_action_line(spans: &mut Vec<tau_cli_term::Span>, line: &str, styles: ActionStyles) {
    if push_action_approval_heading(spans, line, styles) {
        return;
    }
    if push_action_label_line(spans, line, styles) {
        return;
    }

    let mut index = 0;
    if let Some(end) = leading_action_id_end(line) {
        spans.push(tau_cli_term::Span::new(line[..end].to_owned(), styles.id));
        index = end;
    }
    push_action_tokens(spans, &line[index..], styles);
}

fn push_action_approval_heading(
    spans: &mut Vec<tau_cli_term::Span>,
    line: &str,
    styles: ActionStyles,
) -> bool {
    let Some((prefix, id)) = line.rsplit_once(' ') else {
        return false;
    };
    if !prefix.to_ascii_lowercase().contains("approval") || !is_action_id_token(id) {
        return false;
    }
    spans.push(tau_cli_term::Span::new(format!("{prefix} "), styles.output));
    spans.push(tau_cli_term::Span::new(id.to_owned(), styles.id));
    true
}

fn push_action_label_line(
    spans: &mut Vec<tau_cli_term::Span>,
    line: &str,
    styles: ActionStyles,
) -> bool {
    let Some(colon) = line.find(':') else {
        return false;
    };
    if line[..colon].contains(char::is_whitespace) {
        return false;
    }
    let label = &line[..=colon];
    let mut value = &line[colon + 1..];
    spans.push(tau_cli_term::Span::new(label.to_owned(), styles.label));
    if let Some(stripped) = value.strip_prefix(' ') {
        spans.push(tau_cli_term::Span::new(" ", styles.output));
        value = stripped;
    }
    let value_style = if is_action_id_label(&line[..colon]) && is_action_id_token(value) {
        styles.id
    } else {
        styles.value
    };
    spans.push(tau_cli_term::Span::new(value.to_owned(), value_style));
    true
}

fn push_action_tokens(spans: &mut Vec<tau_cli_term::Span>, text: &str, styles: ActionStyles) {
    let mut rest = text;
    while !rest.is_empty() {
        let split_at = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if 0 < split_at {
            spans.push(tau_cli_term::Span::new(
                rest[..split_at].to_owned(),
                styles.output,
            ));
            rest = &rest[split_at..];
            continue;
        }
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        push_action_token(spans, token, styles);
        rest = &rest[token_end..];
    }
}

fn push_action_token(spans: &mut Vec<tau_cli_term::Span>, token: &str, styles: ActionStyles) {
    let Some(eq) = token.find('=') else {
        spans.push(tau_cli_term::Span::new(token.to_owned(), styles.output));
        return;
    };
    if eq == 0 {
        spans.push(tau_cli_term::Span::new(token.to_owned(), styles.output));
        return;
    }
    spans.push(tau_cli_term::Span::new(
        token[..=eq].to_owned(),
        styles.label,
    ));
    spans.push(tau_cli_term::Span::new(
        token[eq + 1..].to_owned(),
        styles.value,
    ));
}

fn leading_action_id_end(line: &str) -> Option<usize> {
    let end = line.find(char::is_whitespace)?;
    let token = &line[..end];
    let rest = &line[end..];
    (is_action_id_token(token) && rest.contains('=')).then_some(end)
}

fn is_action_id_label(label: &str) -> bool {
    label == "id" || label.ends_with("_id") || label.ends_with("-id")
}

fn is_action_id_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 16
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn render_harness_notice(
    theme: &tau_themes::Theme,
    info: &tau_proto::HarnessNotice,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_block;
    use tau_themes::names;

    let style_name = match info.level {
        tau_proto::NoticeLevel::Critical | tau_proto::NoticeLevel::Warning => {
            names::SYSTEM_INFO_IMPORTANT
        }
        tau_proto::NoticeLevel::Info
        | tau_proto::NoticeLevel::Debug
        | tau_proto::NoticeLevel::Trace => names::SYSTEM_INFO,
    };
    themed_block(theme, style_name, &info.message)
}

pub(crate) fn ui_dir_block(theme: &tau_themes::Theme, path: &Path) -> tau_cli_term::StyledBlock {
    system_path_block(theme, "ui dir: ", path, "/")
}

pub(crate) fn session_status_block(
    theme: &tau_themes::Theme,
    path: &Path,
    suffix: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::SESSION_STATUS);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(lifecycle, "session dir: ");
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn system_path_block(
    theme: &tau_themes::Theme,
    prefix: &str,
    path: &Path,
    suffix: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    text.push(info, prefix);
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn system_loaded_block(
    theme: &tau_themes::Theme,
    path: &Path,
    content: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    let stats_style = text.add_style(names::TOOL_STATUS_INFO);
    text.push(info, "loaded: ");
    text.push(path_style, display_path(path));
    text.push(info, " ");
    text.push(stats_style, output_stats_suffix(content).text);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn agent_context_ready_block(
    theme: &tau_themes::Theme,
    agent_id: &tau_proto::AgentId,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let agent_style = text.add_style(names::STATUS_ROLE);
    let status_style = text.add_style(names::SYSTEM_STATUS);
    text.push(info, "agent ");
    text.push(agent_style, format!("@{agent_id}"));
    text.push(info, " context ");
    text.push(status_style, "ready");
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

pub(crate) fn extension_status_block(
    theme: &tau_themes::Theme,
    extension_name: &str,
    status: &str,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let lifecycle = text.add_style(names::EXTENSION_LIFECYCLE);
    let status_style = text.add_style(names::EXTENSION_STATUS);
    text.push(lifecycle, "extension ");
    text.push(lifecycle, extension_name);
    text.push(lifecycle, " ");
    text.push(status_style, status);
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

fn display_path(path: &Path) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.display().to_string();
    };
    let home = Path::new(&home);
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    let Ok(suffix) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if suffix.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", suffix.display())
    }
}
