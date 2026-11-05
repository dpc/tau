# dpc-tau-client

## Build identity

With SDK 0.22.0 / protocol 16.7, extensions can report their own package release
and source revision in the initial Hello. Older peers omit or ignore this
optional diagnostic metadata. `self_info` shows current ready instances and
reports missing metadata as `unknown`, without substituting the harness's version.

Pin `dpc-tau-client` and `dpc-tau-proto` to `=0.22.0` after publication. Add
`built = { version = "0.8", default-features = false, features = ["git2"] }` to
the extension's `[build-dependencies]`, then generate metadata in its `build.rs`:

```rust
fn main() {
    built::write_built_file().expect("generate build metadata");
}
```

Include it from `src/built_info.rs`:

```rust
include!(concat!(env!("OUT_DIR"), "/built.rs"));
```

Declare `mod built_info;` and add this call to `TauExtension::register`:

```rust,ignore
builder.build_identity(tau_client::package_build_identity!(built_info));
```

Declaration-aware extensions must also set the same `Some(...)` identity on
the Hello passed to `prepare_inspection`. Raw protocol implementations can
populate `BuildIdentity` directly. Use the extension package's release and
source revision, never SDK versions or inherited `TAU_BUILD` environment values.

The helper uses caller-generated Git metadata for source builds and marks dirty
trees with `-modified`. For archives without Git metadata, it reports an unknown
hash.
For Git-free Nix or release builds, packagers can fill the shared executable's
40-byte `__TAU_BUILD_GIT_REVISION_PLACEHOLDER____` and 17-byte
`__TAU_BUILD_DIRTY` slots after linking, using the package's own source SHA and
`clean____________` or `modified_________`. Preserve widths and verify replacement
counts. Stamped metadata takes precedence, so never stamp separately sourced
extension executables with Tau core's hash. The helper does not read the runtime
environment or launch subprocesses.

`dpc-tau-client` provides the shared runtime used by standalone Tau extensions and
other harness protocol peers.

Generic activity lease requests/results are available as typed event payloads.
Subscribe to live results before sending acquire; require harness publisher
attribution and matching session, owner, operation, and action before effects.
Release after effect and context settlement. These transient, connection-owned
holds prevent idle shutdown only, have no expiry, and are not restored on replay.

It handles the extension startup sequence, typed configuration and event
dispatch, outbound protocol messages, manual event loops, and standard logging
setup. The runtime advertises the protocol revision exported by
`dpc-tau-proto`.

The crate's Rust API follows its Cargo package version. Tau's wire protocol has
an independent major/minor revision.

Live typed handler contexts and raw deliveries expose the harness-authenticated
`publisher`; historical/synthetic deliveries may omit it. Domain protocols should
use live-only handlers and validate publisher identity, session, and their own
request/incarnation correlation rather than trusting payload-local source labels.
Generic custom-event `agent_id` metadata controls private-agent diagnostics; it
does not route or interpret the payload.

Configured extensions can send `ExtPassiveContextAppendRequest` to append
attributed output to a loaded agent without activating inference. Observe its
correlated `ExtPassiveContextAppendResult`: sending is not a commit acknowledgement.
The same publisher/agent/operation ID and body is idempotent; changed content
under an existing ID fails. Failure or silence never authorizes repeating the
external operation that produced the output.
