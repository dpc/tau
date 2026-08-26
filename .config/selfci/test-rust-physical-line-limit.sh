#!/usr/bin/env bash
set -euo pipefail

repository_root=$(git rev-parse --show-toplevel)
checker="$repository_root/.config/selfci/check-rust-physical-line-limit.sh"
fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/tau-rust-physical-line-limit.XXXXXX")
trap 'rm -rf "$fixture_root"' EXIT

write_rust_file() {
  local path=$1
  local line_count=$2

  mkdir -p "$(dirname "$path")"
  awk -v line_count="$line_count" 'BEGIN {
    for (line = 0; line < line_count; line++) {
      print "// physical Rust line";
    }
  }' >"$path"
}

assert_success() {
  if ! "$checker"; then
    echo "Rust physical line limit unexpectedly rejected a passing fixture" >&2
    exit 1
  fi
}

git -C "$fixture_root" init --quiet

pushd "$fixture_root" >/dev/null
write_rust_file "src/below-limit.rs" 9999
write_rust_file "src/exact-limit.rs" 10000
write_rust_file "src/exact-limit-unterminated.rs" 9999
printf '// unterminated physical Rust line' >>"src/exact-limit-unterminated.rs"
git add -- \
  "src/below-limit.rs" \
  "src/exact-limit.rs" \
  "src/exact-limit-unterminated.rs"
assert_success

write_rust_file "src/untracked-over-limit.rs" 10001
assert_success

git_path="$fixture_root/bin"
mkdir "$git_path"
real_git=$(command -v git)
cat >"$git_path/git" <<EOF
#!/usr/bin/env bash
if [[ \$1 == -C && \$3 == ls-files ]]; then
  echo "injected tracked-file enumeration failure" >&2
  exit 42
fi
exec "$real_git" "\$@"
EOF
chmod +x "$git_path/git"

set +e
output=$(PATH="$git_path:$PATH" "$checker" 2>&1)
status=$?
set -e
if [[ $status -eq 0 ]] || [[ $output != *"Failed to enumerate tracked Rust files"* ]]; then
  echo "Rust physical line limit did not reject failed tracked-file enumeration" >&2
  printf '%s\n' "$output" >&2
  exit 1
fi

write_rust_file "src/over-limit.rs" 10001
write_rust_file "src/unterminated-over-limit.rs" 10000
printf '// unterminated physical Rust line' >>"src/unterminated-over-limit.rs"
unusual_path=$'src/unusual space\nname.rs'
write_rust_file "$unusual_path" 10001
git add -- \
  "src/over-limit.rs" \
  "src/unterminated-over-limit.rs" \
  "$unusual_path"

set +e
output=$("$checker" 2>&1)
status=$?
set -e
if [[ $status -ne 1 ]]; then
  echo "Rust physical line limit did not fail for tracked oversized files" >&2
  printf '%s\n' "$output" >&2
  exit 1
fi

printf -v unusual_path_display '%q' "$unusual_path"
for expected in \
  "src/over-limit.rs" \
  "src/unterminated-over-limit.rs" \
  "$unusual_path_display" \
  "10001 lines" \
  "Split it into coherent modules rather than suppressing this check."
do
  if [[ $output != *"$expected"* ]]; then
    echo "Rust physical line limit diagnostic omitted: $expected" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
done
popd >/dev/null
