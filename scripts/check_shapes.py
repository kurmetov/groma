#!/usr/bin/env python3
"""Build every represented product of an IFC with IfcOpenShell and report what
the kernel accepts, by product type. The settings are the ones a Box-only or
Axis-only product needs; see docs/reverse-engineering.md.

Each product is built in a forked child, because `create_shape` does not only
raise: on some bodies it segfaults and takes the interpreter with it. Run in
one process, this script then dies after tens of minutes having printed
nothing, which is how AR S1 read as "still running" three times. A crash now
costs that one product and is counted as its own outcome, so the tally is
always complete and the products the kernel cannot survive are named.
"""

import collections
import os
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
    crashed = collections.Counter()
    crashed_tags = []
    for product in model.by_type("IfcProduct"):
        if not getattr(product, "Representation", None):
            continue
        child = os.fork()
        if child == 0:
            try:
                ifcopenshell.geom.create_shape(settings, product)
                os._exit(0)  # noqa: SLF001 - the child must not run any exit handler
            except Exception:  # noqa: BLE001 - the kernel's refusal is the datum
                os._exit(1)  # noqa: SLF001
        _, status = os.waitpid(child, 0)
        if os.WIFSIGNALED(status):
            crashed[product.is_a()] += 1
            crashed_tags.append(product.Tag)
        elif os.WEXITSTATUS(status) == 0:
            built[product.is_a()] += 1
        else:
            failed[product.is_a()] += 1
    print(
        f"built {sum(built.values())}, failed {sum(failed.values())},"
        f" crashed the kernel {sum(crashed.values())}"
    )
    for name in sorted(set(built) | set(failed) | set(crashed)):
        print(
            f"  {name}\tbuilt {built[name]}\tfailed {failed[name]}"
            f"\tcrashed {crashed[name]}"
        )
    if crashed_tags:
        print("  crashed on element " + ", ".join(str(tag) for tag in crashed_tags[:20]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
