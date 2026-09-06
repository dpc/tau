# tau-proto

`tau-proto` defines Tau's harness-peer wire messages, extension-visible event
schemas, identifiers, and CBOR codec helpers.

The crate also exports `PROTOCOL_VERSION`, the protocol revision compiled into
clients and extensions. That protocol revision is independent of this crate's
Cargo package version.

The protocol is under active development. A matching major protocol revision
is required for admission; minor skew is best-effort and may still expose
behavioral differences.
