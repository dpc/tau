#!/usr/bin/env bash
set -euo pipefail

# Run the pinned analyzer with the same config as the real CI derivation.
cargo_crap=$1
config=$2
fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/tau-cargo-crap-exclusions.XXXXXX")
trap 'rm -rf "$fixture_root"' EXIT
cp "$config" "$fixture_root/.cargo-crap.toml"
mkdir -p "$fixture_root/member/src"
cat >"$fixture_root/Cargo.toml" <<'EOF'
[workspace]
members = ["member"]
resolver = "2"
EOF
cat >"$fixture_root/member/Cargo.toml" <<'EOF'
[package]
name = "crap-scope-fixture"
version = "0.0.0"
edition = "2021"
EOF
cat >"$fixture_root/member/src/lib.rs" <<'EOF'
pub fn production_simple() {}
#[cfg(test)]
mod tests;
#[cfg(test)]
mod inline_tests {
    fn inline_test_only() {}
}
EOF

# Twenty decisions score 462 at missing/pessimistic coverage: above the unchanged
# limit of 400, and above the absolute lane's --min 100.
write_complex_function() {
  local path=$1 name=$2
  mkdir -p "$(dirname "$path")"
  {
    printf 'pub fn %s(mut x: u32) -> u32 {\n' "$name"
    for i in {1..20}; do
      printf '    if x == %s { x += 1; }\n' "$i"
    done
    printf '    x\n}\n'
  } >"$path"
}

test_paths=(
  tests/integration.rs benches/bench.rs examples/example.rs
  src/tests.rs src/tests/mod.rs src/nested/tests/helper.rs
  src/parser_tests.rs src/nested/parser_tests.rs
)
for index in "${!test_paths[@]}"; do
  write_complex_function "$fixture_root/member/${test_paths[$index]}" "test_only_complex_$index"
done

cd "$fixture_root"
"$cargo_crap" --workspace --min 100 --format github --fail-above >passing.txt
if grep -q 'test_only_complex' passing.txt; then
  echo "Production CRAP gate included external test helpers" >&2
  exit 1
fi

assert_gate_failure() {
  local output=$1
  shift
  local status=0
  "$cargo_crap" --workspace --min 100 --format github --fail-above "$@" >"$output" || status=$?
  if [[ $status -ne 1 ]]; then
    echo "Expected CRAP threshold failure (exit 1), got $status" >&2
    cat "$output" >&2
    exit 1
  fi
}

# Negative control: these are real scanner inputs, not vacuously absent files.
assert_gate_failure unfiltered.txt --no-default-excludes
for index in "${!test_paths[@]}"; do
  grep -q "test_only_complex_$index" unfiltered.txt
done

# Ordinary production helpers and similar-looking paths must still be gated.
for path in src/helpers.rs src/contest.rs src/testing.rs src/nested/test_helpers.rs; do
  name=$(basename "$path" .rs)
  write_complex_function "$fixture_root/member/$path" "production_$name"
done
assert_gate_failure production.txt
for name in helpers contest testing test_helpers; do
  grep -q "production_$name" production.txt
done
if grep -q 'test_only_complex' production.txt; then
  echo "Production CRAP gate included external test helpers" >&2
  exit 1
fi
echo "cargo-crap production-only exclusion fixtures passed"
