#!/usr/bin/env python3
"""Compare the decoded wall layer tables against Revit's own IFC export.

The corpus holds two AR files that ship the IFC Revit itself exported from the
same model. That export is an answer this project did not produce, and it
carries, per wall, the element identifier Revit gives it (`IfcWall.Tag`), the
name of its type (`IfcWall.Name`, `family:type:id`) and an
`IfcMaterialConstituentSet` listing the wall's layers in order with each
layer's material name and its share of the total width. Those are exactly the
things `rivet layers` decodes out of `CompoundStructure`, so the two can be
joined and disagreements counted.

    rivet layers model.rvt --types 4000 --links > layers.txt
    scripts/compare_wall_layers.py revit-export.ifc layers.txt

The second argument may instead be our own `export-ifc` output, which carries
the same element identifier in `IfcWall.Tag` and the layers as an
`IfcMaterialLayerSet`. That checks the export path as well as the decode, since
the layers then travel all the way through `bim-core` and the IFC writer.

    rivet export-ifc model.rvt --output ours.ifc
    scripts/compare_wall_layers.py revit-export.ifc ours.ifc

Two measurements come out:

A. whether `m_WallAttributesId` names the type Revit gives the same wall;
B. whether the decoded layers reproduce the constituent set - same count, same
   material names in the same order, and each layer's share of the total width.

Widths are compared as shares rather than absolutely because Revit's export
writes `IfcMaterialConstituent.Fraction`, not a thickness. Zero-width layers -
membranes - are dropped from our side first: Revit's exporter omits them.
"""

import collections
import re
import sys


def decode(text):
    """Resolve IFC's `\\X2\\...\\X0\\` UTF-16 escapes."""

    def units(match):
        digits = match.group(1)
        return "".join(
            chr(int(digits[at : at + 4], 16)) for at in range(0, len(digits), 4)
        )

    return re.sub(r"\\X2\\([0-9A-Fa-f]+)\\X0\\", units, text)


def arguments(text):
    """Split one IFC instance's argument list, respecting strings and nesting."""
    out, depth, current, quoted = [], 0, "", False
    for character in text:
        if quoted:
            current += character
            if character == "'":
                quoted = False
        elif character == "'":
            quoted = True
            current += character
        elif character == "(":
            depth += 1
            current += character
        elif character == ")":
            if depth == 0:
                break
            depth -= 1
            current += character
        elif character == "," and depth == 0:
            out.append(current)
            current = ""
        else:
            current += character
    out.append(current)
    return out


def read_reference_export(path):
    """Walls from Revit's export: element id -> (type name, layers or None)."""
    text = open(path, encoding="utf-8", errors="replace").read()
    instances = {
        int(m.group(1)): (m.group(2), m.group(3))
        for m in re.finditer(r"^#(\d+)=(IFC[A-Z0-9]+)\((.*)$", text, re.M)
    }

    materials = {
        at: decode(arguments(rest)[0].strip("'"))
        for at, (entity, rest) in instances.items()
        if entity == "IFCMATERIAL"
    }
    constituents = {}
    for at, (entity, rest) in instances.items():
        if entity != "IFCMATERIALCONSTITUENT":
            continue
        fields = arguments(rest)
        constituents[at] = (
            int(fields[2].strip().lstrip("#")),
            float(fields[3]),
        )
    sets = {
        at: [
            int(x.strip().lstrip("#"))
            for x in arguments(rest)[2].strip().strip("()").split(",")
            if x.strip()
        ]
        for at, (entity, rest) in instances.items()
        if entity == "IFCMATERIALCONSTITUENTSET"
    }
    associated = {}
    for at, (entity, rest) in instances.items():
        if entity != "IFCRELASSOCIATESMATERIAL":
            continue
        fields = arguments(rest)
        for related in fields[4].strip().strip("()").split(","):
            related = related.strip().lstrip("#")
            if related.isdigit():
                associated[int(related)] = int(fields[5].strip().lstrip("#"))

    products = {}
    for at, (entity, rest) in instances.items():
        if not is_tagged_product(entity):
            continue
        fields = arguments(rest)
        if len(fields) < 8:
            continue
        tag = fields[7].strip().strip("'")
        if not tag.isdigit():
            continue
        # `family:type:id`, with the type in the middle.
        parts = decode(fields[2].strip("'")).split(":")
        type_name = parts[1] if len(parts) > 2 else parts[-1]
        constituent_set = sets.get(associated.get(at))
        layers = (
            None
            if constituent_set is None
            else [
                (constituents[c][1], materials.get(constituents[c][0], "?"))
                for c in constituent_set
            ]
        )
        products[int(tag)] = (entity, type_name, layers)
    return products


def is_tagged_product(entity):
    """Whether the entity is a product carrying `Tag` as its eighth attribute.

    Everything relational, material or type-level is excluded; what is left is
    `IfcElement` and its subtypes, which all declare `Tag` in that slot.
    """
    return not any(
        entity.startswith(prefix)
        for prefix in ("IFCREL", "IFCMATERIAL", "IFCPROPERTY", "IFCQUANTITY")
    ) and not entity.endswith("TYPE")


def read_our_export(path):
    """Our own `export-ifc` output, in the shape `read_layers_report` returns.

    The layer set stands in for the type record: every product sharing a build-up
    shares one set, so its entity number is a usable identity for the join.
    """
    text = open(path, encoding="utf-8", errors="replace").read()
    instances = {
        int(m.group(1)): (m.group(2), m.group(3))
        for m in re.finditer(r"^#(\d+)=(IFC[A-Z0-9]+)\((.*)$", text, re.M)
    }
    materials = {
        at: decode(arguments(rest)[0].strip("'"))
        for at, (entity, rest) in instances.items()
        if entity == "IFCMATERIAL"
    }
    layers = {}
    for at, (entity, rest) in instances.items():
        if entity != "IFCMATERIALLAYER":
            continue
        fields = arguments(rest)
        material = fields[0].strip().lstrip("#")
        layers[at] = (
            float(fields[1]),
            materials.get(int(material)) if material.isdigit() else None,
        )
    sets = {}
    for at, (entity, rest) in instances.items():
        if entity != "IFCMATERIALLAYERSET":
            continue
        members = [
            int(x.strip().lstrip("#"))
            for x in arguments(rest)[0].strip().strip("()").split(",")
            if x.strip()
        ]
        sets[at] = (at, decode(arguments(rest)[1].strip("'")), [layers[m] for m in members])

    tags = {}
    for at, (entity, rest) in instances.items():
        if not entity.startswith("IFC") or "MATERIAL" in entity:
            continue
        fields = arguments(rest)
        if len(fields) < 8:
            continue
        tag = fields[7].strip().strip("'")
        if tag.isdigit():
            tags[at] = int(tag)

    links = {}
    for at, (entity, rest) in instances.items():
        if entity != "IFCRELASSOCIATESMATERIAL":
            continue
        fields = arguments(rest)
        layer_set = int(fields[5].strip().lstrip("#"))
        if layer_set not in sets:
            continue
        for related in fields[4].strip().strip("()").split(","):
            related = related.strip().lstrip("#")
            if related.isdigit() and int(related) in tags:
                element_id = tags[int(related)]
                links[element_id] = ("-", "-", layer_set, sets[layer_set][1])

    return links, sets


def read_layers_report(path):
    """`rivet layers --links` output: the type links and the layer tables."""
    links, types, current = {}, {}, None
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("LINK "):
            fields = line[5:].split("\t")
            links[int(fields[0])] = (fields[1], fields[2], int(fields[3]), fields[5])
        elif line.startswith("TYPE "):
            fields = line[5:].split("\t")
            current = (int(fields[0]), fields[2], [])
            types[current[0]] = current
        elif line.startswith("  LAYER ") and current is not None:
            fields = line.split("\t")
            current[2].append((float(fields[1]), fields[4]))
    return links, types


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    walls = read_reference_export(sys.argv[1])
    if sys.argv[2].lower().endswith(".ifc"):
        links, types = read_our_export(sys.argv[2])
    else:
        links, types = read_layers_report(sys.argv[2])

    # Only the products whose type this decode claims to read: a compound host
    # object. Revit names an opening or an insert after the wall that hosts it,
    # so including those would compare a window's type against its host's.
    hosts = ("IFCWALL", "IFCSLAB", "IFCROOF", "IFCCOVERING", "IFCPLATE")
    counts = collections.Counter()
    disagreed = collections.Counter()
    for element_id, (entity, type_name, _) in walls.items():
        if entity not in hosts:
            continue
        counts[entity, "in the reference"] += 1
        link = links.get(element_id)
        if link is None:
            continue
        counts[entity, "linked"] += 1
        if link[3] == type_name or link[3].endswith(type_name):
            counts[entity, "same type name"] += 1
        else:
            disagreed[(link[3], type_name)] += 1
    print("A. the type link against the type Revit gives the same element")
    for entity in hosts:
        total = counts[entity, "in the reference"]
        if total == 0:
            continue
        print(
            f"   {entity}: {total} in the reference,"
            f" {counts[entity, 'linked']} linked,"
            f" {counts[entity, 'same type name']} naming the same type"
        )
    for (ours, theirs), count in disagreed.most_common(8):
        print(f"     ours {ours!r} vs Revit {theirs!r}: {count}")

    checked = same_count = same_names = same_shares = 0
    differing = []
    per_entity = collections.Counter()
    worst = 0.0
    for element_id, (entity, _, theirs) in walls.items():
        link = links.get(element_id)
        if theirs is None or link is None or link[2] not in types:
            continue
        # Revit's export omits the zero-width membrane layers.
        ours = [(w, name) for w, name in types[link[2]][2] if w > 0.0]
        checked += 1
        per_entity[entity] += 1
        if len(ours) != len(theirs):
            if len(differing) < 4:
                differing.append(
                    (element_id, entity, len(ours), len(theirs))
                )
            continue
        same_count += 1
        if [name for _, name in ours] == [name for _, name in theirs]:
            same_names += 1
        total = sum(w for w, _ in ours)
        if total <= 0.0:
            continue
        error = max(
            abs(w / total - fraction) for (w, _), (fraction, _) in zip(ours, theirs)
        )
        worst = max(worst, error)
        same_shares += error < 1e-6
    print()
    print("B. the decoded layer table against the constituent set Revit wrote")
    print(f"   products where both sides have one: {checked}")
    for entity, count in per_entity.most_common():
        print(f"     {entity}: {count}")
    print(f"   same layer count: {same_count}")
    print(f"   same material names, in order: {same_names}")
    print(f"   every layer's share of the total agrees to 1e-6: {same_shares}")
    print(f"   worst disagreement in any layer's share: {worst:.3e}")
    for element_id, entity, ours, theirs in differing:
        print(f"     {entity} {element_id}: {ours} layers here, {theirs} in the reference")
    return 0


if __name__ == "__main__":
    sys.exit(main())
