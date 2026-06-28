# tau-socket architecture

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
