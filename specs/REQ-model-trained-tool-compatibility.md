# REQ-model-trained-tool-compatibility: Model-trained tool compatibility

Source: dpc, confirmed 2026-07-15

For Codex/ChatGPT models, Tau must keep its shell-access tool shapes and
semantics as close as practical to the corresponding verified Codex CLI
interfaces. These models are trained to use those interfaces, and material
differences increase tool-call mistakes.

The provider-visible top-level tool shape is the primary compatibility
boundary. Small differences are permitted when Tau's architecture, safety, or
product needs justify them, but each intentional divergence must have an
explicit rationale and appropriate test and documentation coverage. This
requirement does not require wholesale Codex CLI emulation.

Acceptance requires comparisons to be grounded in verified upstream source or
behavior rather than assumptions. Compatible definitions and semantics must
have regression coverage at the provider-visible boundary, while accepted
differences must be identifiable in current documentation and tests. At the
verified Codex CLI revision, both legacy `shell_command` and unified
`exec_command` name their invocation-local directory argument `workdir`;
`write_stdin` has no directory argument. Tau's corresponding advertised
call-local argument must therefore be `workdir`, and using it must not
persistently change the agent's remembered workdir.

The selected implementation approach is recorded in
[DESIGN-model-native-tool-surfaces](DESIGN-model-native-tool-surfaces.md).
Exact Codex compatibility findings are tracked in `tau-agent-80v9`; the
workdir refinement is related to `tau-agent-k5tn`.
