# SPEC-context-output-formatting: Context output formatting

## Record justification

These conventions span tool outputs and harness-injected context across producers, prompt assembly, and provider delivery, so no single implementation artifact owns them.

## Purpose

This record describes conventions for tool outputs and data the harness injects into an agent's context.

## Information and concision

Our goal is to provide all useful information clearly and precisely while keeping output concise and minimal.

Common anti-patterns include:

- Redundancy.
- Needless prose.
- Repeating data the agent already supplied as tool-call arguments.
- Restating information already implied by the tool or context.
- Success confirmations or separate notices that add no information.

An empty result is appropriate when there is nothing useful to report.

## Envelopes

Nearly every piece of data injected into an agent's context as a user prompt is wrapped in an XML-lite envelope to distinguish elements clearly and avoid confusion.

Envelope tags should have meaningful names. Optional attributes should carry information that is not already present in tool-output headers.

Injected notices and messages are not tool outputs. Their metadata, such as sender, agent, or task, belongs in envelope attributes rather than header-formatted body text.

## Tool outputs

Whenever possible, tool outputs are delivered inline as tool-result prompt elements. In other circumstances, such as deferred delivery after backgrounding or partial streamed updates, they are delivered inside envelopes.

Whether standalone or enveloped, tool outputs typically use `headers + single empty line + payload`. Both headers and payload may be empty or absent. The separating empty line is present only when both are present.

### Header formatting

Each header uses a `key: value` line. Header names should be clear and minimal.

Tool-output status belongs in the headers, not duplicated in envelope attributes. A `format` header can concisely describe the payload's layout.

Structured artifact tool-output fields and headers use bare artifact keys.
Markdown/context prose uses `<tau-artifact:…>` autolinks; plain-text UI locators
may use `tau-artifact:…` without brackets.

### Errors

The `error` header contains a short error identifier rather than a prose explanation. Useful details, such as a conflicting path, belong in separate headers without repeating information already supplied by the agent.

Explanations and recovery guidance belong in the payload. Concise prose that helps the agent understand and correct a mistake is useful, not needless.
