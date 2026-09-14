# Native Linux distribution

The native builder produces **Tau plus all seven owned extension projects**
(nine executables, including `tau-telegram-gateway`). Use `tau-full` for the
complete distribution, or install individual packages. Installation does not
enable extensions, create accounts, install services, supply credentials, or
change user configuration. Existing Nix packages are unchanged.

`distribution.toml` is the required inventory shared by the builder, assembler,
qualification probes, staging, and publisher. Missing products fail the build;
there is no core-only fallback. See [native-builds.md](native-builds.md) for
commands, pinned inputs, provenance, and workflow trust boundaries.

## Packages and archives

Each architecture gets 30 package/archive assets:

* `tau-<tau-version>-<arch>.{deb,rpm,tar.gz}`;
* eight external binary packages,
  `<binary>-<upstream-version>-tau-<tau-version>-<arch>.{deb,rpm,tar.gz}`;
* `tau-full-<tau-version>-<arch>.{deb,rpm,tar.gz}`.

The DEB/RPM `tau-full` package contains **only exact-version dependencies** on
the nine individual packages. It owns no files, so installing it alongside
individual packages cannot create overlapping file ownership. Place all matching
architecture DEBs/RPMs together and install them with the distro package manager;
the metapackage is not useful without the individual packages being available.

An individual package installs `/usr/bin/<binary>`, the upstream project
`LICENSE`, full generated `THIRD_PARTY_NOTICES.html`, and its source/ELF manifest
under `/usr/share/{licenses,doc}/<binary>`. Archives contain the same payload in
`bin/` and `share/` below one named prefix. The full archive combines those
payloads; it is not an archive of package-manager files.

External Cargo versions stay unchanged. For example, upstream `0.1.0` from the
Tau `0.1.1` release set becomes native package version `0.1.0-1.tau0.1.1`.
A later release set gets a different revision even if the upstream version is
still `0.1.0`. DEB/RPM numeric ordering covers `0.1.9` → `0.1.10`; prereleases use
tilde ordering. Source URL, full Git SHA, locked NAR hash, source-file hashes,
upstream version/license, ELF dependencies, and notice digest remain explicit.

Manual candidates instead use `0.0.0~test.<source-sha>-1`, below stable `0.0.0`.
They are not promoted into releases. Tags must exactly match the application
version in `crates/tau/Cargo.toml`, not the independently versioned SDK or the
default version of unchanged workspace crates.

## Qualification boundaries

Every successful complete build must:

1. Build all exact locked sources on a matching native architecture.
2. Generate full notices per source/architecture with pinned cargo-about, including
   build dependencies and excluding dev-only dependencies. Unknown license
   requirements fail; no warning-only or metadata-only license substitute exists.
3. Inspect every ELF without `ldd` or executing it during assembly. The filter
   requires standard GNU loaders, no Nix-store linkage/RPATH, only reviewed
   libc/libm/libgcc/pthread/dl/loader dependencies, and GLIBC symbols ≤2.34.
4. Extract actual individual and full archives, compare their binaries, check
   every executable's `--help`, and verify core version/revision/date.
5. Use extracted Tau to admit each packaged stdio extension's protocol-7 Hello
   with an intentionally invalid credential-free configuration. The private
   Telegram gateway is checked for executable startup, not stdio Hello.
6. Install with dependency resolution, check exact ownership/no lifecycle
   scripts, execute installed `--help`, remove packages, preserve a user-state
   sentinel, and check release revision ordering in disposable Debian 12 and
   Fedora 43 userspaces.

The probes are **not Ready/service-integration qualification**. They do not
establish default restricted supervision, shell/PTY/CA behavior, minimum kernel
support, every RPM distro, or broad portability. Containers share a host kernel.
No complete hosted result is implied merely by implementing these gates.
The historical core-only Actions run 34556681071 (September 11, 2026) does not
qualify the new complete distribution.

## Tests and low-level inspection

```console
python3 packaging/test_native.py
python3 packaging/test_build.py
python3 packaging/test_complete.py
python3 packaging/test_publish.py
python3 packaging/native.py inventory --source-sha FULL_40_CHARACTER_COMMIT_SHA
python3 packaging/native.py audit-elf --binary /path/to/tau --arch amd64
```

The four unit suites run in SelfCI. `test_tools.py` additionally requires nFPM
2.46.3, dpkg, and rpm; its inert core/full format fixtures are not runtime evidence.
`native.py package-core` remains a low-level supplied-binary diagnostic tool,
not the complete release builder. Its manifest explicitly disclaims an attested
binary/source relationship and it does not generate third-party notices.

All outputs use new directories; failures do not leave a final candidate.
Checksums provide integrity, not authentication. Ownership/timestamps are
normalized, but byte-for-byte archive/build reproducibility is not claimed.
See `docs/extensions.md` for explicit extension configuration.
