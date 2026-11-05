# dpc-tau-client

## Build identity

With SDK 0.23.0 / protocol 16.7, extensions can report their own package release
and source revision in the initial Hello. Older peers omit or ignore this
optional diagnostic metadata. `self_info` shows current ready instances and
reports missing metadata as `unknown`, without substituting the harness's version.

Pin `dpc-tau-client` and `dpc-tau-proto` to `=0.23.0` after publication. Keep the
ordinary runtime dependency and enable the shared capture helper only in the
build dependency:

```toml
[build-dependencies]
tau-client = { package = "dpc-tau-client", version = "=0.23.0", features = ["build"] }
```

Call the helper from `build.rs`:

```rust
fn main() {
    tau_client::build::emit().expect("capture package build identity");
}
```

Then add this call to `TauExtension::register`:

```rust,ignore
builder.build_identity(tau_client::package_build_identity!());
```

Declaration-aware extensions must also set the same `Some(...)` identity on
the Hello passed to `prepare_inspection`. Raw protocol implementations can
populate `BuildIdentity` directly. Use the extension package's release and
source revision, never SDK versions or inherited `TAU_BUILD` environment values.

By default, the build helper requires the package directory to be a Git checkout
root; it never discovers a parent repository. Monorepo packages must use
`emit_from_root("../..")` (or their own relative source root). The selected root
must own the checkout and track the package manifest. Cargo archive/vendor markers
force unknown metadata even when an enclosing repository tracks those files.
Archives and Git-free builds therefore report an unknown hash unless stamped.

Tracked worktree or staged changes add `-modified`; untracked files do not.
Cargo watches HEAD, index, refs, linked-worktree metadata, and tracked paths,
including files excluded from Cargo packaging. This refreshes identity after
commits and dirty-state changes without forcing unchanged builds to rerun.

If Cargo cannot reliably watch a tracked input, such as a missing file, gitlink,
or file symlink, the helper reports unknown rather than a fresh clean or dirty
state. Valid Git, index, and file watches remain active, including for an unborn
repository's first commit. After you restore an input manually, the identity may
remain unknown until another watched change. Tau's root `CLAUDE.md` file symlink
means plain source builds can be unknown; packaged stamps remain authoritative.

Owned source-directory symlinks use recursive Cargo watches, so changes beneath
their targets can cause extra rebuilds. Targets outside the checkout, overlapping
Git metadata, or containing the current build output are not watched.

The macro that takes a module argument, introduced in SDK 0.22.0, remains available
for independently validated caller metadata. Do not use plain
`built::write_built_file()` as a provenance check: it searches ancestor repositories
and does not establish the Git refresh watches needed here.

For Git-free Nix or release builds, packagers can fill the shared executable's
40-byte `__TAU_BUILD_GIT_REVISION_PLACEHOLDER____` and 17-byte
`__TAU_BUILD_DIRTY` slots after linking, using the package's own source SHA and
`clean____________` or `modified_________`. Preserve widths and verify replacement
counts. Stamped metadata takes precedence, so never stamp separately sourced
extension executables with Tau core's hash. Runtime identity reads only embedded
metadata, never environment variables, files, or subprocess output.

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
