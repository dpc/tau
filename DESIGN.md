# tau Design Notes

Living document of architectural decisions and design direction.

Pre-implementation design exploration lives in `doc/ORIGINAL_DESIGN.md`.

## Core threading model

Prefer a single thread blocking on one `mpsc::Receiver`, reacting to
events. All I/O with external processes or sockets uses a pair of
threads per connection (one reading, one implicitly via synchronous
write from the event loop). Reader threads decode incoming data and
send tagged events into the shared channel. The main loop dispatches
immediately — no polling, no timeouts, no `select!`.

```
  reader thread (ext stdout) ──┐
  reader thread (ext stdout) ──┤
  reader thread (socket)     ──┼──► mpsc::Receiver ──► event loop ──► write to sinks
  accept thread (listener)   ──┘
```

This gives:
- instant wakeup on any event from any source
- no wasted cycles on idle sources
- linear scaling with extension count (adding sources is just cloning
  the Sender)
- simple reasoning — one thread owns all mutable state

Timeouts are acceptable only as safety nets (e.g. extension startup),
never as the primary dispatch mechanism.

## Process topology

```
  tau chat ──(unix socket)──► tau component harness ──(stdio)──► extensions
  (UI client)                  (daemon process)                  (agent, fs, shell)
```

- `tau chat` always connects to a harness daemon as a socket client.
  By default it starts a fresh daemon; future `tau chat -r` will
  reattach to an existing one.
- The harness daemon manages extensions, the event bus, tool routing,
  and session persistence.
- Extensions are child processes speaking CBOR over stdio.
- The chat's TUI renders events received from the harness via a
  background reader thread and the thread-safe `TermHandle`.

## Daemon runtime directory

Adapted from selfci's mq daemon. Each harness instance gets a
directory under `$XDG_RUNTIME_DIR/tau/{pid}/` containing:

- `tau.sock` — Unix socket for client connections
- `tau.dir` — project root path (discovery marker)
- `tau.pid` — daemon process PID

The marker file `tau.dir` is written **after** extensions are started
and the socket is bound. Finding it guarantees the harness is ready.

## Event protocol

CBOR-encoded events over self-delimiting streams (stdio pipes or Unix
sockets). Same protocol regardless of transport. Event families:

- `lifecycle.*` — hello, subscribe, ready, disconnect
- `tool.*` — register, unregister, request, invoke, result, error,
  progress, cancel, cancelled
- `message.*` — user, agent
- `extension.*` — starting, ready, exited, restarting

The bus routes events to subscribed connections. Tool requests are
resolved to a live provider and forwarded as directed invocations.

## Extension model

Extensions are standalone executables. Each extension:
- starts as an ordinary process
- handshakes (hello → subscribe → register tools → ready)
- sends and receives events over stdin/stdout
- is automatically unregistered when it disconnects

Tool ownership is explicit — the harness tracks which connection
provides each tool and routes requests accordingly.

## Single binary

Tau ships as one binary containing CLI, harness, agent, and built-in
tools. The hidden `tau component <name>` subcommand dispatches to
extension entry points. See `ARCHITECTURE.md` for details.
