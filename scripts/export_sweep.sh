#!/usr/bin/env bash
# Export every corpus file to IFC and tally what came out, one row per file.
# Any change to the geometry or typing paths can buy one class by selling
# another, so this table is what such a change is judged on: take it before and
# after, then diff.
#
#   scripts/export_sweep.sh after.tsv
#
# Each IFC is counted and then deleted: the corpus is 50 files of a few hundred
# megabytes each and only the counts are wanted.
set -u
rivet=${RIVET:-./target/release/rivet}
out=${1:?usage: export_sweep.sh <output.tsv>}
work=${WORK:-data/test/project/ifc-out/sweep}
mkdir -p "$work"
printf 'file\tstatus\tstoreys\telements\twith_geometry\tshapes\tbreps\tspaces\tseconds\n' > "$out"
for f in data/test/project/extracted/*/*.rvt; do
  name=$(basename "$f" .rvt)
  ifc="$work/$name.ifc"
  started=$SECONDS
  if log=$("$rivet" export-ifc "$f" --include-unplaced -o "$ifc" 2>&1); then
    status=ok
  else
    status=failed
  fi
  storeys=$(printf '%s\n' "$log" | sed -n 's/^Building storeys: //p')
  elements=$(printf '%s\n' "$log" | sed -n 's/^Elements: //p')
  geometry=$(printf '%s\n' "$log" | sed -n 's/^Elements with verified geometry: //p')
  if [ -f "$ifc" ]; then
    counts=$(grep -o '=IFC[A-Z0-9]*' "$ifc" | sort | uniq -c)
    count() { printf '%s\n' "$counts" | awk -v e="=$1" '$2==e {print $1; found=1} END {if (!found) print 0}'; }
    shapes=$(count IFCSHAPEREPRESENTATION)
    breps=$(count IFCADVANCEDBREP)
    spaces=$(count IFCSPACE)
    rm -f "$ifc"
  else
    shapes=0; breps=0; spaces=0
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$name" "$status" "${storeys:-0}" "${elements:-0}" "${geometry:-0}" \
    "$shapes" "$breps" "$spaces" "$((SECONDS - started))" >> "$out"
done
rmdir "$work" 2>/dev/null || true
echo EXPORT_SWEEP_DONE
