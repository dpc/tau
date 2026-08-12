#!/usr/bin/env bash
set -euo pipefail

rule=.config/ast-grep/rules/debug-assert-expression-must-not-mutate.yml
count_rule=.config/ast-grep/test-rules/debug-assert-direct-invocation.yml
fixtures=.config/ast-grep/debug-assert-fixtures

expect_scan_pass() {
  ast-grep scan --error --rule "$rule" "$1" >/dev/null
}

expect_scan_fail() {
  local output
  set +e
  output=$(ast-grep scan --error --rule "$rule" "$1" 2>&1)
  local status=$?
  set -e
  if [[ $status -ne 1 ]] || [[ $output != *"$2"* ]]; then
    echo "expected $1 to fail with $2" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
}

expect_scan_pass "$fixtures/acknowledged.rs"
expect_scan_fail "$fixtures/missing.rs" "debug-assert-expression-must-not-mutate"
expect_scan_fail "$fixtures/blank-line.rs" "unused-suppression"
expect_scan_fail "$fixtures/intervening-node.rs" "unused-suppression"
expect_scan_fail "$fixtures/suppress-all.rs" "no-suppress-all"
expect_scan_fail "$fixtures/stale.rs" "unused-suppression"

reject_counting_rule_suppressions() {
  if rg -q --glob '*.rs' \
    'ast-grep-ignore[^:]*:.*debug-assert-direct-invocation' "$@"
  then
    echo "production source must not suppress the debug_assert counting rule" >&2
    return 1
  fi
}

if ! reject_counting_rule_suppressions crates; then
  exit 1
fi

# ast-grep 0.42.1 accepts rule-specific directive variants at line one followed
# by whitespace-only line two as whole-file suppression. Prove that behavior,
# then reject every accepted spelling under Tau's per-invocation policy.
whole_file_fixtures=(
  "$fixtures/whole-file.rs"
  "$fixtures/whole-file-prose.rs"
  "$fixtures/whole-file-rule-list.rs"
)
for fixture in "${whole_file_fixtures[@]}"; do
  expect_scan_pass "$fixture"
done

reject_whole_file_suppressions() {
  local file
  local first_line
  local second_line
  for file; do
    first_line=$(sed -n '1p' "$file")
    second_line=$(sed -n '2p' "$file")
    if [[ $first_line =~ ast-grep-ignore[^:]*:.*debug-assert-expression-must-not-mutate ]] &&
      [[ -z ${second_line//[[:space:]]/} ]]
    then
      echo "whole-file debug_assert suppression is forbidden: $file" >&2
      return 1
    fi
  done
}

mapfile -d '' rust_files < <(git ls-files -z -- 'crates/**/*.rs')
if ! reject_whole_file_suppressions "${rust_files[@]}"; then
  exit 1
fi

temporary_fixtures=$(mktemp -d)
trap 'rm -rf "$temporary_fixtures"' EXIT
for variant in indented whitespace-only cr-whitespace-only; do
  fixture="$temporary_fixtures/$variant.rs"
  case $variant in
    indented)
      printf '    // ast-grep-ignore: debug-assert-expression-must-not-mutate\n\nfn check(condition: bool) { debug_assert!(condition); }\n' >"$fixture"
      ;;
    whitespace-only)
      printf '// ast-grep-ignore: debug-assert-expression-must-not-mutate\n \t\nfn check(condition: bool) { debug_assert!(condition); }\n' >"$fixture"
      ;;
    cr-whitespace-only)
      printf '// ast-grep-ignore: debug-assert-expression-must-not-mutate\n\r\nfn check(condition: bool) { debug_assert!(condition); }\n' >"$fixture"
      ;;
  esac
  expect_scan_pass "$fixture"
  if reject_whole_file_suppressions "$fixture" 2>/dev/null; then
    echo "expected $variant whole-file suppression to be forbidden" >&2
    exit 1
  fi
done

expect_multiple_direct_invocations_fail() {
  local output
  set +e
  output=$(
    ast-grep scan --json=stream --rule "$count_rule" "$1" |
      python3 -c '
import json
import sys
from collections import Counter

locations = Counter(
    (match["file"], match["range"]["start"]["line"])
    for line in sys.stdin
    if (match := json.loads(line))
)
duplicates = [location for location, count in locations.items() if 1 < count]
if duplicates:
    for file, line in duplicates:
        print(f"multiple direct debug_assert! invocations on one line: {file}:{line + 1}")
    raise SystemExit(1)
'
  )
  local status=$?
  set -e
  if [[ $status -ne 1 ]] || [[ $output != *"multiple direct debug_assert! invocations"* ]]; then
    echo "expected $1 to reject multiple direct debug_assert! invocations" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
}

# Native suppression has line, not AST-node, scope. One directive would suppress
# both direct macro nodes on either source line, so require one invocation per
# line before native suppression associates the directive.
expect_multiple_direct_invocations_fail "$fixtures/multiple-on-one-line.rs"

# A source can name both rule IDs in one directive. The production preflight
# rejects this escape hatch, so prove that native ast-grep suppresses the
# counting rule when a combined list includes it.
combined_rule_fixture="$temporary_fixtures/combined-rule-list.rs"
printf '%s\n' \
  'fn tuple(first: bool, second: bool) {' \
  '    // ast-grep-ignore : debug-assert-expression-must-not-mutate, debug-assert-direct-invocation' \
  '    let _ = (debug_assert!(first), debug_assert!(second));' \
  '}' >"$combined_rule_fixture"
if [[ -n $(ast-grep scan --json=stream --rule "$count_rule" "$combined_rule_fixture") ]]; then
  echo "combined rule list did not suppress the counting rule" >&2
  exit 1
fi
if reject_counting_rule_suppressions "$combined_rule_fixture" 2>/dev/null; then
  echo "combined rule list did not fail the counting-rule preflight" >&2
  exit 1
fi

if ! ast-grep scan --json=stream --rule "$count_rule" crates |
  python3 -c '
import json
import sys
from collections import Counter

locations = Counter(
    (match["file"], match["range"]["start"]["line"])
    for line in sys.stdin
    if (match := json.loads(line))
)
duplicates = [location for location, count in locations.items() if 1 < count]
if duplicates:
    for file, line in duplicates:
        print(f"multiple direct debug_assert! invocations on one line: {file}:{line + 1}")
    raise SystemExit(1)
'
then
  exit 1
fi
