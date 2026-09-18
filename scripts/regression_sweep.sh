#!/usr/bin/env bash
# Walk the top classes of every corpus file and report how many records each
# class explains exactly. Any change to the record walk can buy one class by
# selling another, so this table is what a change is judged on: take it before
# and after, then diff.
#
#   scripts/regression_sweep.sh before.tsv
set -u
openrvt=${OPENRVT:-./target/release/openrvt}
out=${1:?usage: regression_sweep.sh <output.tsv>}
: > "$out"
for f in data/test/*.rvt; do
  file=$(basename "$f" | cut -d_ -f1)
  classes=$("$openrvt" inspect "$f" 2>/dev/null \
    | sed -n '/^Element classes/,/^$/p' \
    | awk 'NF==3 && $1 ~ /^[0-9]+$/ {print $2}')
  for c in $classes; do
    printf '%s\t%s\t%s\n' "$file" "$c" \
      "$("$openrvt" serial-probe "$f" --record --class "$c" 2>/dev/null \
         | grep -E 'Records walked|^  explained exactly' | tr '\n' ' ' | tr -s ' ')" >> "$out"
  done
done
