# tau-client

`tau-client` provides the shared runtime used by standalone Tau extensions and
other harness protocol peers.

It handles the extension startup sequence, typed configuration and event
dispatch, outbound protocol messages, manual event loops, and standard logging
setup. The runtime advertises the protocol revision exported by `tau-proto`.

The crate's Rust API follows its Cargo package version. Tau's wire protocol has
an independent major/minor revision.
