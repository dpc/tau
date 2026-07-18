# SPEC-tau-cli-transcript-styling: Transcript styling


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

Live response and thinking styling updates incrementally as complete lines
arrive and preserves parser context across chunks. An incomplete streamed line
remains base-styled until a newline or final rendering supplies a complete parse.
Final and static blocks parse the complete string immediately.

Formatting is scoped to submitted user prompts, assistant response text, and
reasoning/thinking text. Tool calls, tool payloads/results, shell output,
status/progress lines, and agent-to-agent message debug displays must stay on
their existing renderers unless there is a separate product decision.

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
