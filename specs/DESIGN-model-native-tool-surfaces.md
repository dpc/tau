# DESIGN-model-native-tool-surfaces: Preserve model-native tool surfaces

Status: confirmed, 2026-07-15, dpc

Tau preserves the provider/model-native top-level shape of tool surfaces that
are deliberately offered to models trained on established tool interfaces.
For the ChatGPT/Codex shell surface, the compatibility reference is the
verified Codex CLI interface. Model-specific tool selection remains harness
policy over neutral extension and provider tags; it does not require the shell
extension to own model selection.

Compatibility is practical rather than exact emulation. Tau may make a small
intentional divergence for a concrete architectural, safety, or product
reason, but the difference must be explicit, justified, documented, and
covered at the provider-visible boundary. Internal implementation, transport,
and unrelated Codex CLI behavior are outside this decision.

Generic per-instance prefixes may structurally qualify names as defined by
[DESIGN-extension-tool-prefixes](DESIGN-extension-tool-prefixes.md); they do
not authorize arbitrary schema or semantic rewriting. The harness continues
to choose the effective prompt surface according to
[SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md),
using provider metadata described by
[ARCH-tau-provider-chatgpt](../crates/tau-provider-chatgpt/specs/ARCH-tau-provider-chatgpt.md)
and neutral shell metadata described by
[ARCH-tau-ext-shell](../crates/tau-ext-shell/specs/ARCH-tau-ext-shell.md).

The source audit at Codex revision `2f7d89b141` found that both its legacy
`shell_command` and unified `exec_command` use an invocation-local argument
named `workdir`, while `write_stdin` has no directory argument. Tau's
ChatGPT-facing `shell_command` therefore advertises only `workdir`: it does not
advertise or accept a runtime-only legacy `cwd` alias. This call-local
`shell_command.workdir` is distinct from Tau's top-level persistent
`workdir(path)` tool and must never update per-agent workdir state. Omitting it
uses the shell instance's remembered persistent workdir. Sibling calls in one
provider batch retain no causal ordering, so a persistent setter must complete
before a dependent call is made in a later turn.

## Rationale

Tool definitions are part of the effective interface between Tau and the
model, not merely an internal API. Preserving a familiar top-level interface
reduces avoidable model errors while leaving Tau free to retain its own
architecture and to make narrow, reviewable exceptions when compatibility
would conflict with stronger constraints.

This decision implements
[REQ-model-trained-tool-compatibility](REQ-model-trained-tool-compatibility.md).
