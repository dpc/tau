# GATE-configured-extension-trust-boundary: Configured extension trust boundary

## Gate

Configured extensions, including Providers, are trusted same-user executables.
The harness may use robust validation to preserve protocol and semantic
invariants, but must not introduce complex containment or sanitization solely
because a configured extension is presumed hostile.

## Justification

The user wants straightforward configured-extension interfaces while retaining
validation that protects Tau's own invariants. This boundary does not change the
separate treatment of externally sourced content.
