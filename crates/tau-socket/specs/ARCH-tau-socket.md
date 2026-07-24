# ARCH-tau-socket: tau-socket architecture

`tau-socket` owns Tau's local Unix-domain socket transport adapters. It does not
own higher-level daemon policy, runtime-directory selection, socket activation,
or external listener lifetimes.

## Directionality

`SocketPeer` is the client-side adapter: it sends `HarnessInputMessage` values
toward the harness and receives `HarnessOutputMessage` values as explicit
`SocketReceive` outcomes. `SocketListener::accept` instead returns a
`SocketAcceptedClient`, which receives harness inputs and sends harness outputs
for one accepted server-side peer. Listener accept paths must preserve that
protocol direction.

## Listener ownership

`SocketListener::bind` creates parent directories, rejects non-socket and active
paths, removes only sockets proven stale by a refused connection, and removes
only its own path on drop. Probe failures other than `ConnectionRefused` fail
closed so permission and platform errors cannot unlink an active socket. The
probe may create an observable short-lived connection to a running daemon.

Binding is not a cross-process synchronization primitive: callers must
serialize startup or use private runtime directories because another process
can race between probing and stale-path removal.

## Reader lifecycle and boundary

`SocketPeer` uses a bounded background reader queue. Dropping the peer closes
the queue and stream and joins the reader thread. Timeout, closure, and decoded
messages remain distinct outcomes; malformed or truncated frames are decode
errors.

This crate provides no authentication, encryption, network exposure, or
cross-user isolation. Callers must place sockets in private per-user runtime
directories with appropriate filesystem permissions.
