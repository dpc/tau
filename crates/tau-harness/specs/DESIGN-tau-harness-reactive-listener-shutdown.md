# DESIGN-tau-harness-reactive-listener-shutdown: Daemon listener shutdown is reactive and path-independent

Status: unconfirmed

The harness daemon accept forwarder must not poll on fixed sleeps while waiting
for clients or shutdown. It should block in an OS readiness primitive over the
listener fd and an owned wake/cancellation fd, then accept ready clients and exit
promptly when the wake fd fires.

Shutdown must not rely on reconnecting to the daemon socket pathname. Runtime
socket paths can be removed or replaced while the listener fd remains alive, so
the wake primitive has to be owned by the forwarder thread. Tests for this area
should cover idle shutdown, missing/replaced socket paths, and the invariant that
internal wake traffic is never delivered as a harness client.
