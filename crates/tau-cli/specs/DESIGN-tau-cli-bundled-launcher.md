# DESIGN-tau-cli-bundled-launcher: Bundled component launcher

Status: confirmed, 2026-06-17, dpc

The unified `tau` binary launches in-process bundled programs with the
`tau component <component>` subcommand. This vocabulary is intentionally broader
than "extension": bundled extensions such as `ext-shell` and
`ext-provider-builtin` are components, but the harness is also a component and
is not an extension. Internal harness startup and built-in extension defaults
should therefore use `tau component harness` and `tau component <extension>`;
`tau ext <name>` is not a supported compatibility alias.
