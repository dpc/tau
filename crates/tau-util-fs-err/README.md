# tau-util-fs-err

Tau's reviewed façade over `fs-err`.

Use this crate for ordinary filesystem operations whose errors may be shown to
an operator. It keeps one workspace dependency and feature policy while adding
the attempted operation and path to `std::io::Error` display text.

The dependency uses default features only. Do not enable `debug`,
`debug_tokio`, or `expose_original_error`: the default contextual display keeps
the OS reason, while the public outer error has no `source()` and its
`raw_os_error()` is `None`.

The full supplied path is rendered with lossy, unescaped `Path::display()`:
invalid bytes become replacement characters, control characters remain active,
and sensitive paths are not redacted. Keep raw-errno branches, typed domain
errors, and redacted, model-visible, provider-visible, or secret-facing
boundaries on deliberately reviewed implementations.

Callers should expose `std::io::Error` rather than façade wrapper types in
public APIs. `OpenOptions::options().open(...)` bypasses path context; use the
wrapper's `open()` method instead.
