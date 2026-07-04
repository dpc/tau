# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Single-threaded script runtime with supervised shell workers

Status: unconfirmed

The Rhai interpreter runs on one main runtime thread, while tau-client-owned
harness reading/writing and crate-owned shell execution use helper threads.
Shell workers are owned by runtime state through cancellation/process-group
handles and join handles, so disconnect and runtime drop synchronously cancel,
kill, and reap pending shell work before `run` returns, subject to a bounded
shutdown join timeout.

This keeps script execution non-concurrent while still allowing host shell
commands to run without blocking harness frame handling.

Shell output capture is also bounded after foreground completion, timeout, or
cancellation. Unix commands run in an owned process group/session, but detached
descendants can survive with inherited stdout/stderr pipes; the runtime drains
only immediately available pipe output for a bounded post-stop window instead of
waiting for pipe EOF.

## Protocol tests drive `run`

Status: unconfirmed

Behavior tests for this crate should prefer serialized Tau protocol frames sent
through `run` and assertions on outbound frames. Shell behavior should be tested
through Rhai tools returning `ShellJob` when possible, so tests cover script API
admission, deferred tool result/error emission, tau-client startup staging, and
shell process supervision together.

## Rhai tools are currently untagged

Status: unconfirmed

`register_tool` does not expose Tau `ToolTag`s yet. Rhai tools are registered
without tag metadata, so tag-based role/model policy will not match them until
the Rhai tool-registration API grows validated tag support.
