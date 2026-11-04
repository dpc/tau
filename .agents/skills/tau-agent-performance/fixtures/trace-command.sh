#!/bin/sh
# Fake `tau agent trace ID --agents-dir ROOT --format FORMAT` for scan tests.
# Keep this executable immutable while tests run; only ROOT/trace.jsonl varies.
case "$3" in
  old) exit 1 ;;
  slow) exec sleep 10 ;;
  bad) echo '{}' ;;
  ok) exec cat "$5/trace.jsonl" ;;
  *) exit 2 ;;
esac
