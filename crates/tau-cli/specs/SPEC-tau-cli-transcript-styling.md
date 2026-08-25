# SPEC-tau-cli-transcript-styling: Transcript styling

## Record justification

This contract spans CLI configuration, Markdown parsing, theme resolution, and terminal layout/emission, so no single local artifact can own it coherently.

Tau applies Markdown-lite formatting in the terminal UI only. The harness,
protocol events, durable agent logs, prompt previews, model context, and other
clients continue to see the original plain text.

The formatter is deliberately small. It recognizes headings, unordered and
ordered list markers, `*strong*` / `**strong**`, `_emphasis_`, combined
`***strong emphasis***`, `~~strikethrough~~`, basic backslash escapes, and
leading-pipe tables. Triple-asterisk runs compose strong and emphasis styles,
while strikethrough uses its own semantic style; this does not introduce a
general CommonMark parser. Most
constructs are style-only and preserve exact source characters rather than
stripping delimiters or rewriting list/header prefixes. Tables are the exception:
the UI may add bounded display-only padding spaces so cells align while the
visible text remains valid Markdown table syntax. Inline backticks, fenced code
blocks, and indented code-like lines get code styling and suppress nested
Markdown-lite styling; escaped marker sequences get escape styling. This keeps
live terminal wrapping, scrollback, and copy/paste behavior stable outside
intentional table padding.

Table bounds and alignment use terminal display columns, including the visible
OSC 8 link-label projection (or the label plus target fallback when OSC 8 is
disabled), rather than source bytes or Unicode scalar counts. The delimiter row
selects left (`---`/`:---`), right (`---:`), or centered (`:---:`) placement for
header and body cells; centered odd spare columns put the smaller share on the
left. Tau preserves the delimiter colons and bounds the final logical row width
and aggregate inserted padding before creating the display projection.

Inline links (`[label](target)`), HTTP(S) autolinks (`<url>`), and recognized
bare HTTP(S) URLs are the other intentional text transformation. With
`osc8_links: true`, inline links display only their clickable label to conserve
terminal space; autolinks and bare URLs display the clickable URL. Their visible
text uses the `markdown.link` theme style, which built-in themes make bold and
red, and carries structured OSC 8 metadata through layout and wrapping.
`cli.yaml` enables this behavior by default. Setting `osc8_links: false` removes
OSC 8 metadata and renders inline links as `label (target)`, allowing terminal
URL detection to remain useful; autolinks and bare URLs continue to display the
URL once. Link targets and labels remain subject to the terminal renderer
control-character sanitization boundary.

Bare URLs, autolinks, and explicit links nested inside supported strong,
emphasis, combined strong-emphasis, or strikethrough delimiters retain both
their hyperlink metadata and the surrounding semantic style. Delimiters remain
visible under the same copy/paste-preserving rule. Inline code and escaped text
inside those styles continue to suppress link recognition.

Headings and list markers are structural emphasis: built-in themes make them
bold without assigning a foreground, so they retain the surrounding user,
assistant, or thinking foreground (and background). Strong and emphasis likewise
compose attributes onto that surrounding style. Code, escapes, and
strikethrough remain independent semantic styles and may intentionally use
distinct colors.

Live response and thinking styling updates incrementally as complete lines
arrive and preserves parser context across chunks. An incomplete streamed line
remains base-styled until a newline or final rendering supplies a complete parse.
Final and static blocks parse the complete string immediately.

Formatting is scoped to submitted user prompts, assistant response text, and
reasoning/thinking text. Tool calls, tool payloads/results, shell output,
status/progress lines, and agent-to-agent message debug displays must stay on
their existing renderers unless there is a separate product decision.

Transcript state markers distinguish message lifecycle at a glance. By default,
submitted user prompts use `⬤`, while queued prompts and the currently composed
prompt use `◯`; configured submitted and prompt symbols replace those respective
defaults. Completed agent responses use `◆`, while responses still streaming
use `◇`.

The default dark theme's named `white` foreground renders submitted user prompt
text with ANSI palette index 15 (bright white), not ordinary-white index 7 or
bold promotion. Custom themes retain control of the `user.prompt` style,
including any explicit foreground override.

Fixed semantic transcript rows use category markers without rewriting their
content: agent-to-agent and external messages use `■`, harness-authored
structured status updates use `▤`, and harness or local UI notices use `□`.
These markers apply equally when the renderer folds live delivery, restored
history, or a retained hidden transcript.
`□` means textual Tau/UI output; it does not encode visibility, severity, or
whether the text also enters model context.

Session/UI directory announcements and agent-context initialization are
structured status updates. Extension lifecycle rows, disconnects, and
command/completion feedback are notices. Tool rendering and provider content
retain their own semantic renderers and do not acquire these markers.

When a submitted or steered prompt carries extension provenance, or
harness-internal provenance without a front-exact queued user-prompt
projection, it is a `■` message. A front-exact queued user prompt retains its
submitted-prompt marker when submission or steering promotes that exact queued
projection, regardless of source; queue records have no provenance. The
renderer does not consume a different front queued projection because a later
row happens to match.

Queued prompt presentation occupies at most two display rows when its ordinary
layout would exceed that bound. The first row keeps the beginning of the first
source line and ends with an omission marker; the second row starts with an
omission marker, keeps the end of the last source line, and ends with the queued
label. Narrow layouts compact those adornments rather than exceeding the current
terminal width. This abbreviation is presentation-only: dispatch, steering, and
recall retain the complete authoritative prompt text.

For bounded render work, prompts larger than the renderer's fixed source-window
limit use the abbreviated presentation without first laying out the complete
prompt.

Provider response stats are a standalone live-indicator status line. The CLI may
remember the latest `provider.response_updated.response_stats` sample for an
in-flight prompt only to repaint the transient ellipsis block, and derives
bytes/rate from the event's `current` and `previous` samples. The CLI renders a
generic `(elapsed, total bytes, Δinterval rate, average rate)` suffix only on
that transient indicator, not on visible assistant text, and must not copy stats
text into editor current-response state, prompt-stdin capture, durable
transcripts, or final response rendering. The live throughput suffix is a pure
render of the latest provider stats sample; the CLI must not interpolate elapsed
time or recompute `Δ` on redraw/timer ticks.

When stats carry first-semantic-output timing, the indicator inserts `first
output <duration>` after elapsed time. It renders milliseconds below five
seconds, whole seconds below five minutes, and whole minutes thereafter.
Absence renders no placeholder. The value has the same in-flight lifetime and
prompt/agent isolation as the surrounding indicator.
