//! Theming and block rendering for tool calls and other transcript
//! elements. Pure functions over [`tau_proto`] payloads — no
//! [`tau_cli_term`] state lives here.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStatus};

use crate::turn_stats_projection::{
    CumulativeTurnUsageProjection, PreviousTurnUsageProjection, TurnStatsPresentationProjection,
    TurnStatsUsageProjection,
};

#[cfg(test)]
pub(crate) fn format_turn_stats_line(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> String {
    turn_stats_parts_from_provider(
        usage,
        &usage.stats.total,
        previous_usage,
        turn_latency,
        total_latency,
    )
    .into_iter()
    .map(|part| part.text)
    .collect()
}

#[cfg(test)]
pub(crate) fn render_turn_stats_block(
    theme: &tau_themes::Theme,
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> tau_cli_term::StyledBlock {
    render_provider_turn_stats_block_with_cumulative_usage(
        theme,
        usage,
        &usage.stats.total,
        previous_usage,
        turn_latency,
        total_latency,
    )
}

#[cfg(test)]
pub(crate) fn render_provider_turn_stats_block_with_cumulative_usage(
    theme: &tau_themes::Theme,
    usage: &tau_proto::ProviderTokenUsage,
    cumulative_usage: &tau_proto::TokenUsageCounts,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> tau_cli_term::StyledBlock {
    render_turn_stats_parts(
        theme,
        turn_stats_parts_from_provider(
            usage,
            cumulative_usage,
            previous_usage,
            turn_latency,
            total_latency,
        ),
    )
}

/// Renders one retained turn-stat presentation projection.
///
/// The projection contains only the scalar current, preceding, and cumulative
/// values that can affect the block's terminal cells and styles.
pub(crate) fn render_turn_stats_projection_block(
    theme: &tau_themes::Theme,
    projection: &TurnStatsPresentationProjection,
) -> tau_cli_term::StyledBlock {
    render_turn_stats_parts(
        theme,
        turn_stats_parts(
            projection.usage,
            projection.cumulative_usage,
            projection.previous_usage,
            projection.turn_latency,
            projection.total_latency,
        ),
    )
}

fn render_turn_stats_parts(
    theme: &tau_themes::Theme,
    parts: Vec<TurnStatsPart>,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_text;
    use tau_themes::{SpanTree, ThemedText, names};

    let mut themed = ThemedText::new();
    let root = themed.add_style(names::TOKEN_STATS);
    let mut children = Vec::new();
    for part in parts {
        let style = themed.add_style(part.style_name);
        children.push(SpanTree::span(style, vec![SpanTree::text(part.text)]));
    }
    themed.push_tree(SpanTree::span(root, children));
    tau_cli_term::StyledBlock::new(themed_text(theme, &themed))
}

const CACHE_HIT_WARNING_PERCENT: u8 = 90;

enum CacheEfficiency {
    Exact {
        cached: u64,
        ceiling: u64,
        percent: u8,
    },
    Estimated {
        cached: u64,
        ceiling: u64,
        percent: u8,
    },
    NoOpportunity,
    EstimatedNoOpportunity,
    Invalid {
        cached: u64,
    },
}

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
    usage: TurnStatsUsageProjection,
    cumulative_usage: CumulativeTurnUsageProjection,
    previous_usage: Option<PreviousTurnUsageProjection>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> Vec<TurnStatsPart> {
    use tau_themes::names;

    let turn_cache_possible = reusable_prompt_prefix_tokens(usage, previous_usage);
    let new_prompt_tokens = usage.prompt_sent_tokens.saturating_sub(turn_cache_possible);
    let mut parts = Vec::new();

    let efficiency = cache_efficiency(usage, turn_cache_possible);
    let (marker, detail, style) = match efficiency {
        CacheEfficiency::Exact {
            cached,
            ceiling,
            percent,
        } => (
            format!("Δ{percent}%"),
            format!(
                " {}/{}",
                format_token_count(cached),
                format_token_count(ceiling)
            ),
            cache_hit_style_name(percent),
        ),
        CacheEfficiency::Estimated {
            cached,
            ceiling,
            percent,
        } => (
            format!("Δ{percent}%?"),
            format!(
                " {}/{}?",
                format_token_count(cached),
                format_token_count(ceiling)
            ),
            cache_hit_style_name(percent),
        ),
        CacheEfficiency::NoOpportunity => (
            "Δ—".to_owned(),
            " 0/0".to_owned(),
            names::TOKEN_STATS_CACHE_HIT,
        ),
        CacheEfficiency::EstimatedNoOpportunity => (
            "Δ—?".to_owned(),
            " 0/0?".to_owned(),
            names::TOKEN_STATS_CACHE_HIT,
        ),
        CacheEfficiency::Invalid { cached } => (
            "Δ!".to_owned(),
            format!(" {}/?", format_token_count(cached)),
            names::TOKEN_STATS_CACHE_MISS,
        ),
    };
    parts.push(TurnStatsPart::new(marker, names::TOKEN_STATS_DELTA));
    parts.push(TurnStatsPart::new(detail, style));
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
            format_token_count(cumulative_usage.cached_tokens),
            format_token_count(cumulative_usage.sent_tokens),
        ),
        names::TOKEN_STATS_INPUT,
    ));
    parts.push(TurnStatsPart::new(" ↓", names::TOKEN_STATS_DOWN));
    parts.push(TurnStatsPart::new(
        format_token_count(cumulative_usage.received_tokens),
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

#[cfg(test)]
fn turn_stats_parts_from_provider(
    usage: &tau_proto::ProviderTokenUsage,
    cumulative_usage: &tau_proto::TokenUsageCounts,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> Vec<TurnStatsPart> {
    turn_stats_parts(
        TurnStatsUsageProjection::from(usage),
        CumulativeTurnUsageProjection::from(*cumulative_usage),
        previous_usage
            .map(TurnStatsUsageProjection::from)
            .map(Into::into),
        turn_latency,
        total_latency,
    )
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

fn cache_hit_style_name(percent: u8) -> &'static str {
    use tau_themes::names;

    if percent == 100 {
        names::TOKEN_STATS_CACHE_HIT
    } else if CACHE_HIT_WARNING_PERCENT < percent {
        names::TOKEN_STATS_CACHE_WARN
    } else {
        names::TOKEN_STATS_CACHE_MISS
    }
}

/// Returns the current prompt prefix that can be reused from the preceding
/// turn.
fn reusable_prompt_prefix_tokens(
    usage: TurnStatsUsageProjection,
    previous_usage: Option<PreviousTurnUsageProjection>,
) -> u64 {
    previous_usage
        .map_or(0, |usage| {
            usage
                .prompt_sent_tokens
                .saturating_add(usage.response_received_tokens)
        })
        .min(usage.prompt_sent_tokens)
}

fn cache_efficiency(usage: TurnStatsUsageProjection, estimated_ceiling: u64) -> CacheEfficiency {
    let cached = usage.prompt_cached_tokens;
    let sent = usage.prompt_sent_tokens;
    if sent < cached {
        return CacheEfficiency::Invalid { cached };
    }
    let Some(ceiling) = usage.prompt_cache_read_ceiling_tokens else {
        if estimated_ceiling < cached {
            return CacheEfficiency::Invalid { cached };
        }
        if estimated_ceiling == 0 {
            return CacheEfficiency::EstimatedNoOpportunity;
        }
        let percent = (u128::from(cached) * 100 / u128::from(estimated_ceiling)) as u8;
        return CacheEfficiency::Estimated {
            cached,
            ceiling: estimated_ceiling,
            percent,
        };
    };
    if sent < ceiling || ceiling < cached {
        return CacheEfficiency::Invalid { cached };
    }
    if ceiling == 0 {
        return CacheEfficiency::NoOpportunity;
    }
    let percent = (u128::from(cached) * 100 / u128::from(ceiling)) as u8;
    CacheEfficiency::Exact {
        cached,
        ceiling,
        percent,
    }
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

/// Formats a context-size magnitude in at most three numeric columns.
///
/// Values below one thousand remain exact. Larger values round half up: values
/// below ten of a unit retain one decimal, values through 999 use whole units,
/// and a rounded thousand promotes to the next unit.
pub(crate) fn format_context_token_count(tokens: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000, "k"),
        (1_000_000, "m"),
        (1_000_000_000, "b"),
        (1_000_000_000_000, "t"),
        (1_000_000_000_000_000, "q"),
        (1_000_000_000_000_000_000, "e"),
    ];

    if tokens < 1_000 {
        return tokens.to_string();
    }

    let first_unit = UNITS
        .iter()
        .rposition(|(unit, _)| *unit <= tokens)
        .expect("values below one thousand returned above");
    for (index, &(unit, suffix)) in UNITS.iter().enumerate().skip(first_unit) {
        let tenths = rounded_units(tokens, unit / 10);
        if tenths < 100 {
            return format!("{}.{}{}", tenths / 10, tenths % 10, suffix);
        }

        let whole = rounded_units(tokens, unit);
        if whole < 1_000 || index + 1 == UNITS.len() {
            return format!("{whole}{suffix}");
        }
    }

    unreachable!("the exa unit covers every u64 token count")
}

fn rounded_units(value: u64, unit: u64) -> u64 {
    u64::try_from((u128::from(value) + u128::from(unit / 2)) / u128::from(unit)).unwrap_or(u64::MAX)
}

/// Semantic style for one compact tool-line segment.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ToolStatus {
    Success,
    Warning,
    Error,
    Pending,
    Info,
    WorkTitle,
    Agent,
    AgentContext,
    Counter,
    Progress,
    DiffAdded,
    DiffRemoved,
    Context,
    Tools,
    Time,
}

/// Status variant for completed compaction lines. Kept separate from
/// tool-call display state because compaction is not a model-visible tool
/// invocation.
#[derive(Clone, Copy)]
pub(crate) enum CompactionStatus {
    /// The compaction stopped without an accepted replacement window.
    Failure,
    /// The compaction completed with an accepted replacement window.
    Success,
    /// The compaction provider work is still in progress.
    Progress,
}

/// One generic compact header segment, renderable before or after tool fields.
#[derive(Clone, PartialEq)]
pub(crate) struct ToolLineSegment {
    pub(crate) text: String,
    pub(crate) status: ToolStatus,
    /// When true, suppress the implicit space the renderer normally
    /// inserts before this segment. Used to glue parts of a multi-span
    /// chip (e.g. the colored `+N/-M` diff stat) into one continuous
    /// run.
    pub(crate) no_leading_space: bool,
}

/// Decomposed compact tool-block label, painted as themed spans:
/// `<status_prefix> <tool_name> <leading...> <mode> <args> <range>
/// <suffix...>`.
#[derive(Clone, PartialEq)]
pub(crate) struct ToolCallDisplay {
    /// Optional atomic semantic status rendered before the stable identity.
    pub(crate) status_prefix: Option<(String, ToolStatus)>,
    pub(crate) tool_name: String,
    pub(crate) tool_name_style: Option<&'static str>,
    /// Generic compact segments rendered directly after the stable identity.
    pub(crate) leading_segments: Vec<ToolLineSegment>,
    pub(crate) mode: String,
    pub(crate) args: String,
    pub(crate) range: Option<String>,
    pub(crate) suffixes: Vec<ToolLineSegment>,
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

/// Build the completion descriptor for a finished `agent_start` call.
///
/// Current first-party rendering uses generic watched-agent status rows for
/// live child activity, so this helper only shapes the final metadata/error
/// line for the already-completed spawn tool.
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

fn tool_suffix(text: String, status: ToolStatus) -> ToolLineSegment {
    ToolLineSegment {
        text,
        status,
        no_leading_space: false,
    }
}

pub(crate) fn pending_tool_call_display(tool_name: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        status_prefix: None,
        tool_name: tool_name.to_owned(),
        tool_name_style: None,
        leading_segments: Vec::new(),
        mode: String::new(),
        args: String::new(),
        range: None,
        suffixes: vec![tool_suffix("pending".to_owned(), ToolStatus::Pending)],
        payload: None,
    }
}
fn info_suffix(text: String) -> ToolLineSegment {
    tool_suffix(text, ToolStatus::Info)
}

fn counter_suffix(text: String) -> ToolLineSegment {
    tool_suffix(text, ToolStatus::Counter)
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

pub(crate) fn tool_duration_suffix(duration: Duration) -> ToolLineSegment {
    tool_suffix(format_tool_duration(duration), ToolStatus::Time)
}

pub(crate) fn format_tool_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

fn abbreviate_inline_text(text: &str) -> String {
    const EDGE_CHARS: usize = 20;

    let one_line = normalize_inline_text(text);
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

/// Replaces line boundaries with spaces using the historical compact-field
/// normalization shared by tool headers and shell summaries.
fn normalize_inline_text(text: &str) -> String {
    text.lines().collect::<Vec<_>>().join(" ")
}

/// Render a [`ToolUseState`] descriptor directly to a
/// [`ToolCallDisplay`]. The generic path the renderer takes when the
/// tool side attached a display descriptor to its result/error event —
/// no `match tool_name` arms needed. Falls back to
/// [`format_tool_completion`] for older events that didn't carry a
/// descriptor.
pub(crate) fn render_tool_use_state(tool_name: &str, display: &ToolUseState) -> ToolCallDisplay {
    render_tool_use_state_inner(
        tool_name,
        display,
        true,
        ToolPayloadProjection::RetainDescriptor,
    )
}

/// Renders a tool descriptor while deriving diff counters from a separately
/// owned payload and omitting that payload from the returned header.
///
/// Completed diff history owns the payload separately so settings changes can
/// re-render its body without retaining a second copy in [`ToolCallDisplay`].
pub(crate) fn render_tool_use_state_payload_free(
    tool_name: &str,
    display: &ToolUseState,
    payload: &ToolUsePayload,
) -> ToolCallDisplay {
    render_tool_use_state_inner(
        tool_name,
        display,
        true,
        ToolPayloadProjection::BorrowCounters(payload),
    )
}

/// Renders shared tool-like fields without fabricating a lifecycle status.
///
/// This is reserved for presentation-only rows, such as watched-agent
/// activity, that reuse tool counters but are not tool invocations.
pub(crate) fn render_tool_use_state_without_status(
    tool_name: &str,
    display: &ToolUseState,
) -> ToolCallDisplay {
    render_tool_use_state_inner(
        tool_name,
        display,
        false,
        ToolPayloadProjection::RetainDescriptor,
    )
}

/// Selects whether a rendered tool display owns its descriptor payload or only
/// borrows a separately retained payload for header counters.
#[derive(Clone, Copy)]
enum ToolPayloadProjection<'a> {
    /// Clone and retain the descriptor payload in the rendered display.
    RetainDescriptor,
    /// Omit the payload while deriving counters from this history-owned value.
    BorrowCounters(&'a ToolUsePayload),
}

fn render_tool_use_state_inner(
    tool_name: &str,
    display: &ToolUseState,
    include_status: bool,
    payload_projection: ToolPayloadProjection<'_>,
) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolLineSegment> = Vec::new();
    let counter_payload = match payload_projection {
        ToolPayloadProjection::RetainDescriptor => display.payload.as_ref(),
        ToolPayloadProjection::BorrowCounters(payload) => Some(payload),
    };
    let (added, removed) = counter_payload.map(diff_payload_counts).unwrap_or_default();
    if 0 < added {
        suffixes.push(tool_suffix(format!("+{added}"), ToolStatus::DiffAdded));
    }
    if 0 < removed {
        suffixes.push(ToolLineSegment {
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
        suffixes.push(tool_suffix(
            chip.clone(),
            if is_agent_id_chip(chip) {
                ToolStatus::Agent
            } else {
                ToolStatus::Info
            },
        ));
    }
    if include_status {
        let status_kind = match display.status {
            ToolUseStatus::Success => ToolStatus::Success,
            ToolUseStatus::Warning => ToolStatus::Warning,
            ToolUseStatus::Error => ToolStatus::Error,
            ToolUseStatus::InProgress => ToolStatus::Progress,
        };
        let supplied_status = display.status_text.trim();
        let supplied_status_width =
            tau_cli_term::StyledText::from(supplied_status.to_owned()).char_count();
        let mut status_text = if supplied_status_width == 0 {
            match display.status {
                ToolUseStatus::Success => "ok".to_owned(),
                ToolUseStatus::Warning => "warn".to_owned(),
                ToolUseStatus::Error => "err".to_owned(),
                ToolUseStatus::InProgress => tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            }
        } else {
            supplied_status.to_owned()
        };
        if matches!(display.status, ToolUseStatus::Error) {
            status_text = error_status_text(&status_text);
        }
        suffixes.push(tool_suffix(status_text, status_kind));
    }
    ToolCallDisplay {
        status_prefix: None,
        tool_name: tool_name.to_owned(),
        tool_name_style: None,
        leading_segments: Vec::new(),
        mode: display.mode.clone(),
        args: display.args.clone(),
        range: display.range.as_ref().and_then(format_tool_use_range),
        suffixes,
        payload: match payload_projection {
            ToolPayloadProjection::RetainDescriptor => display.payload.clone(),
            ToolPayloadProjection::BorrowCounters(_) => None,
        },
    }
}

/// Recognizes a complete, syntactically valid `@agent-id` info chip.
fn is_agent_id_chip(chip: &str) -> bool {
    chip.strip_prefix('@')
        .is_some_and(|agent_id| tau_proto::AgentId::parse(agent_id).is_ok())
}

fn format_progress_counter(counter: &tau_proto::ProgressCounter) -> ToolLineSegment {
    let format_context_or_token_count = |tokens| {
        if counter.label.as_deref() == Some("ctx") {
            format_context_token_count(tokens)
        } else {
            format_token_count(tokens)
        }
    };
    let body = match counter.unit {
        tau_proto::ProgressUnit::Count => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!("{c}/{t}"),
            (Some(c), None) => c.to_string(),
            (None, Some(t)) => format!("-/{t}"),
            (None, None) => "-".to_owned(),
        },
        tau_proto::ProgressUnit::Percent => match (counter.complete, counter.total) {
            (Some(p), Some(t)) => format!("{p}%/{}", format_context_or_token_count(t)),
            (Some(p), None) => format!("{p}%"),
            (None, Some(t)) => format!("-%/{}", format_context_or_token_count(t)),
            (None, None) => "-%".to_owned(),
        },
        tau_proto::ProgressUnit::Tokens => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!(
                "{}/{}",
                format_context_or_token_count(c),
                format_context_or_token_count(t)
            ),
            (Some(c), None) => format_context_or_token_count(c),
            (None, Some(t)) => format!("-/{}", format_context_or_token_count(t)),
            (None, None) => "-".to_owned(),
        },
    };
    match counter.label.as_deref() {
        Some("ctx") => tool_suffix(format!("#{body}"), ToolStatus::Context),
        Some("tools") => tool_suffix(format!("%{body}"), ToolStatus::Tools),
        Some(label) => counter_suffix(format!("{label}: {body}")),
        None => counter_suffix(body),
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
    if 1024 <= bytes {
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
        suffixes.push(ToolLineSegment {
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
        status_prefix: None,
        tool_name: "tools".to_owned(),
        tool_name_style: None,
        leading_segments: Vec::new(),
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
        CompactionStatus::Failure => names::TOOL_STATUS_ERROR,
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

/// Priority bands for one-line tool-call presentation elements.
///
/// The externally requested bands are identity `0`, result status `10`, error
/// details `20`, arguments `30`, and agent ids `40`. The remaining existing
/// fields use mode `50`, range `60`, diff/progress counters `70`, generic info
/// `80`, and duration `90`. Self-reported task titles use `75`, so a watched
/// row drops its display name before its title and both before telemetry.
#[derive(Clone, Copy)]
pub(crate) enum ToolLineElement {
    /// Tool name or watched-row identity.
    Identity,
    /// Exact success, failure, warning, pending, or progress status.
    ResultStatus,
    /// Human-readable text following an exact `err` status.
    ErrorDetails,
    /// Primary tool arguments.
    Arguments,
    /// Stable `@agent-id` information.
    AgentId,
    /// Atomic relationship context for a watched-agent identity.
    AgentContext,
    /// Tool execution or access mode.
    Mode,
    /// Structured range associated with the arguments.
    Range,
    /// Self-reported task title following a stable activity state.
    WorkTitle,
    /// Diff statistics or structured progress counters.
    Counter,
    /// Other informational suffix chips.
    Info,
    /// Elapsed tool duration.
    Duration,
}

impl ToolLineElement {
    /// Returns the documented priority where zero is most important.
    pub(crate) const fn priority(self) -> tau_cli_term::PriorityLinePriority {
        let value = match self {
            Self::Identity => 0,
            Self::ResultStatus => 10,
            Self::ErrorDetails => 20,
            Self::Arguments => 30,
            Self::AgentId | Self::AgentContext => 40,
            Self::Mode => 50,
            Self::Range => 60,
            Self::Counter => 70,
            Self::WorkTitle => 75,
            Self::Info => 80,
            Self::Duration => 90,
        };
        tau_cli_term::PriorityLinePriority::new(value)
    }
}

/// Builds one styled fragment with the historical tool-output parent style.
fn tool_line_text(
    theme: &tau_themes::Theme,
    style_name: &str,
    text: impl Into<String>,
) -> tau_cli_term::StyledText {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledText};
    use tau_themes::names;

    let style = overlay_style(
        resolve(theme, names::TOOL_OUTPUT),
        resolve(theme, style_name),
    );
    StyledText::from(Span::new(normalize_inline_text(&text.into()), style))
}

/// Renders recursive-watch attribution with a semantic watched-agent identity.
///
/// The relationship marker retains ordinary agent-context styling while the
/// attributed identity uses the same style as the watched row's primary
/// identity.
fn watched_agent_context_text(
    theme: &tau_themes::Theme,
    context: &str,
) -> tau_cli_term::StyledText {
    use tau_themes::names;

    let Some((label, agent_id)) = context.split_once(" @") else {
        return tool_line_text(theme, tool_status_style(ToolStatus::AgentContext), context);
    };
    let mut rendered = tool_line_text(
        theme,
        tool_status_style(ToolStatus::AgentContext),
        format!("{label} "),
    );
    let agent_id = tool_line_text(theme, names::WATCHING_NAME, format!("@{agent_id}"));
    for span in agent_id.spans() {
        rendered.push(span.clone());
    }
    rendered
}

/// Returns the theme token used for one suffix category.
fn tool_status_style(status: ToolStatus) -> &'static str {
    use tau_themes::names;

    match status {
        ToolStatus::Success => names::TOOL_STATUS_SUCCESS,
        // Warning/Pending have no dedicated tokens yet — share the info
        // colour so the chip still reads as "non-error" without a theme
        // migration.
        ToolStatus::Warning
        | ToolStatus::Pending
        | ToolStatus::Info
        | ToolStatus::WorkTitle
        | ToolStatus::Agent
        | ToolStatus::AgentContext
        | ToolStatus::Counter => names::TOOL_STATUS_INFO,
        ToolStatus::Error => names::TOOL_STATUS_ERROR,
        ToolStatus::Progress => names::PROGRESS_INDICATOR,
        ToolStatus::DiffAdded => names::DIFF_ADDED,
        ToolStatus::DiffRemoved => names::DIFF_REMOVED,
        ToolStatus::Context => names::STATUS_CONTEXT,
        ToolStatus::Tools => names::STATUS_TOOLS,
        ToolStatus::Time => names::TOOL_STATUS_TIME,
    }
}

/// Returns the priority category for a non-error suffix.
fn tool_suffix_element(status: ToolStatus) -> ToolLineElement {
    match status {
        ToolStatus::Success | ToolStatus::Warning | ToolStatus::Pending | ToolStatus::Progress => {
            ToolLineElement::ResultStatus
        }
        ToolStatus::Agent => ToolLineElement::AgentId,
        ToolStatus::AgentContext => ToolLineElement::AgentContext,
        ToolStatus::DiffAdded
        | ToolStatus::DiffRemoved
        | ToolStatus::Context
        | ToolStatus::Tools
        | ToolStatus::Counter => ToolLineElement::Counter,
        ToolStatus::WorkTitle => ToolLineElement::WorkTitle,
        ToolStatus::Info => ToolLineElement::Info,
        ToolStatus::Time => ToolLineElement::Duration,
        ToolStatus::Error => ToolLineElement::ResultStatus,
    }
}

/// Paints a [`ToolCallDisplay`] as an adaptive one-row themed header.
///
/// Identity, error details, arguments, agent ids, mode, and range use bounded
/// middle truncation. Result status and all numeric/informational chips stay
/// atomic so truncation cannot turn success into failure or vice versa.
pub(crate) fn render_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    render_tool_block_with_payload(theme, display, true)
}

/// Paints only the adaptive header of a [`ToolCallDisplay`] without cloning or
/// rendering its potentially large payload.
pub(crate) fn render_tool_header_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
) -> tau_cli_term::StyledBlock {
    render_tool_block_with_payload(theme, display, false)
}

/// Paints a tool header and optionally attaches its text payload body.
fn render_tool_block_with_payload(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    include_payload: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{
        PriorityLine, PriorityLineAlignment, PriorityLineTruncation, Span, StyledBlock, StyledText,
    };
    use tau_themes::names;

    const IDENTITY_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(4, 32);
    const ERROR_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(5, 48);
    const ARGUMENT_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(5, 48);
    const AGENT_ID_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(5, 32);
    const MODE_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(3, 16);
    const RANGE_BOUNDS: PriorityLineTruncation = PriorityLineTruncation::new(5, 32);

    let left = PriorityLineAlignment::Left;
    let mut line = PriorityLine::new();
    line.require_through(ToolLineElement::ResultStatus.priority());
    line.set_separator_style(overlay_style(
        resolve(theme, names::TOOL_OUTPUT),
        resolve(theme, names::TOOL_ARGS),
    ));
    if let Some((text, status)) = &display.status_prefix {
        line.push(
            tool_suffix_element(*status).priority(),
            left,
            tool_line_text(theme, tool_status_style(*status), text.clone()),
        );
    }
    line.push_truncated(
        ToolLineElement::Identity.priority(),
        left,
        tool_line_text(
            theme,
            display.tool_name_style.unwrap_or(names::TOOL_NAME),
            display.tool_name.clone(),
        ),
        IDENTITY_BOUNDS,
    );
    for segment in &display.leading_segments {
        let element = tool_suffix_element(segment.status);
        let text = if matches!(segment.status, ToolStatus::AgentContext) {
            watched_agent_context_text(theme, &segment.text)
        } else {
            tool_line_text(
                theme,
                tool_status_style(segment.status),
                segment.text.clone(),
            )
        };
        let attached = segment.no_leading_space || segment.text.starts_with(':');
        match element {
            ToolLineElement::AgentId => {
                if attached {
                    line.push_truncated_attached(element.priority(), left, text, AGENT_ID_BOUNDS);
                } else {
                    line.push_truncated(element.priority(), left, text, AGENT_ID_BOUNDS);
                }
            }
            ToolLineElement::WorkTitle | ToolLineElement::Info if !attached => {
                line.push_truncated(element.priority(), left, text, ARGUMENT_BOUNDS);
            }
            ToolLineElement::WorkTitle | ToolLineElement::Info => {
                line.push_truncated_attached(element.priority(), left, text, ARGUMENT_BOUNDS);
            }
            _ if attached => line.push_attached(element.priority(), left, text),
            _ => line.push(element.priority(), left, text),
        }
    }
    if !display.mode.is_empty() {
        line.push_truncated(
            ToolLineElement::Mode.priority(),
            left,
            tool_line_text(theme, names::TOOL_MODE, display.mode.clone()),
            MODE_BOUNDS,
        );
    }
    if !display.args.is_empty() {
        line.push_truncated(
            ToolLineElement::Arguments.priority(),
            left,
            tool_line_text(theme, names::TOOL_ARGS, display.args.clone()),
            ARGUMENT_BOUNDS,
        );
    }
    if let Some(range) = &display.range {
        line.push_truncated(
            ToolLineElement::Range.priority(),
            left,
            tool_line_text(theme, names::TOOL_ARGS, range.clone()),
            RANGE_BOUNDS,
        );
    }
    for suffix in &display.suffixes {
        let style_name = tool_status_style(suffix.status);
        if matches!(suffix.status, ToolStatus::Error) {
            let details = suffix.text.strip_prefix("err:").map(str::trim);
            line.push(
                ToolLineElement::ResultStatus.priority(),
                left,
                tool_line_text(theme, style_name, "err"),
            );
            if let Some(details) = details.filter(|details| !details.is_empty()) {
                line.push_truncated_attached(
                    ToolLineElement::ErrorDetails.priority(),
                    left,
                    tool_line_text(theme, style_name, format!(": {details}")),
                    ERROR_BOUNDS,
                );
            }
            continue;
        }
        let element = tool_suffix_element(suffix.status);
        let text = tool_line_text(theme, style_name, suffix.text.clone());
        let attached = suffix.no_leading_space || suffix.text.starts_with(':');
        match element {
            ToolLineElement::AgentId => {
                if attached {
                    line.push_truncated_attached(element.priority(), left, text, AGENT_ID_BOUNDS);
                } else {
                    line.push_truncated(element.priority(), left, text, AGENT_ID_BOUNDS);
                }
            }
            _ if attached => line.push_attached(element.priority(), left, text),
            _ => line.push(element.priority(), left, text),
        }
    }

    let mut body = StyledText::new();
    if include_payload && let Some(ToolUsePayload::Text { text }) = &display.payload {
        let style = overlay_style(
            resolve(theme, names::TOOL_OUTPUT),
            resolve(theme, names::TOOL_ARGS),
        );
        body = StyledText::from(Span::new(text.clone(), style));
    }

    StyledBlock::new("")
        .priority_line(line)
        .priority_line_body(body)
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

/// Like [`render_tool_block`] but attaches expanded unified-diff detail rows
/// when `expanded` is true and `diff` has hunks.
///
/// The adaptive priority-line header owns the body, so block layout wraps diff
/// rows independently and suppresses them if the essential header cannot fit.
pub(crate) fn render_diff_tool_block(
    theme: &tau_themes::Theme,
    display: &ToolCallDisplay,
    diff: &tau_proto::DiffSummary,
    expanded: bool,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::resolve;
    use tau_cli_term::{Span, StyledText};
    use tau_themes::names;

    // Reuse the adaptive header and attach only the expanded diff detail body.
    let header = render_tool_block(theme, display);

    if !expanded || diff.hunks.is_empty() {
        return header;
    }
    assert!(
        header.priority_line_content().is_some(),
        "tool block priority header"
    );
    let mut spans: Vec<Span> = Vec::new();

    let added_style = resolve(theme, names::DIFF_ADDED);
    let removed_style = resolve(theme, names::DIFF_REMOVED);
    let context_style = resolve(theme, names::DIFF_CONTEXT);
    let header_style = resolve(theme, names::DIFF_HUNK_HEADER);
    let added_inline_style = overlay_style(added_style, resolve(theme, names::DIFF_ADDED_INLINE));
    let removed_inline_style =
        overlay_style(removed_style, resolve(theme, names::DIFF_REMOVED_INLINE));

    for hunk in &diff.hunks {
        if !spans.is_empty() {
            spans.push(Span::new("\n", context_style));
        }
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
    header.priority_line_body(StyledText::from(spans))
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
    use tau_cli_term::{Span, StyledText};
    use tau_themes::names;

    let header = render_tool_block(theme, display);

    if !expanded || files.iter().all(|file| file.diff.hunks.is_empty()) {
        return header;
    }
    assert!(
        header.priority_line_content().is_some(),
        "tool block priority header"
    );
    let mut spans: Vec<Span> = Vec::new();

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
        if !spans.is_empty() {
            spans.push(Span::new("\n", context_style));
        }
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
    header.priority_line_body(StyledText::from(spans))
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
    let mut spans = vec![Span::new(
        crate::transcript_markers::NOTICE,
        resolve(theme, names::PROMPT_MARKER_SUBMITTED),
    )];
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
        Span::new(
            crate::transcript_markers::NOTICE,
            resolve(theme, names::PROMPT_MARKER_SUBMITTED),
        ),
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
    themed_block(
        theme,
        style_name,
        format!("{}{}", crate::transcript_markers::NOTICE, info.message),
    )
}

pub(crate) fn ui_dir_block(theme: &tau_themes::Theme, path: &Path) -> tau_cli_term::StyledBlock {
    system_path_block(theme, "ui dir: ", path, "/")
}

/// Renders the ordered configuration profile stack selected for this UI's
/// daemon.
pub(crate) fn config_profile_selection_block(
    theme: &tau_themes::Theme,
    selection: &str,
) -> tau_cli_term::StyledBlock {
    use tau_cli_term::resolve::themed_block;
    use tau_themes::names;

    themed_block(
        theme,
        names::SYSTEM_INFO,
        format!(
            "{}config profile stack: {selection}",
            crate::transcript_markers::STATUS_UPDATE
        ),
    )
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
    text.push(lifecycle, crate::transcript_markers::STATUS_UPDATE);
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
    text.push(info, crate::transcript_markers::STATUS_UPDATE);
    text.push(info, prefix);
    text.push(path_style, format!("{}{}", display_path(path), suffix));
    tau_cli_term::StyledBlock::new(tau_cli_term::resolve::themed_text(theme, &text))
}

/// Render initial-prompt context plus the count of other canonical session
/// skills.
pub(crate) fn agent_context_initialized_block(
    theme: &tau_themes::Theme,
    initialized: &tau_proto::HarnessAgentContextInitialized,
    unadvertised_skill_count: usize,
) -> tau_cli_term::StyledBlock {
    use tau_themes::{ThemedText, names};

    let mut text = ThemedText::new();
    let info = text.add_style(names::SYSTEM_INFO);
    let path_style = text.add_style(names::SYSTEM_PATH);
    let stats_style = text.add_style(names::TOOL_STATUS_INFO);
    text.push(info, crate::transcript_markers::STATUS_UPDATE);
    text.push(info, format!("initialized {}", initialized.agent_id));
    if !initialized.listed_skills.is_empty() || 0 < unadvertised_skill_count {
        text.push(info, "\nskills:");
        for skill in &initialized.listed_skills {
            text.push(info, "\n  ");
            text.push(path_style, skill.name.to_string());
            text.push(
                stats_style,
                format!(
                    " {}",
                    format_stats(
                        None,
                        Some(skill.description.lines().count() as u64),
                        Some(skill.description.len() as u64),
                    )
                ),
            );
        }
        if 0 < unadvertised_skill_count {
            text.push(
                info,
                format!(
                    "\n  {unadvertised_skill_count} other session skill{} available",
                    if unadvertised_skill_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
            );
        }
    }
    if !initialized.agents_files.is_empty() {
        text.push(info, "\nAGENTS.md:");
        for file in &initialized.agents_files {
            text.push(info, "\n  ");
            text.push(path_style, file.file_path.display().to_string());
            text.push(
                stats_style,
                format!(
                    " {}",
                    format_stats(None, Some(file.lines), Some(file.bytes))
                ),
            );
        }
    }
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
    text.push(info, crate::transcript_markers::STATUS_UPDATE);
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
    text.push(lifecycle, crate::transcript_markers::NOTICE);
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
