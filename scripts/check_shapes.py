#!/usr/bin/env python3
"""Build every represented product of an IFC with IfcOpenShell and report what
the kernel accepts, by product type. The settings are the ones a Box-only or
Axis-only product needs; see docs/reverse-engineering.md."""

import collections
import sys

import ifcopenshell
import ifcopenshell.geom


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <input.ifc>", file=sys.stderr)
        return 2
    model = ifcopenshell.open(sys.argv[1])
    settings = ifcopenshell.geom.settings()
    settings.set("dimensionality", ifcopenshell.ifcopenshell_wrapper.CURVES_SURFACES_AND_SOLIDS)
    settings.set("keep-bounding-boxes", True)
    settings.set("use-world-coords", True)

    built = collections.Counter()
    failed = collections.Counter()
    reasons = collections.Counter()
    for product in model.by_type("IfcProduct"):
        if not getattr(product, "Representation", None):
            continue
        try:
            ifcopenshell.geom.create_shape(settings, product)
            built[product.is_a()] += 1
        except Exception as error:  # noqa: BLE001 - the kernel's refusal is the datum
            failed[product.is_a()] += 1
            reasons[str(error).splitlines()[0][:80]] += 1
    print(f"built {sum(built.values())}, failed {sum(failed.values())}")
    for name in sorted(set(built) | set(failed)):
        print(f"  {name}\tbuilt {built[name]}\tfailed {failed[name]}")
    for reason, count in reasons.most_common(10):
        print(f"  {count}\t{reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
