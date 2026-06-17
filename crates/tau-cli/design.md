# tau-cli design decisions

This file records local terminal-UI design decisions that future changes should
preserve unless the project intentionally revisits them. It complements the
crate README/AGENTS instructions with durable rationale for transcript rendering
and other UI boundaries.

## Markdown-lite transcript styling

Status: confirmed, 2026-06-15, dpc


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

Live response and thinking blocks use an append-aware cache. Text before a blank
line is treated as sealed and parsed once; the current unsealed suffix remains
base-styled until a future update seals it. The cache also preserves parser
context, including open fenced code blocks, across sealed chunks. Final/static
blocks parse the complete string immediately.

Formatting is scoped to submitted user prompts, assistant response text, and
reasoning/thinking text. Tool calls, tool payloads/results, shell output,
status/progress lines, and agent-to-agent message debug displays must stay on
their existing renderers unless there is a separate product decision.

## Bundled component launcher

Status: confirmed, 2026-06-17, dpc

The unified `tau` binary launches in-process bundled programs with the
`tau component <component>` subcommand. This vocabulary is intentionally broader
than "extension": bundled extensions such as `ext-shell` and
`ext-provider-builtin` are components, but the harness is also a component and
is not an extension. Internal harness startup and built-in extension defaults
should therefore use `tau component harness` and `tau component <extension>`;
`tau ext <name>` is not a supported compatibility alias.

## Notice filtering

Status: confirmed, 2026-06-17, dpc

Harness/UI notices are filtered in the terminal UI, not at the harness emission site. The default threshold is `info`; `/set notice-level <level>` and persisted `cli.json` `notice_level` change what routine notices a UI renders. Critical notices and `always_show` warning diagnostics remain visible regardless of threshold. UI special-casing must use the stable `harness.notice.kind` field rather than parsing notice text.

## Theme defaults

Status: confirmed, 2026-06-17, dpc

The built-in `tau-plain-dark` theme is intentionally conservative. It keeps
semantic text attributes such as bold, italic, underline, and strikethrough, and
limits hard-coded foreground colors to default color plus yellow, cyan, green,
and red. Those colors are considered generally safe terminal colors, while other
`tau-dpc` theme colors are dropped or mapped so Tau remains readable on unusual
terminal palettes. More opinionated built-ins, including the personalized
`tau-dpc` theme and the light-background `tau-plain-light` theme, remain
selectable but are not the default.

## tau-cli testing strategy

Status: unconfirmed

Pure transcript renderers should be tested at the rendered block/span boundary,
not by snapshotting built-in theme implementation details. Rendering and theme
behavior tests must use representative fixture themes with distinct semantic
attributes, assert exact text preservation except for documented display-only
transforms such as table padding, and check that the resolved spans carry the
intended semantic styling. Built-in theme tests should only validate that the
embedded files parse and satisfy intentional theme-level invariants, so built-in
theme tweaks do not force unrelated renderer expectation churn.
