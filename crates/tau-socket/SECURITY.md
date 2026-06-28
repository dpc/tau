# Security and reliability notes

`tau-socket` is local Unix-domain IPC. It does not provide authentication,
encryption, network exposure, or cross-user isolation by itself. Callers must use
private per-user runtime directories and appropriate filesystem permissions for
socket paths.

Reliability-sensitive invariants:

- Client-side `SocketPeer` sends `HarnessInputMessage` and receives
  `HarnessOutputMessage`.
- Server-side `SocketAcceptedClient` receives `HarnessInputMessage` and sends
  `HarnessOutputMessage`.
- `SocketReceive::Timeout`, `SocketReceive::Closed`, and decoded messages remain
  distinct; malformed or truncated frames must be decode errors.
- Binding must not unlink non-socket paths or active sockets.
- Active-socket probing intentionally creates a short-lived local connection
  that an already-running daemon can observe. Existing socket paths are treated
  as stale only after a refused connection; other probe failures must fail closed.
- Drop-time cleanup must remove only the socket path created by that listener.
- Background reader threads must stop when their `SocketPeer` is dropped, and
  unread output must not buffer without bound.

Future changes to listener cleanup, receive semantics, reader lifecycle, or
protocol direction must update `ARCHITECTURE.md`, this file, rustdoc, and focused
regression tests together.
