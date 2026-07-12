# DESIGN-tau-cli-theme-defaults: Theme defaults

Status: confirmed, 2026-06-17, dpc

The built-in `tau-plain-dark` theme is intentionally conservative. It keeps
semantic text attributes such as bold, italic, underline, and strikethrough, and
limits hard-coded foreground colors to default color plus yellow, dark yellow,
cyan, green, and red. Dark yellow is used for passive watched-agent `watching`
labels so they do not look like active yellow tool calls. Those colors are
considered generally safe terminal colors, while other `tau-dpc` theme colors
are dropped or mapped so Tau remains readable on unusual terminal palettes. More
opinionated built-ins, including the personalized `tau-dpc` theme and the
light-background `tau-plain-light` theme, remain selectable but are not the
default.
