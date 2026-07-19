# DECISION-model-native-tool-surfaces: Preserve model-native tool surfaces

Authority: confirmed, 2026-07-15, dpc

Tau preserves the provider/model-native top-level shape of tool surfaces that
are deliberately offered to models trained on established tool interfaces. For
the ChatGPT/Codex shell surface, the compatibility reference is the verified
Codex CLI interface. Model-specific selection remains harness policy over
neutral extension and provider tags.

Compatibility is practical rather than exact emulation. A divergence requires a
concrete architectural, safety, or product reason and provider-visible coverage.
Tool definitions are part of the model interface, so familiar top-level shapes
reduce avoidable model errors at the cost of an explicit compatibility constraint.

This decision implements
[REQ-model-trained-tool-compatibility](REQ-model-trained-tool-compatibility.md).
