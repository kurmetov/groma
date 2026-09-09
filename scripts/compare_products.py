#!/usr/bin/env python3
"""Compare the IFC entity we give each element with the one Revit gives it.

Revit's own export of the corpus AR files carries the Revit element id in
`IfcElement.Tag`, so our export and Revit's can be joined on it and every
disagreement counted. That is the measurement the typing rules are justified
by: a `(class, category)` pair earns a mapping row when the join shows it maps
to one entity with no spread.

    rivet export-ifc model.rvt --output ours.ifc
    scripts/compare_products.py revit-export.ifc ours.ifc

Revit writes the void it cuts with the same element id as the product that cuts
it, so a tag naming both an `IfcOpeningElement` and a product resolves to the
product.
"""

import collections
import importlib.util
import pathlib
import re
import sys

_spec = importlib.util.spec_from_file_location(
    "compare_wall_layers", pathlib.Path(__file__).with_name("compare_wall_layers.py")
)
_layers = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_layers)


def tagged_products(path):
    """Element id -> IFC entity, for every product carrying a `Tag`."""
    text = open(path, encoding="utf-8", errors="replace").read()
    out = {}
    for match in re.finditer(r"^#(\d+)=(IFC[A-Z0-9]+)\((.*)$", text, re.M):
        entity = match.group(2)
        if not _layers.is_tagged_product(entity):
            continue
        fields = _layers.arguments(match.group(3))
        if len(fields) < 8 or not _layers.carries_global_id(fields):
            continue
        tag = fields[7].strip().strip("'")
        if not tag.isdigit():
            continue
        tag = int(tag)
        if tag in out and entity == "IFCOPENINGELEMENT":
            continue
        out[tag] = entity
    return out


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    theirs = tagged_products(sys.argv[1])
    ours = tagged_products(sys.argv[2])
    matched = set(theirs) & set(ours)
    agree = sum(1 for tag in matched if theirs[tag] == ours[tag])
    print(f"products in the reference: {len(theirs)}")
    print(f"products we export: {len(ours)}")
    print(f"matched by element id: {len(matched)}")
    share = 100 * agree / max(len(matched), 1)
    print(f"same IFC entity: {agree} ({share:.1f}%)")
    disagreed = collections.Counter(
        (ours[tag], theirs[tag]) for tag in matched if ours[tag] != theirs[tag]
    )
    if disagreed:
        print("\ndisagreements, ours -> Revit:")
        for (mine, revit), count in disagreed.most_common(12):
            print(f"  {count:6d}  {mine} -> {revit}")
    missing = collections.Counter(theirs[tag] for tag in set(theirs) - set(ours))
    if missing:
        print(f"\nin the reference, not exported by us: {sum(missing.values())}")
        for entity, count in missing.most_common(8):
            print(f"  {count:6d}  {entity}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
