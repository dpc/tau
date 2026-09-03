#!/usr/bin/env bash
set -euo pipefail

repository_root=$(git rev-parse --show-toplevel)
checker="$repository_root/.config/ast-grep/check-rust-struct-field-limit.py"
fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/tau-rust-struct-field-limit.XXXXXX")
trap 'rm -rf "$fixture_root"' EXIT

write_named_struct() {
  local name=$1
  local count=$2
  local type=${3:-u8}

  printf 'struct %s {\n' "$name"
  for ((field = 1; field <= count; field++)); do
    printf '    field_%02d: %s,\n' "$field" "$type"
  done
  printf '}\n'
}

write_tuple_struct() {
  local name=$1
  local count=$2

  printf 'struct %s(\n' "$name"
  for ((field = 1; field <= count; field++)); do
    printf '    #[allow(dead_code)] pub Option<(u8, u16)>,\n'
  done
  printf ');\n'
}

git -C "$fixture_root" init --quiet
pushd "$fixture_root" >/dev/null

mkdir -p src
{
  write_named_struct "TwentyNine" 29
  write_named_struct "Thirty" 30
} >src/boundaries.rs
git add -- src/boundaries.rs
"$checker"

{
  printf 'fn nested_declarations() {\n'
  write_named_struct "NestedThirtyOne" 31
  printf '}\n\n'
  printf 'struct GenericThirtyTwo<T>\nwhere\n    T: Clone,\n{\n'
  for ((field = 1; field <= 32; field++)); do
    printf '    field_%02d: Option<(T, T)>,\n' "$field"
  done
  printf '}\n'
} >src/multiple.rs

unusual_path=$'src/unusual space\nname.rs'
write_tuple_struct "TupleThirtyThree" 33 >"$unusual_path"
option_path=-dash.rs
write_named_struct "OptionLikeThirtyFour" 34 >"$option_path"
git add -- src/multiple.rs "$unusual_path" "$option_path"

set +e
output=$("$checker" 2>&1)
status=$?
set -e
if [[ $status -ne 1 ]]; then
  echo "Rust struct field limit did not reject oversized structs" >&2
  printf '%s\n' "$output" >&2
  exit 1
fi

nested_line=$(grep -n '^struct NestedThirtyOne' src/multiple.rs | cut -d: -f1)
generic_line=$(grep -n '^struct GenericThirtyTwo' src/multiple.rs | cut -d: -f1)
unusual_display=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$unusual_path")

for expected in \
  "-dash.rs:1: struct OptionLikeThirtyFour has 34 fields (maximum 30)." \
  "src/multiple.rs:$nested_line: struct NestedThirtyOne has 31 fields (maximum 30)." \
  "src/multiple.rs:$generic_line: struct GenericThirtyTwo has 32 fields (maximum 30)." \
  "$unusual_display:1: struct TupleThirtyThree has 33 fields (maximum 30)." \
  "Split it into smaller, logically coherent sub-state structs grouped by ownership, lifecycle, or invariants rather than suppressing this check."
do
  if [[ $output != *"$expected"* ]]; then
    echo "Rust struct field diagnostic omitted: $expected" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
done

if [[ $(grep -c 'maximum 30' <<<"$output") -ne 4 ]]; then
  echo "Rust struct field limit did not report every violation exactly once" >&2
  printf '%s\n' "$output" >&2
  exit 1
fi
popd >/dev/null
