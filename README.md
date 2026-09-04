# Rivet

Rivet is an early-stage, Linux-native, read-only ingestion engine for Autodesk
Revit `.rvt` files. It does not require Revit, Windows, Wine, Autodesk Platform
Services, or ODA BimRv.

The current milestone opens the CFB/OLE container, inventories and extracts raw
streams, recognizes the known truncated-gzip framing, reads the Revit release
from verified `BasicFileInfo` layouts, decodes the generic class inventory in
`Formats/Latest`, and inventories checksum-paged compressed partition members
while retaining only a bounded 16-byte structural prefix per member. It also
decodes the `Global/ElemTable` candidate-ID index, recovers the fixed
per-member descriptor and the length-prefixed record array inside partition
members, including records that continue across member boundaries, resolves
each record's element identifier and schema class, and can run aggregate, schema-backed partition probes without loading
complete partitions into memory.

## Build

Rivet requires Rust 1.85 or newer.

```bash
cargo build --workspace
cargo test --workspace
```

## CLI

```bash
cargo run -p rivet-cli -- info model.rvt
cargo run -p rivet-cli -- streams model.rvt
cargo run -p rivet-cli -- dump-stream model.rvt BasicFileInfo --output BasicFileInfo.bin
cargo run -p rivet-cli -- partitions model.rvt
cargo run -p rivet-cli -- schema model.rvt
cargo run -p rivet-cli -- elem-table model.rvt
cargo run -p rivet-cli -- partition-id-probe model.rvt
cargo run -p rivet-cli -- schema-prefix-probe model.rvt
cargo run -p rivet-cli -- marker-envelopes model.rvt --dump 8
cargo run -p rivet-cli -- member-framing model.rvt --dump 4
cargo run -p rivet-cli -- dump-member model.rvt Partitions/81 44 --output member.bin
cargo run -p rivet-cli -- schema model.rvt --class ElementHeader
cargo run -p rivet-cli -- records model.rvt
cargo run -p rivet-cli -- bodies model.rvt --class ElementHeader --count 8
cargo run -p rivet-cli -- element model.rvt <element-id> --bytes 32
cargo run -p rivet-cli -- inspect model.rvt
cargo run -p rivet-cli -- names model.rvt
cargo run -p rivet-cli -- parameters model.rvt --class FamilyInstance
cargo run -p rivet-cli -- export-json model.rvt --output model.jsonl
cargo run -p rivet-cli -- export-ifc model.rvt --output model.ifc
python -m ifcopenshell.validate model.ifc --rules
```

`schema` reports the generic class hierarchy and property counts without
assigning BIM semantics. `elem-table` reports index structure without listing
IDs unless `--records` is explicitly requested. Commands ending in `-probe`
report experimental correlations, not decoded objects. `marker-envelopes`
retains a bounded byte window around each schema-resolved class marker, with
its partition, member, and decoded offset, and reports the distributions a
record boundary would have to explain; it does not claim one. `member-framing`
validates the fixed 40-byte descriptor in front of every compressed member and
walks each decoded member as length-prefixed records, carrying a record that
continues into the next member and checking every walk against two independent
descriptor fields. `inspect` reports the recovered object
inventory: identifiers, records, and the class histogram. `element` lists every
record belonging to one identifier, with its category and family reference when
the element's header record carries them. `bodies` prints record payloads of one
class as hex for field analysis. `export-json` writes one JSON object per
element with its class, name, category code, level, phase, level elevation,
parameter values, Revit 2023 built-in names, model-defined parameter specs,
known canonical-unit conversions, deletion/lock flags, and the byte location
it came from. Independently bounds-verified straight pipes additionally carry
a metric swept-disk axis and radius; owner-verified, single-line pipe-fitting
centerlines carry an axis-only metric representation. A diagnostic
`FamilyInstance` placement is included only when one orthonormal
`m_instOrigin`/`m_RefDir`/`m_zAxis` candidate is unique inside independently
recovered owner bounds; it is not yet promoted to IFC. `names` reports where each class keeps its
string and how consistently, so a name read at a calibrated offset can be told
apart from one found by scanning.
`export-ifc` writes an IFC4 Reference View file with
`IfcProject -> IfcSite -> IfcBuilding -> IfcBuildingStorey`, metric units,
deterministic 22-character GlobalIds, and typed MEP elements. The current
conservative mapping covers pipe segments/fittings, sanitary/air/fire-
suppression terminals; unknown class/category pairs remain
`IfcBuildingElementProxy` instances. Verified straight pipes receive an
`IfcPolyline` axis and `IfcSweptDiskSolid` body. Pipe fittings with one
unambiguous straight `PipeFittingCenterLine` receive an `IfcPolyline` axis but
no invented body. Bounds-verified, right-handed `GInstance` transforms become
storey-relative `IfcLocalPlacement` values; world-space geometry is converted
back into that local frame. Other geometry is omitted rather than approximated.
Project storeys are `Level` records without a family reference; family-local
reference levels are not promoted to building storeys.
By default it includes live categorized records with a recovered level;
`--include-unplaced` also includes categorized records without one. Parameter
arrays are written only when their value kinds agree with the dynamic set
classes referenced by the element and exactly one matching run is present;
legacy unbound results from unsupported releases stay out of IFC. Pass
`--model-namespace <UUID>` to keep IDs stable if the source file moves,
otherwise the canonical RVT path determines the namespace.
Unknown stream bytes are always available through `dump-stream` and the
`rvt-container` API.

## Workspace

- `rvt-container`: physical CFB streams and known compression/framing
- `rvt-schema`: generic schema definitions
- `rvt-model`: loss-preserving serialized-object IR
- `bim-core`: format-independent BIM types
- `revit-catalog`: versioned Revit identifiers and Forge unit conversion
- `ifc-export`: IFC4 model builder, GlobalId and STEP writer
- `rivet-cli`: command-line interface

See [`docs/architecture.md`](docs/architecture.md) and
[`docs/format-notes.md`](docs/format-notes.md) for current boundaries and
evidence.

## Status

Rivet is experimental. It recovers the object graph's records, identifiers,
classes, selected fields, level elevations, names, and schema-bound parameter
sets, plus independently checked straight-pipe bodies and straight fitting
axes. General geometry is not decoded, and Rivet never writes RVT files.

The JSON-lines diagnostic exporter and a conservative IFC4 exporter are
implemented. Coordination IFC still requires most element geometry, and legacy
parameter candidates from unsupported schemas stay out of IFC. See
[`docs/architecture.md`](docs/architecture.md).

The live metadata sample is checked with IfcOpenShell's schema validator and
IFC4 EXPRESS rules in addition to the Rust test suite.
