# REQ-independent-manipulation-extension-instances: Independent manipulation instances

Source: dpc, confirmed 2026-07-15

Tau must support zero, one, or multiple independently configured filesystem and
shell manipulation extension instances. Instances may operate in different host,
container, or SSH filesystem namespaces, so Tau must not require or synthesize
one global agent working directory.

Each active surface must expose enough current path context to use it safely
without duplicating tool discovery. Durable state, initialization, replay, and
inheritance must preserve instance independence. A path originating in the
harness namespace must not be copied into another configured extension's
namespace merely because that extension implements shell tools.

The approved implementation choice is recorded in
[DECISION-per-agent-extension-workdirs](DECISION-per-agent-extension-workdirs.md).
