#!/usr/bin/env python3
"""Read a `.rvs` scene the way a browser reads it, and check it holds together.

`groma export-scene` reports what it believes it wrote. This reads the file
back through the format's own contract - the 24-byte trailer, the manifest, the
raw-deflate sections - and checks that report against what is actually there.
Every decompression here is plain raw deflate, which is what a browser gets
from `DecompressionStream("deflate-raw")`, so a file this script reads is a
file a viewer can read.

    groma export-scene model.rvt --output model.rvs
    scripts/check_scene.py model.rvs

Pass `--obj model.obj` to dequantize the triangles into a single OBJ in world
metres, for the viewers that already have a path here:

    scripts/check_scene.py model.rvs --obj /tmp/model.obj
    blender --python scripts/blender_view_obj.py -- /tmp/model.obj

That conversion undoes exactly the sixteen-bit position encoding a viewer has
to undo, so a model that looks right in Blender is one whose chunk extents and
quantization are right.
"""

import argparse
import array
import json
import struct
import sys
import zlib

CHUNK_MAGIC = 0x48435652  # "RVCH"
HEADER_WORDS = 12
MESH_WORDS = 7
TRAILER_BYTES = 24


class Failed(Exception):
    """A check that did not hold. Carries the message the caller prints."""


def check(condition, message):
    if not condition:
        raise Failed(message)


def section(data, span):
    """One raw-deflate section, checked against the length it declares."""
    try:
        raw = zlib.decompress(data[span["offset"]:span["offset"] + span["stored"]], -15)
    except zlib.error as error:
        raise Failed(f"section at {span['offset']} is not valid raw deflate: {error}") from error
    check(
        len(raw) == span["length"],
        f"section at {span['offset']} inflated to {len(raw)}, not {span['length']}",
    )
    return raw


def read_manifest(data):
    # The second of each pair is what a scene packed before the rename from
    # Rivet carries. Only the name stamped in the header changed.
    check(data[:8] in (b"OPENRVTS", b"RIVETSCN"), "file does not start with OPENRVTS")
    check(data[-8:] in (b"OPENRVTE", b"RIVETEND"), "file does not end with OPENRVTE")
    version, _reserved = struct.unpack_from("<II", data, 8)
    offset, stored, length = struct.unpack_from("<QII", data, len(data) - TRAILER_BYTES)
    manifest = json.loads(section(data, {"offset": offset, "stored": stored, "length": length}))
    check(
        manifest["format"] in ("openrvt-scene", "rivet-scene"),
        f"unexpected format {manifest['format']!r}",
    )
    return version, manifest


def read_chunk(data, entry):
    """One chunk's header, mesh table, and dequantized positions in metres."""
    raw = section(data, entry)
    header = array.array("I")
    header.frombytes(raw[:HEADER_WORDS * 4])
    check(header[0] == CHUNK_MAGIC, f"chunk magic {header[0]:#x}, not {CHUNK_MAGIC:#x}")
    vertices, indices, _edge_vertices, _edge_indices, meshes = header[1:6]
    off_positions, _off_normals, off_indices = header[6:9]

    quantised = array.array("H")
    quantised.frombytes(raw[off_positions:off_positions + vertices * 6])
    low = entry["min"]
    span = [max(entry["max"][axis] - low[axis], sys.float_info.min) for axis in range(3)]
    positions = [
        low[axis] + quantised[at + axis] / 65535.0 * span[axis]
        for at in range(0, len(quantised), 3)
        for axis in range(3)
    ]

    triangles = array.array("I")
    triangles.frombytes(raw[off_indices:off_indices + indices * 4])

    table = array.array("I")
    table.frombytes(raw[header[11]:header[11] + meshes * MESH_WORDS * 4])
    return positions, triangles, table, header


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scene")
    parser.add_argument("--obj", help="also write the triangles here, in world metres")
    arguments = parser.parse_args()

    data = open(arguments.scene, "rb").read()
    version, manifest = read_manifest(data)
    counts = manifest["counts"]
    source = manifest["source"]
    print(f"{arguments.scene}: {len(data):,} bytes, format version {version}")
    print(f"  source     {source['name']} ({source['kind']}, {source['application']} {source['release']})")
    print(f"  unit       {manifest['unit']}")
    print(f"  reports    {counts['elements']:,} elements, {counts['withGeometry']:,} with geometry")
    print(f"             {counts['triangles']:,} triangles, {counts['vertices']:,} vertices, {counts['edges']:,} edges")
    if counts["skippedFaces"]:
        print(f"  skipped    {counts['skippedFaces']:,} faces the tessellator could not read")

    # An element carries no box unless it reached a chunk, so the rows that
    # are not NaN are exactly the elements with geometry.
    bounds = array.array("f")
    bounds.frombytes(section(data, manifest["elementBounds"]))
    boxes = {
        at // 6: bounds[at:at + 6]
        for at in range(0, len(bounds), 6)
        if bounds[at] == bounds[at]
    }
    check(
        len(boxes) == counts["withGeometry"],
        f"{len(boxes)} element boxes for {counts['withGeometry']} elements with geometry",
    )

    obj = open(arguments.obj, "w") if arguments.obj else None
    written = 0
    vertices = triangles = 0
    with_geometry = set()
    for entry in manifest["chunks"]:
        positions, indices, table, header = read_chunk(data, entry)
        check(
            header[1] == entry["vertices"] and header[2] // 3 == entry["triangles"],
            "a chunk's header disagrees with the manifest row that located it",
        )
        vertices += header[1]
        triangles += header[2] // 3
        for at in range(0, len(table), MESH_WORDS):
            with_geometry.add(table[at])
        # Checking a position against the box it was decoded with would prove
        # nothing: it cannot fall outside by construction. Two things can be
        # said without it.
        #
        # The chunk's box and the element boxes accumulate the same positions,
        # one over the chunk and one per element, so their union is the chunk
        # box exactly. That is what a viewer decodes positions with, and it is
        # checked here against a section written independently of it.
        named = [table[at] for at in range(0, len(table), MESH_WORDS)]
        for element in named:
            check(element in boxes, f"chunk names element {element}, which has no bounds")
        for axis in range(3):
            low = min(boxes[element][axis] for element in named)
            high = max(boxes[element][axis + 3] for element in named)
            check(
                abs(low - entry["min"][axis]) <= 1e-3 and abs(high - entry["max"][axis]) <= 1e-3,
                f"chunk {'xyz'[axis]} extent {entry['min'][axis]:.4f}..{entry['max'][axis]:.4f} "
                f"against its elements' union {low:.4f}..{high:.4f}",
            )
        # And an element's triangles must land inside the box declared for it.
        # Not on it: the box also covers the element's declared edge curves,
        # which follow the true arc and so sit outside a chord-tolerant
        # triangulation.
        slack = [(entry["max"][axis] - entry["min"][axis]) / 65535.0 * 2 + 1e-4 for axis in range(3)]
        for at in range(0, len(table), MESH_WORDS):
            element, vertex_start, vertex_count = table[at], table[at + 1], table[at + 2]
            declared = boxes[element]
            for axis in range(3):
                seen = [positions[(vertex_start + i) * 3 + axis] for i in range(vertex_count)]
                check(
                    min(seen) >= declared[axis] - slack[axis]
                    and max(seen) <= declared[axis + 3] + slack[axis],
                    f"element {element}: decoded {'xyz'[axis]} "
                    f"{min(seen):.4f}..{max(seen):.4f} outside declared bounds "
                    f"{declared[axis]:.4f}..{declared[axis + 3]:.4f}",
                )
        if obj:
            for at in range(0, len(positions), 3):
                obj.write(f"v {positions[at]:.6f} {positions[at + 1]:.6f} {positions[at + 2]:.6f}\n")
            for at in range(0, len(indices), 3):
                a, b, c = (indices[at] + 1 + written, indices[at + 1] + 1 + written, indices[at + 2] + 1 + written)
                obj.write(f"f {a} {b} {c}\n")
            written += header[1]
    if obj:
        obj.close()

    # The chunks are the geometry. If their headers do not add up to what the
    # manifest claims, a viewer would draw something other than what the
    # exporter reported, which is the failure worth catching here.
    check(vertices == counts["vertices"], f"chunks hold {vertices} vertices, manifest says {counts['vertices']}")
    check(triangles == counts["triangles"], f"chunks hold {triangles} triangles, manifest says {counts['triangles']}")
    check(
        len(with_geometry) == counts["withGeometry"],
        f"chunks name {len(with_geometry)} elements, manifest says {counts['withGeometry']}",
    )
    print(f"  chunks     {len(manifest['chunks'])} decode, and agree with the manifest")

    blocks = manifest["properties"]["blocks"]
    size = manifest["properties"]["blockSize"]
    for span in blocks:
        json.loads(section(data, span))
    expected = -(-len(manifest["elements"]["ids"]) // size)
    check(len(blocks) == expected, f"{len(blocks)} property blocks for {len(manifest['elements']['ids'])} elements")
    print(f"  properties {len(blocks)} blocks of {size} decode as JSON")

    extent = [
        (min(box[axis] for box in boxes.values()), max(box[axis + 3] for box in boxes.values()))
        for axis in range(3)
    ]
    print(f"  extent     " + "  ".join(f"{'xyz'[axis]} {low:.1f}..{high:.1f}" for axis, (low, high) in enumerate(extent)))
    print(f"  levels     {len(manifest['levels'])}")
    if arguments.obj:
        print(f"  wrote      {arguments.obj}")
    print("OK")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failed as failure:
        print(f"FAILED: {failure}", file=sys.stderr)
        sys.exit(1)
