# SPEC-extension-declaration-inspection: Cooperative declaration-only startup

## Status

The protocol and SDK bootstrap are available. Built-in opt-ins and the separate
CLI collector are not yet connected; ordinary extension entrypoints continue to
advertise no inspection support.

## Record justification

Purpose selection, cooperative extension bootstrap, declaration construction,
and collection are necessarily distributed across protocol peers, the SDK, and
the inspection caller, so no single local artifact owns their contract.

## Contract

Inspection is a separately selected declaration preview, never a replacement for
effective prompt/tool rendering or ordinary extension startup. The collector
checks compatible protocol major and explicit Hello inspection support before
sending a declaration-inspection Configure. Unsupported peers receive no
Configure and never fall back to normal startup.

Inspection support is an additive Hello field, separate from protocol
authorities. Ordinary Configure omits its runtime-default purpose; existing
normal harnesses can ignore the new Hello field without decoding a new
capability-enum variant. Inspection completion is a distinct directed message,
not Ready or a published event. Ordinary harness activation rejects it.

An opted-in executable waits for purpose selection before runtime registration,
state factories, ordinary Configure handlers, workers, storage, secret retrieval,
network activity or project discovery. Inspection permits supplied configuration,
instance/prefix, explicitly selected non-secret settings, in-memory computation
and protocol I/O. Configure carries no state directory or authorized secrets.
Extensions share pure declaration constructors with normal registration; anything
that depends on operational state remains explicitly unavailable.

The terminal inventory contains typed tool/group declarations, initial prompt
fragments, provider/model declarations and closed completeness reason codes.
Structural names use the normal prefix mapping. It contains neither raw config
nor free-form configuration-error text. An empty gap list means only that the
extension computed its complete declaration inventory: runtime context,
credentials, model policy, service availability and effective tool selection
remain unverified. Session/agent context and filesystem discovery are not inferred
from fake runtime lifecycle events.

The collector is separate from normal harness construction and owns bounded
memory-only collection and finite child cleanup. Existing effective previews and
normal readiness/collision/recovery semantics remain unchanged. Preconstructed
state and arbitrary executable bootstrap cannot retroactively be made pure by
using an SDK API; their callers must explicitly move that work behind the
ordinary continuation before advertising support.

This is a cooperative contract for trusted same-user executables, not a sandbox
or a promise that no extension code executes. It preserves
[GATE-configured-extension-trust-boundary](GATE-configured-extension-trust-boundary.md)
and is subject to
[GATE-persistence-and-extension-interface-change-approval](GATE-persistence-and-extension-interface-change-approval.md).
