#!/usr/bin/env bash
set -euo pipefail

workspace=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

registry_consumer=false
if [[ "${1:-}" == "--registry" ]]; then
  registry_consumer=true
  shift
fi
if (($# != 0)); then
  echo "usage: $0 [--registry]" >&2
  exit 2
fi

packages=(
  dpc-tau-actions
  dpc-tau-blocking-notify-channel
  dpc-tau-proto
  dpc-tau-client
)
msrv_nixpkgs=github:NixOS/nixpkgs/b6018f87da91d19d0ab4cf979885689b469cdd41

export CARGO_TARGET_DIR="$tmp/target"
export CARGO_TERM_COLOR=never
export CARGO_TERM_PROGRESS_WHEN=never

cd "$workspace"
metadata=$(cargo metadata --format-version 1 --no-deps)
declare -A versions
for package in "${packages[@]}"; do
  versions["$package"]=$(
    python3 -c '
import json
import sys

package = sys.argv[1]
metadata = json.load(sys.stdin)
matches = [item["version"] for item in metadata["packages"] if item["name"] == package]
if len(matches) != 1:
    raise SystemExit(f"expected one metadata entry for {package}, found {len(matches)}")
print(matches[0])
' "$package" <<<"$metadata"
  )
done

cargo package \
  --locked \
  --allow-dirty \
  --no-verify \
  "${packages[@]/#/-p}"

mkdir "$tmp/packages"
for package in "${packages[@]}"; do
  version=${versions["$package"]}
  archive="$CARGO_TARGET_DIR/package/${package}-${version}.crate"
  test -f "$archive"
  tar -xzf "$archive" -C "$tmp/packages"

  manifest="$tmp/packages/${package}-${version}/Cargo.toml"
  python3 - "$package" "$manifest" <<'PY'
import pathlib
import sys
import tomllib

package = sys.argv[1]
manifest = tomllib.loads(pathlib.Path(sys.argv[2]).read_text())

def check_dependencies(table):
    for dependency, value in table.items():
        if isinstance(value, dict) and "path" in value:
            raise SystemExit(
                f"{package} package retains path dependency {dependency}"
            )

if manifest.get("workspace") is not None:
    raise SystemExit(f"{package} package retains workspace inheritance")

for key in ("dependencies", "dev-dependencies", "build-dependencies"):
    check_dependencies(manifest.get(key, {}))
for target in manifest.get("target", {}).values():
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        check_dependencies(target.get(key, {}))
PY
  test -f "$tmp/packages/${package}-${version}/README.md"
done

mkdir -p "$tmp/consumer/src"
cat >"$tmp/consumer/Cargo.toml" <<EOF
[package]
name = "tau-sdk-package-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.91"
publish = false

[dependencies]
tau-actions = { package = "dpc-tau-actions", version = "=${versions[dpc-tau-actions]}" }
tau-blocking-notify-channel = { package = "dpc-tau-blocking-notify-channel", version = "=${versions[dpc-tau-blocking-notify-channel]}" }
tau-client = { package = "dpc-tau-client", version = "=${versions[dpc-tau-client]}" }
tau-proto = { package = "dpc-tau-proto", version = "=${versions[dpc-tau-proto]}" }
EOF

if [[ "$registry_consumer" == false ]]; then
  cat >>"$tmp/consumer/Cargo.toml" <<EOF

[patch.crates-io]
dpc-tau-actions = { path = "$tmp/packages/dpc-tau-actions-${versions[dpc-tau-actions]}" }
dpc-tau-blocking-notify-channel = { path = "$tmp/packages/dpc-tau-blocking-notify-channel-${versions[dpc-tau-blocking-notify-channel]}" }
dpc-tau-client = { path = "$tmp/packages/dpc-tau-client-${versions[dpc-tau-client]}" }
dpc-tau-proto = { path = "$tmp/packages/dpc-tau-proto-${versions[dpc-tau-proto]}" }
EOF
fi

cat >"$tmp/consumer/src/lib.rs" <<'EOF'
/// Verifies that the packaged protocol and client surfaces resolve together.
#[test]
fn packaged_sdk_round_trips_the_advertised_protocol_version() {
    let encoded = tau_proto::encode_message_to_vec(&tau_proto::PROTOCOL_VERSION)
        .expect("protocol version should encode");
    let decoded: tau_proto::ProtocolVersion =
        tau_proto::decode_message_from_slice(&encoded).expect("protocol version should decode");

    assert_eq!(decoded, tau_proto::ProtocolVersion::new(1, 1));
    let _logging_initializer: fn(&'static str) = tau_client::init_logging_for;
}
EOF

nix shell \
  "$msrv_nixpkgs#cargo" \
  "$msrv_nixpkgs#rustc" \
  --command bash -euo pipefail -c '
    [[ "$(rustc --version)" == "rustc 1.91."* ]]
    [[ "$(cargo --version)" == "cargo 1.91."* ]]
    cd "$1"
    cargo test --manifest-path Cargo.toml -- --color never
  ' bash "$tmp/consumer"
