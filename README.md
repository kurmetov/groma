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
element with its class, name, category code, level, phase, parameter values,
deletion/lock flags, and the byte location it came from. `names` reports where each class keeps its
string and how consistently, so a name read at a calibrated offset can be told
apart from one found by scanning. Parameters, names, and geometry are still undecoded, so they are
absent from the export rather than approximated.
Unknown stream bytes are always available through `dump-stream` and the
`rvt-container` API.

## Workspace

- `rvt-container`: physical CFB streams and known compression/framing
- `rvt-schema`: generic schema definitions
- `rvt-model`: loss-preserving serialized-object IR
- `bim-core`: format-independent BIM types
- `rivet-cli`: command-line interface

See [`docs/architecture.md`](docs/architecture.md) and
[`docs/format-notes.md`](docs/format-notes.md) for current boundaries and
evidence.

## Status

Rivet is experimental. It recovers the object graph's records, identifiers, and
classes, but not yet record contents or geometry, and it never writes RVT
files.

The exporter targets are JSON first and IFC after it, both reading `bim-core`
only. Neither is implemented yet: they follow the object graph and semantic
reconstruction. See [`docs/architecture.md`](docs/architecture.md).
