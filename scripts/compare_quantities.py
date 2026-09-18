#!/usr/bin/env python3
"""Compare the volume we measure for each element with the one Revit measures.

The corpus holds AR files that ship the IFC Revit itself exported from the same
model, and that export carries `Qto_..BaseQuantities` with a `NetVolume` per
element, joined to ours on the element identifier in `IfcElement.Tag`. Revit's
number comes from its own kernel, so it is an answer this project did not
produce - the independent oracle for anything that changes the solid.

    openrvt export-ifc model.rvt --output ours.ifc --base-quantities
    scripts/compare_quantities.py revit-export.ifc ours.ifc

Products are grouped by the `IfcShapeRepresentation.RepresentationType` our
export wrote them as, because the swept-profile path and the boundary-shell
path fail at very different rates and a single percentage hides which one
moved.
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

# How far apart two volumes may be and still be called the same number. The
# tight one is what agreement looks like when both sides measure the same
# solid; the loose one is the threshold a disagreement is reported at, so that
# rounding in the reference export is never counted as an error.
AGREES = 0.001
DISAGREES = 0.05


def entities(text, name):
    """Every instance of one entity type, as (id, argument list)."""
    for match in re.finditer(rf"^#(\d+)={name}\((.*)$", text, re.M):
        yield int(match.group(1)), _layers.arguments(match.group(2))


def net_volumes(text):
    """Product entity id -> NetVolume, for every product a quantity set names."""
    of_quantity = {}
    for entity, fields in entities(text, "IFCQUANTITYVOLUME"):
        if len(fields) < 4 or "NetVolume" not in fields[0]:
            continue
        try:
            of_quantity[entity] = float(fields[3])
        except ValueError:
            continue
    of_set = {}
    for entity, fields in entities(text, "IFCELEMENTQUANTITY"):
        if len(fields) < 6:
            continue
        for reference in re.findall(r"#(\d+)", fields[5]):
            if int(reference) in of_quantity:
                of_set[entity] = of_quantity[int(reference)]
    of_product = {}
    for _, fields in entities(text, "IFCRELDEFINESBYPROPERTIES"):
        if len(fields) < 6:
            continue
        definition = re.findall(r"#(\d+)", fields[5])
        if not definition or int(definition[0]) not in of_set:
            continue
        for reference in re.findall(r"#(\d+)", fields[4]):
            of_product[int(reference)] = of_set[int(definition[0])]
    return of_product


def representation_types(text):
    """Product definition shape entity id -> its representation types."""
    of_shape = {}
    for entity, fields in entities(text, "IFCSHAPEREPRESENTATION"):
        if len(fields) >= 3:
            of_shape[entity] = fields[2].strip().strip("'")
    of_definition = {}
    for entity, fields in entities(text, "IFCPRODUCTDEFINITIONSHAPE"):
        if len(fields) < 3:
            continue
        kinds = {
            of_shape[int(reference)]
            for reference in re.findall(r"#(\d+)", fields[2])
            if int(reference) in of_shape
        }
        of_definition[entity] = "+".join(sorted(kinds)) or "-"
    return of_definition


def read(path):
    """Element id -> (representation type, NetVolume) for every product with one."""
    text = open(path, encoding="utf-8", errors="replace").read()
    volume_of = net_volumes(text)
    type_of = representation_types(text)
    out = {}
    for entity, entity_type, fields in (
        (int(match.group(1)), match.group(2), _layers.arguments(match.group(3)))
        for match in re.finditer(r"^#(\d+)=(IFC[A-Z0-9]+)\((.*)$", text, re.M)
    ):
        if entity not in volume_of or not _layers.is_tagged_product(entity_type):
            continue
        if len(fields) < 8 or not _layers.carries_global_id(fields):
            continue
        tag = fields[7].strip().strip("'")
        if not tag.isdigit():
            continue
        definition = re.findall(r"#(\d+)", fields[6])
        written_as = type_of.get(int(definition[0]), "-") if definition else "-"
        out[int(tag)] = (written_as, volume_of[entity])
    return out


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    theirs = read(sys.argv[1])
    ours = read(sys.argv[2])
    both = sorted(set(theirs) & set(ours))
    if not both:
        print("no element carries a volume on both sides")
        return 1
    print(f"volumes in the reference: {len(theirs)}")
    print(f"volumes in ours: {len(ours)}")
    print(f"elements carrying one on both sides: {len(both)}")

    def apart(tag):
        reference = theirs[tag][1]
        return abs(ours[tag][1] - reference) / max(abs(reference), 1e-9)

    agree = sum(1 for tag in both if apart(tag) <= AGREES)
    print(f"agree to a thousandth: {agree} ({100 * agree / len(both):.1f}%)")
    rows = collections.defaultdict(lambda: [0, 0])
    for tag in both:
        row = rows[ours[tag][0]]
        row[0] += 1
        row[1] += int(apart(tag) > DISAGREES)
    print(f"off by more than {DISAGREES:.0%}, by what we wrote it as:")
    for written_as, (count, off) in sorted(rows.items(), key=lambda row: -row[1][1]):
        print(f"  {written_as:<20} {off:>6} of {count:>6} ({100 * off / count:.1f}%)")
    worst = sorted(both, key=lambda tag: -apart(tag))[:10]
    print("the ten furthest apart:")
    for tag in worst:
        print(
            f"  {tag} written as {ours[tag][0]}: "
            f"{ours[tag][1]:.4f} against {theirs[tag][1]:.4f}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
