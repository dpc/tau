Before changing this crate, discover and read the applicable Linked Specs in
`specs/` and every wider `specs/` scope, then follow relevant links.

- Before introducing a new free-form payload kind, or reframing an existing one,
  in the shared generic user-role `ContentPart::Text` carrier, read
  [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md)
  and
  [`SPEC-exact-sentinel-prompt-envelopes`](../../specs/SPEC-exact-sentinel-prompt-envelopes.md);
  use the shared registry rather than a component-local provenance wrapper.
  Typed tool results and the system/developer prompt channel are outside that
  rule.
