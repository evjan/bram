#!/usr/bin/env bash
# Compare two normalized trace snapshots from scripts/record-trace.sh.
#
# Usage (from repo root):
#   scripts/diff-trace.sh <before-snapshot> <after-snapshot>
#
# Output: a plain `diff -u` between the two normalized files, followed
# by a small structural reporter that flags structural differences
# (event kinds appearing/disappearing, `reason=` values changing) more
# loudly than additive ones (a new optional field on an existing event).
#
# Exit code:
#   0 — no structural difference detected.
#   1 — structural difference detected. Suitable for a precommit hook.

set -euo pipefail

if [ $# -lt 2 ]; then
  echo "usage: $0 <before-snapshot> <after-snapshot>" >&2
  exit 64
fi
before="$1"
after="$2"

if [ ! -f "$before" ]; then
  echo "$0: not a file: $before" >&2
  exit 1
fi
if [ ! -f "$after" ]; then
  echo "$0: not a file: $after" >&2
  exit 1
fi

# Plain diff first. -u for readability; do not exit on non-zero (we
# expect differences and want to keep reporting).
echo "============================================================"
echo "diff -u $before $after"
echo "============================================================"
diff -u "$before" "$after" || true

echo ""
echo "============================================================"
echo "Structural reporter"
echo "============================================================"

# Count distinct event kinds on each side. An event kind appearing or
# disappearing entirely is the strongest structural signal.
before_kinds=$(grep -oE '^\[[^]]+\]' "$before" | sort -u)
after_kinds=$(grep -oE '^\[[^]]+\]' "$after" | sort -u)

added_kinds=$(comm -13 <(echo "$before_kinds") <(echo "$after_kinds") || true)
removed_kinds=$(comm -23 <(echo "$before_kinds") <(echo "$after_kinds") || true)

structural=0
if [ -n "$added_kinds" ]; then
  echo "Event kinds ONLY in after:"
  echo "$added_kinds" | sed 's/^/  + /'
  structural=1
fi
if [ -n "$removed_kinds" ]; then
  echo "Event kinds ONLY in before:"
  echo "$removed_kinds" | sed 's/^/  - /'
  structural=1
fi

# Count occurrences per kind on each side, compare. macOS ships bash
# 3.2 which lacks associative arrays, so use sort | uniq -c and a
# small awk join instead.
before_count_file=$(mktemp)
after_count_file=$(mktemp)
trap 'rm -f "$before_count_file" "$after_count_file"' EXIT

grep -oE '^\[[^]]+\]' "$before" | sort | uniq -c | sed 's/^ *//' > "$before_count_file"
grep -oE '^\[[^]]+\]' "$after"  | sort | uniq -c | sed 's/^ *//' > "$after_count_file"

# Join on the kind column and print rows where counts differ. Each
# count-file line is "N [kind]"; the kind is the rest of the line
# after the count and one space.
count_diffs=$(awk '
  function key_of(line) {
    sub(/^[0-9]+ /, "", line)
    return line
  }
  FNR == NR { b[key_of($0)] = $1; next }
  { a[key_of($0)] = $1 }
  END {
    for (k in b) {
      av = (k in a) ? a[k] : 0
      if (av != b[k]) printf "  ~ %s  before=%d  after=%d\n", k, b[k], av
    }
    for (k in a) {
      if (!(k in b)) printf "  ~ %s  before=0  after=%d\n", k, a[k]
    }
  }
' "$before_count_file" "$after_count_file")

if [ -n "$count_diffs" ]; then
  echo "Event kinds with changed occurrence counts:"
  printf "%s\n" "$count_diffs"
  structural=1
fi

# `reason=` values changing on otherwise-comparable lines is a useful
# secondary signal.
before_reasons=$(grep -oE 'reason=[A-Za-z_-]+' "$before" | sort -u)
after_reasons=$(grep -oE 'reason=[A-Za-z_-]+' "$after" | sort -u)
added_reasons=$(comm -13 <(echo "$before_reasons") <(echo "$after_reasons") || true)
removed_reasons=$(comm -23 <(echo "$before_reasons") <(echo "$after_reasons") || true)
if [ -n "$added_reasons" ]; then
  echo "reason= values ONLY in after:"
  echo "$added_reasons" | sed 's/^/  + /'
  structural=1
fi
if [ -n "$removed_reasons" ]; then
  echo "reason= values ONLY in before:"
  echo "$removed_reasons" | sed 's/^/  - /'
  structural=1
fi

if [ "$structural" -eq 0 ]; then
  echo "No structural differences detected."
fi

exit "$structural"
