#!/usr/bin/env python3
"""Convert an IFC file's geometrized products into a single OBJ, for viewers
(Blender, MeshLab, ...) that have no native IFC support. Uses world
coordinates so everything lands in one consistent scene.
"""

import sys

import ifcopenshell
import ifcopenshell.geom


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <input.ifc> <output.obj>", file=sys.stderr)
        return 2
    ifc_path, obj_path = sys.argv[1], sys.argv[2]

    model = ifcopenshell.open(ifc_path)
    settings = ifcopenshell.geom.settings()
    settings.set(settings.USE_WORLD_COORDS, True)

    vertex_offset = 0
    objects = 0
    with open(obj_path, "w", encoding="utf-8") as out:
        out.write(f"# converted from {ifc_path}\n")
        for product in model.by_type("IfcProduct"):
            if not getattr(product, "Representation", None):
                continue
            try:
                shape = ifcopenshell.geom.create_shape(settings, product)
            except Exception:
                continue
            geometry = shape.geometry
            verts = geometry.verts
            faces = geometry.faces
            if not verts or not faces:
                continue
            objects += 1
            name = f"{product.is_a()}_{product.id()}"
            out.write(f"o {name}\n")
            for i in range(0, len(verts), 3):
                out.write(f"v {verts[i]} {verts[i + 1]} {verts[i + 2]}\n")
            for i in range(0, len(faces), 3):
                a = faces[i] + 1 + vertex_offset
                b = faces[i + 1] + 1 + vertex_offset
                c = faces[i + 2] + 1 + vertex_offset
                out.write(f"f {a} {b} {c}\n")
            vertex_offset += len(verts) // 3

    print(f"Wrote {objects} objects to {obj_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
