#!/usr/bin/env python3
"""Compare the type each element is an instance of with the one Revit gives it.

Revit's own export of the corpus AR files carries the Revit element id in
`IfcElement.Tag`, and the *type* element's own id in `IfcTypeObject.Tag`, so
both exports can be joined on the element and the two type ids compared. That
is the only honest measurement of type identity, because the number of
`IfcDoorType` entities in a file is not the number of Revit types in it:
Revit's exporter writes one type entity per distinct *geometry*, each with its
own `RepresentationMaps`, so one Revit type reaches the file as many entities
all carrying the same `Tag`. Counting entities reads as a collapse on our side
that the ids say is not there.

    groma export-ifc model.rvt --output ours.ifc
    scripts/compare_types.py revit-export.ifc ours.ifc

A reference type id this export names nowhere is reported separately, because
that is what a mismatched pair looks like: a reference exported from a *later
save* of the project names types created after the `.rvt` in hand was written,
and `groma element <id>` finds no record for them at all - their ids sit above
the file's own largest element id. Check the pairing before reading anything
into a disagreement: the reference's `FILE_NAME` and its `NumberOfSaves`
against the save the `.rvt` is. The line is only a flag, not a verdict - an id
that does name a real element the export happens not to use as a type lands in
it too, and separating those needs `groma element`.

The residue that survives a correctly paired run on AR S1 is 37 curtain-wall
panels, and it is Revit's own convention rather than a disagreement about the
file: for a panel, Revit tags the type with the element id of one of the
*panels* it types, and every one of those ids is a `FamilyInstance` in the
`.rvt` while ours is the `FamilySymbol`/`SysPanelFamSym` the record names.
"""

import argparse
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

INSTANCE = re.compile(r"^#(\d+)\s*=\s*(IFC[A-Z0-9]+)\((.*)$", re.M)

# `IfcTypeObject.Tag` and `IfcElement.Tag` are both the eighth attribute, and
# `Name` is the third of either.
TAG_AT = 7
NAME_AT = 2

# Spatial structure is not `IfcElement` and carries no `Tag`: an `IfcSpace`'s
# eighth attribute is its `LongName`, which would join as a room name. Revit
# writes an `IfcSpaceType` per space rather than per Revit type, so nothing is
# lost by leaving the spaces out of the element join.
SPATIAL = (
    "IFCSPACE",
    "IFCBUILDINGSTOREY",
    "IFCBUILDING",
    "IFCSITE",
    "IFCSPATIALZONE",
    "IFCZONE",
)


def is_type(entity):
    """Whether the entity is an `IfcTypeObject`, by name alone.

    `IfcDoorStyle` and `IfcWindowStyle` are IFC2x3's spelling of a type and
    Revit still writes them, so both endings count. A caller must also check
    [`_layers.carries_global_id`] - `IfcSurfaceStyle` and the rest of the
    presentation styles end the same way and carry no `GlobalId`.
    """
    return entity.endswith(("TYPE", "STYLE"))


def read(path):
    """Read one export into (products, types, product -> type entity number).

    `products` and `types` are keyed by entity number; each holds the entity's
    class, its `Name` and its `Tag`. An `IfcOpeningElement` is left out: Revit
    gives the void it cuts the same element id as the product cutting it, so
    keeping both would make the join ambiguous.
    """
    text = pathlib.Path(path).read_text(encoding="utf-8", errors="replace")
    products, types, of_type = {}, {}, {}
    for match in INSTANCE.finditer(text):
        number, entity = int(match.group(1)), match.group(2)
        if entity == "IFCRELDEFINESBYTYPE":
            fields = _layers.arguments(match.group(3))
            if len(fields) < 6:
                continue
            relating = int(fields[5].strip().lstrip("#"))
            for related in re.findall(r"#(\d+)", fields[4]):
                of_type[int(related)] = relating
            continue
        if entity == "IFCOPENINGELEMENT" or entity in SPATIAL:
            continue
        typed = is_type(entity)
        if not typed and not _layers.is_tagged_product(entity):
            continue
        fields = _layers.arguments(match.group(3))
        if len(fields) <= TAG_AT or not _layers.carries_global_id(fields):
            continue
        row = {
            "entity": entity,
            "name": _layers.decode(fields[NAME_AT].strip().strip("'")),
            "tag": fields[TAG_AT].strip().strip("'"),
        }
        (types if typed else products)[number] = row
    return products, types, of_type


def typed_elements(products, types, of_type):
    """Element id -> the row of the type it is an instance of."""
    out = {}
    for number, product in products.items():
        type_number = of_type.get(number)
        if type_number in types:
            out[product["tag"]] = types[type_number]
    return out


def report_entities(label, types):
    """One line per type class: how many entities, ids and names it has."""
    per = collections.defaultdict(lambda: ([], set(), set()))
    for row in types.values():
        entities, tags, names = per[row["entity"]]
        entities.append(row)
        tags.add(row["tag"])
        names.add(row["name"])
    print(
        f"{label}: {len(types)} type entities, "
        f"{len({row['tag'] for row in types.values()})} distinct type element ids"
    )
    for entity, (entities, tags, names) in sorted(
        per.items(), key=lambda pair: -len(pair[1][0])
    ):
        print(
            f"    {entity:34s} entities={len(entities):5d} "
            f"ids={len(tags):5d} names={len(names):5d}"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference", help="Revit's own IFC export of the model")
    parser.add_argument("ours", help="our `export-ifc` output for the same model")
    parser.add_argument(
        "--examples",
        type=int,
        default=6,
        help="disagreeing type pairs printed per kind",
    )
    arguments = parser.parse_args()

    reference = read(arguments.reference)
    ours = read(arguments.ours)
    report_entities("reference", reference[1])
    report_entities("ours     ", ours[1])

    theirs = typed_elements(*reference)
    mine = typed_elements(*ours)
    common = sorted(
        set(theirs) & set(mine), key=lambda tag: (not tag.isdigit(), tag.zfill(12))
    )
    print(
        f"\nelements with a type in both: {len(common)} "
        f"(reference {len(theirs)}, ours {len(mine)})"
    )
    if not common:
        return 1

    agree = sum(1 for tag in common if theirs[tag]["tag"] == mine[tag]["tag"])
    print(f"  same type element id: {agree}/{len(common)} ({agree / len(common):.1%})")

    present = {row["tag"] for row in ours[1].values()} | set(mine)
    absent = collections.Counter(
        theirs[tag]["tag"] for tag in common if theirs[tag]["tag"] not in present
    )
    unresolved = sum(absent.values())
    print(
        f"  of the rest, naming a type this export carries nowhere: "
        f"{unresolved} element(s) over {len(absent)} type id(s)"
    )
    print(f"  leaving a real disagreement on {len(common) - agree - unresolved} element(s)")

    merged = collections.defaultdict(set)
    split = collections.defaultdict(set)
    for tag in common:
        merged[mine[tag]["tag"]].add(theirs[tag]["tag"])
        split[theirs[tag]["tag"]].add(mine[tag]["tag"])
    merged = {one: many for one, many in merged.items() if len(many) > 1}
    split = {one: many for one, many in split.items() if len(many) > 1}
    print(f"  our type ids the reference splits: {len(merged)}")
    for one, many in list(merged.items())[: arguments.examples]:
        print(f"    ours {one} -> reference {sorted(many)}")
    print(f"  reference type ids we split: {len(split)}")
    for one, many in list(split.items())[: arguments.examples]:
        print(f"    reference {one} -> ours {sorted(many)}")

    per_entity = collections.Counter(
        mine[tag]["entity"] for tag in common if theirs[tag]["tag"] != mine[tag]["tag"]
    )
    if per_entity:
        print("  disagreements by our type class:")
        for entity, count in per_entity.most_common():
            print(f"    {entity:34s} {count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
