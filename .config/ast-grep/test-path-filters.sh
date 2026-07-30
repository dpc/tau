#!/usr/bin/env bash
set -euo pipefail

rule=.config/ast-grep/rules/no-tests-outside-test-files.yml
fixtures=.config/ast-grep/path-fixtures
rejected=(
  "$fixtures/src/inline.rs"
  "$fixtures/src/parser_test.rs"
  "$fixtures/src/mytests.rs"
  "$fixtures/src/test/integration.rs"
)
allowed=(
  "$fixtures/src/doctest.rs"
  "$fixtures/src/tests.rs"
  "$fixtures/src/parser_tests.rs"
  "$fixtures/src/tests/nested.rs"
  "$fixtures/tests/integration.rs"
)

for fixture in "${rejected[@]}" "${allowed[@]}"; do
  test -f "$fixture" || { echo "missing ast-grep path fixture: $fixture" >&2; exit 1; }
done

for fixture in "${rejected[@]}"; do
  set +e
  output=$(ast-grep scan --error --rule "$rule" "$fixture" 2>&1)
  status=$?
  set -e
  if [[ $status -ne 1 ]] || [[ $output != *"no-tests-outside-test-files"* ]]; then
    echo "no-tests-outside-test-files did not reject $fixture with its diagnostic" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
done

ast-grep scan --error --rule "$rule" "${allowed[@]}" >/dev/null
