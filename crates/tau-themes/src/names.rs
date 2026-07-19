//! Canonical style name constants.
//!
//! These match the semantic keys used by Tau's built-in themes.

// -- User input --
pub const USER_PROMPT: &str = "user.prompt";
pub const USER_PROMPT_QUEUED: &str = "user.prompt.queued";

// -- Agent responses --
pub const AGENT_RESPONSE: &str = "agent.response";
pub const AGENT_PENDING: &str = "agent.pending";
/// Live + finalized provider-supplied reasoning summary, rendered as
/// a separate block above the assistant response.
pub const AGENT_THINKING: &str = "agent.thinking";

// -- Markdown-lite transcript formatting --
pub const MARKDOWN_STRONG: &str = "markdown.strong";
pub const MARKDOWN_EMPHASIS: &str = "markdown.emphasis";
pub const MARKDOWN_STRIKETHROUGH: &str = "markdown.strikethrough";
pub const MARKDOWN_HEADING: &str = "markdown.heading";
pub const MARKDOWN_LIST_MARKER: &str = "markdown.list.marker";
pub const MARKDOWN_CODE: &str = "markdown.code";
pub const MARKDOWN_ESCAPE: &str = "markdown.escape";

// -- Tool execution --
//
// Tool-call blocks are composed of three spans: the tool name at the
// start, its arguments in the middle, and a status suffix at the end.
// Each span has its own style so a theme can paint them differently.
pub const TOOL_OUTPUT: &str = "tool.output";
pub const TOOL_NAME: &str = "tool.name";
pub const WATCHING_NAME: &str = "watching.name";
pub const TOOL_MODE: &str = "tool.mode";
pub const TOOL_ARGS: &str = "tool.args";
pub const TOOL_STATUS_SUCCESS: &str = "tool.status.success";
pub const TOOL_STATUS_ERROR: &str = "tool.status.error";
pub const TOOL_STATUS_INFO: &str = "tool.status.info";
pub const TOOL_STATUS_TIME: &str = "tool.status.time";
pub const SHELL_OUTPUT: &str = "shell.output";

// -- Action output --
pub const ACTION_OUTPUT: &str = "action.output";
pub const ACTION_LABEL: &str = "action.label";
pub const ACTION_VALUE: &str = "action.value";
pub const ACTION_ID: &str = "action.id";
pub const ACTION_ERROR: &str = "action.error";

/// Streaming-`…` indicator appended to in-progress agent responses,
/// thinking blocks, and running tool-call lines. Painted independently
/// from the surrounding body so themes can make it stand out.
pub const PROGRESS_INDICATOR: &str = "progress.indicator";

// -- Extensions --
pub const EXTENSION_LIFECYCLE: &str = "extension.lifecycle";
pub const EXTENSION_STATUS: &str = "extension.status";

// -- Sessions --
pub const SESSION_STATUS: &str = "session.status";

// -- System --
pub const SYSTEM_INFO: &str = "system.info";
/// Sender and recipient identities in agent-message routing headers.
pub const AGENT_MESSAGE_IDENTITY: &str = "agent.message.identity";
pub const SYSTEM_INFO_IMPORTANT: &str = "system.info.important";
pub const SYSTEM_DISCONNECT: &str = "system.disconnect";
pub const SYSTEM_PATH: &str = "system.path";
pub const SYSTEM_STATUS: &str = "system.status";

// -- Status bar --
pub const MODEL_STATUS: &str = "model.status";
pub const STATUS_MODEL: &str = "status.model";
pub const STATUS_ROLE: &str = "status.role";
pub const STATUS_SESSION: &str = "status.session";
pub const STATUS_CONTEXT: &str = "status.context";
pub const STATUS_QUOTA_UNDER: &str = "status.quota.under";
pub const STATUS_QUOTA_ALIGNED: &str = "status.quota.aligned";
pub const STATUS_QUOTA_OVER: &str = "status.quota.over";
pub const STATUS_QUOTA_DANGER: &str = "status.quota.danger";
pub const STATUS_QUOTA_UNKNOWN: &str = "status.quota.unknown";
pub const STATUS_EFFORT: &str = "status.effort";
pub const STATUS_VERBOSITY: &str = "status.verbosity";
pub const STATUS_SERVICE_TIER: &str = "status.service_tier";
pub const STATUS_TOOLS: &str = "status.tools";
pub const STATUS_AGENTS: &str = "status.agents";
pub const STATUS_UI_IO_LOW: &str = "status.ui_io.low";
pub const STATUS_UI_IO_MEDIUM: &str = "status.ui_io.medium";
pub const STATUS_UI_IO_HIGH: &str = "status.ui_io.high";
pub const REDRAW_COUNTER: &str = "redraw.counter";

// -- Token stats --
pub const TOKEN_STATS: &str = "token.stats";
pub const TOKEN_STATS_DELTA: &str = "token.stats.symbol.delta";
pub const TOKEN_STATS_SIGMA: &str = "token.stats.symbol.sigma";
pub const TOKEN_STATS_UP: &str = "token.stats.symbol.up";
pub const TOKEN_STATS_DOWN: &str = "token.stats.symbol.down";
pub const TOKEN_STATS_CACHE_HIT: &str = "token.stats.metric.cache_hit";
pub const TOKEN_STATS_CACHE_WARN: &str = "token.stats.metric.cache_warn";
pub const TOKEN_STATS_CACHE_MISS: &str = "token.stats.metric.cache_miss";
pub const TOKEN_STATS_INPUT: &str = "token.stats.metric.input";
pub const TOKEN_STATS_OUTPUT: &str = "token.stats.metric.output";
pub const TOKEN_STATS_LATENCY: &str = "token.stats.metric.latency";

// -- Completion menu --
pub const COMPLETION_LABEL: &str = "completion.label";
pub const COMPLETION_DESC: &str = "completion.desc";
pub const COMPLETION_SELECTED: &str = "completion.selected";

// -- Prompt --
pub const PROMPT_MARKER: &str = "prompt.marker";
/// Combined `<cwd> <&session-id>` context shown in the right prompt.
pub const PROMPT_CWD: &str = "prompt.cwd";
/// Placeholder text shown in the prompt when the input buffer is empty.
pub const PROMPT_PLACEHOLDER: &str = "prompt.placeholder";
pub const PROMPT_MARKER_SUBMITTED: &str = "prompt.marker.submitted";

// -- Banner --
pub const BANNER_ACCENT: &str = "banner.accent";
pub const BANNER_LOGO: &str = "banner.logo";
pub const BANNER_NAME: &str = "banner.name";
pub const BANNER_VERSION: &str = "banner.version";
pub const BANNER_BUILD: &str = "banner.build";
pub const BANNER_PUN: &str = "banner.pun";

// -- Diffs --
//
// File-mutation tools attach a structured `DiffSummary` to their result. The
// renderer paints each hunk line with a tag-specific style; intra-line `Modify`
// segments use the
// inline variants so changed tokens pop out of the surrounding
// (otherwise red/green) line.
pub const DIFF_ADDED: &str = "diff.added";
pub const DIFF_REMOVED: &str = "diff.removed";
pub const DIFF_CONTEXT: &str = "diff.context";
pub const DIFF_HUNK_HEADER: &str = "diff.hunk_header";
pub const DIFF_ADDED_INLINE: &str = "diff.added.inline";
pub const DIFF_REMOVED_INLINE: &str = "diff.removed.inline";
