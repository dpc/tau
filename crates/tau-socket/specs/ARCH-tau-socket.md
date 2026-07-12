# ARCH-tau-socket: tau-socket architecture

`tau-socket` contains the Unix-domain socket transport adapters for Tau protocol
clients and accepted server-side peers.

## Directionality

`SocketPeer` is the client/peer-side adapter:

- `send` writes `HarnessInputMessage` values toward the harness.
- `recv_timeout` reads `HarnessOutputMessage` values from the harness and returns
  an explicit `SocketReceive` outcome.

`SocketListener::accept` returns `SocketAcceptedClient`, the harness/server-side
adapter for one accepted client:

- `recv` reads `HarnessInputMessage` values from the peer.
- `send` writes `HarnessOutputMessage` values back to the peer.

Do not return `SocketPeer` from listener accept paths; that reverses protocol
direction and makes the public listener API unusable for server code.

## Listener ownership

`SocketListener::bind` owns simple path-based listener setup and teardown:

- create parent directories,
- refuse pre-existing non-socket paths,
- refuse active sockets,
- refuse socket paths whose active/stale status cannot be determined,
- remove inactive stale sockets,
- remove only its own socket path on drop.

Higher-level daemon policy, runtime-directory selection, socket activation, and
external listener lifetimes remain outside this crate unless deliberately moved
here with corresponding integration changes. The production daemon listener path is
`crates/tau-harness/src/daemon.rs::{open_listener, bind_listener}` and should use
this crate's safe bind/cleanup APIs rather than duplicating blind path cleanup.

`SocketListener::bind` is not a cross-process synchronization primitive. Callers
should still serialize daemon startup or use private runtime directories because
another process can race between active-socket probing and stale-socket removal.
The active-socket probe intentionally opens a short-lived connection that can be
observed by an already-running daemon. Socket paths are treated as stale only
when that probe fails with `ConnectionRefused`; other probe failures are refused
so permission or platform-specific errors do not cause an active socket to be
unlinked.

## Reader lifecycle

`SocketPeer` uses a bounded background reader queue so unread protocol output
does not grow without bound. Dropping `SocketPeer` drops the receive queue,
shuts down the stream, and joins the reader thread.

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
protocol direction must update this record, rustdoc, and focused
regression tests together.
