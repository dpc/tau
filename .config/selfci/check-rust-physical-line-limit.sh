#!/usr/bin/env bash
set -euo pipefail

readonly maximum_lines=10000
repository_root=$(git rev-parse --show-toplevel)
tracked_paths=$(mktemp)
trap 'rm -f "$tracked_paths"' EXIT
violations=0

if ! git -C "$repository_root" ls-files -z -- '*.rs' >"$tracked_paths"; then
  echo "Failed to enumerate tracked Rust files for the physical line limit." >&2
  exit 1
fi

while IFS= read -r -d '' path; do
  file="$repository_root/$path"
  line_count=$(wc -l <"$file")
  if [[ -s "$file" ]] && [[ $(tail -c 1 -- "$file" | od -An -t x1) != *0a* ]]; then
    ((line_count += 1))
  fi
  if ((line_count > maximum_lines)); then
    printf \
      'Rust file exceeds %d physical lines: %q has %d lines. Split it into coherent modules rather than suppressing this check.\n' \
      "$maximum_lines" "$path" "$line_count" >&2
    violations=1
  fi
done <"$tracked_paths"

exit "$violations"
