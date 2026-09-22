# groma

groma is an early-stage, Linux-native, read-only ingestion engine for building
models. It reads Autodesk Revit `.rvt` files - releases 2023 and 2026, on the
evidence set out below - and IFC (ISO 10303-21), converts between them, and needs neither Revit, Windows,
Wine, Autodesk Platform Services, nor ODA BimRv.

The source format is read from a file's own leading bytes, so `export-scene`
and `export-ifc` take either an `.rvt` or an `.ifc` without being told which.
Both also take several files at once and read them as one federated model.

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

## Supported Revit releases

groma carries identifier tables for **Revit 2023 and Revit 2026**, and for no
other release. `Catalog::for_release` answers for those two and returns nothing
for anything else, so a file from 2024 or 2025 is read without built-in
parameter names, without their units, and without any category-driven IFC
classification, leaving only the handful of mappings made from class names.
`groma info` states which of the two applies to a file, `export-scene` and
`export-ifc` say so when neither does, `export-json --full` writes it as
`parameter_catalog`, and a converted scene carries the same answer into the
viewer, which marks a model read without a catalog.

The two releases do not stand on the same evidence, and it is worth being
precise about the difference.

**2023 is the measured release.** Every model behind
[`tests/baseline/corpus_metrics.tsv`](tests/baseline/corpus_metrics.tsv) and
every `Partitions/*` framing claim in
[`docs/format-notes.md`](docs/format-notes.md) is 2023.

**2026 is catalogued but not measured.** Its `BuiltInParameter` and
`BuiltInCategory` tables are generated from Autodesk's own published 2026
enumerations - 3,699 parameter and 1,212 category codes, against 3,439 and
1,189 for 2023 - so on a 2026 file every built-in code those tables carry is
named by the release that wrote it. That matters beyond the new codes: 2026
renames twelve parameters and one category that 2023 spells differently,
`UNIFORMAT_CODE` becoming `ASSEMBLY_CODE` among them, and reading either
release's file through the other's table would quietly mislabel them. What has
*not* been measured is the rest of a 2026 read: the layers that read a file's
own declarations work - the container, `BasicFileInfo`, and the `Formats/Latest`
class inventory, which was decoded from a 2026 file with no trailing bytes and
no unresolved class - but the record framing inside `Partitions/*` has never
been measured on a 2026 model, because no 2026 file has reached the reference
corpus. Treat a 2026 conversion as unverified until it has.

The Forge spec table is deliberately not release-keyed. A spec is named by a
fully qualified `ForgeTypeId` that the file itself states, and that identity is
version-insensitive by Autodesk's own convention, so `autodesk.spec.aec:length`
means metres whichever release wrote it. An identifier the table does not carry
resolves to nothing and its value reaches an exporter unconverted and marked,
so a newer release's additions cost coverage rather than correctness.

## Build

groma requires Rust 1.85 or newer.

```bash
cargo build --workspace
cargo test --workspace
```

The repository test suite builds synthetic CFB files at runtime, so it needs no
`.rvt` model and runs anywhere.

Decode accuracy is a separate gate, because it can only be measured against real
models and those are not redistributable. If you have a corpus, run:

```bash
scripts/corpus_check.sh                   # measure and compare
scripts/corpus_check.sh --write           # accept the current numbers
GROMA_CORPUS=/path/to/models scripts/corpus_check.sh
```

It re-measures the numbers in [`tests/baseline/corpus_metrics.tsv`](tests/baseline/corpus_metrics.tsv)
and fails if any of them moved the wrong way. Run it before and after any change
to the record walk: such a change can buy one class by selling another, and this
is what catches that. Without `GROMA_CORPUS` the gate skips, so `cargo test`
stays fast and CI stays meaningful.

## Size

An IFC export used to be several times the size of the model it came from: a
93 MB Revit file wrote 702 MB, and a 231 MB one wrote 1.29 GB. Same corpus,
same commit, before and after:

| Source | Before | After | |
| --- | ---: | ---: | ---: |
| 93 MB `.rvt` | 702 MB | **121 MB** | 5.8× smaller |
| 185 MB `.rvt` | 695 MB | **106 MB** | 6.6× smaller |
| 231 MB `.rvt` | 1.29 GB | **208 MB** | 6.2× smaller |
| 454 MB `.rvt` | 41 MB | **8.9 MB** | 4.7× smaller |

Three things, and the first two take nothing out of the file.

**A geometric resource is written once.** IFC gives an `IfcCartesianPoint` no
identity beyond its coordinates, so two with the same coordinates are one
point; the same holds for directions, vectors, lines, circles, surfaces,
placements, vertices and edges. The exporter wrote one per use: 2.17 million
cartesian points where 153 thousand were distinct, and 1.53 million directions
where 17 thousand were. Everything a reader can count as an object of its own -
a product, a representation, a property set, a face, a loop, a shell - is still
written once per use.

**A real is written in the digits it needs.** Every number was fifteen
fractional digits in exponential form, which spends nineteen bytes stating
zero.

**A coordinate is written without its arithmetic noise.** A corner stated twice
comes out of two different chains of matrix multiplications, so the two doubles
differ in their last bits although the model has one corner. Rounding to twelve
significant digits merges those and stops there: past twelve the count of
distinct points stops falling, which is the measurement that says the noise has
gone and nothing else with it. Twelve digits is six orders of magnitude finer
than the `1e-5` metre precision the file's own
`IfcGeometricRepresentationContext` declares. This is the one thing here that
does not write back exactly what it was given, and
[`SIGNIFICANT_DIGITS`](crates/ifc-export/src/lib.rs) carries the table it was
chosen from.

What came out is the same model. On the 93 MB file both exports hold 16 451
products; IfcOpenShell geometrizes the same 397 of the same 400 sampled
products from each, and reading each back into a scene gives the same 16 437
elements, the same 12 storeys, and a summed triangle area of 1570.43 m² against
1570.54 m² - 0.007%, which is the tessellator's own variation over the 79
vertices where two coincident corners became one. It also costs downstream
readers less: IfcOpenShell parses the file in 4.7 s where it took 20.8 s.

For scale, Revit's own IFC export of that 231 MB model is 80 MB. It is smaller
than our 208 MB because it writes walls, floors and columns as swept solids -
a profile and a depth - where this exporter writes the boundary representation
it recovered. Recognising a prism and writing it as one is the next lever, and
it is a change to what the file says rather than to how it says it.

## What a solid is written as

A boundary representation states every face of a solid, and this exporter wrote
them all. Revit's own export writes a wall, a floor or a column as a profile
and a depth instead, and that difference is most of the size gap between the
two files. A solid is now recognised as a prism - one planar profile swept
along a straight line - and written as an `IfcExtrudedAreaSolid` where it is
one:

| Source | Solids swept | The faces they held | Export before | after |
| --- | ---: | ---: | ---: | ---: |
| 231 MB architectural `.rvt` | 9 247 of 12 939 (71.5%) | 16.3% | 207.5 MB | **179.2 MB** |
| 185 MB electrical `.rvt` | 1 652 of 6 723 (24.6%) | 4.7% | 105.5 MB | **102.1 MB** |
| 454 MB structural `.rvt` | 71 of 787 (9.0%) | 3.1% | 8.9 MB | **8.8 MB** |
| 93 MB plumbing `.rvt` | 196 of 9 076 (2.2%) | 0.5% | 120.6 MB | **120.2 MB** |

Nothing is simplified to get there. A shell is written as a sweep only where it
**is** that sweep: every face planar, the shell closed - each edge used once
from either side, all in one piece - exactly two faces square to the direction
and facing each other, every other face parallel to it, and the two bounding
the same region. Those tests are what make the claim, and
[`extrusion.rs`](crates/ifc-export/src/extrusion.rs) carries the argument that
they are enough, along with the two shapes in the corpus that taught it what
they had to be.

An export says which test refused the rest, because that is where the next step
is rather than a guess about it:

```
Solids written as a swept profile, of 12939: 9247 (71.5%), holding 63459 faces (16.3%)
  a face was curved: 88 (0.7%), holding 3784 faces (1.0%)
  an edge was curved: 1938 (15.0%), holding 290712 faces (74.5%)
  the caps were stated in pieces: 1198 (9.3%), holding 12862 faces (3.3%)
  ...
  of the curved, those whose curves turn about one axis: 567 (4.4%), holding 6179 faces (1.6%)
```

Read it and the arc stops being tempting: three quarters of that model's faces
sit in solids with a curved edge, but only a twentieth of those solids turn all
their curves about one axis, which a swept profile would need. A profile that
could hold an arc is worth 1.6% of the faces there, and 8.8% to 15.6% on the
other three. The size that is left is in solids that are genuinely complicated,
not in a form this exporter has not learned yet.

### A face is tiled whole or not at all

A tiling that stops short used to be handed back as far as it got - a region
with a piece missing and no sign of it. It is refused now, and the face is
counted among the ones the tessellator could not read. What the stalls were was
measured before refusing them, and both causes were fixed rather than counted.
Most were faces stating two loops side by side - two regions rather than a
boundary and a hole - and a face's loops are now grouped into regions by
containment. The rest were on surfaces of revolution: a torus closes about its
axis *and* around its own profile, and only the first parameter was being
unwrapped, so a boundary crossing the profile's seam read as a jump that no
tiling can take. Both parameters are unwrapped now.

Three of the four corpus models therefore refuse **fewer** faces than before
this change while refusing every partial tiling - the plumbing model 3 579
against 3 881, the structural 56 against 65 - and two of them draw more surface
than they did.

## Performance

A conversion is measured the way decode accuracy is: against the reference
corpus, before and after. These are one 32-core Linux host, warm page cache,
`--release`:

| Command | Source | Before | After |
| --- | --- | ---: | ---: |
| `export-scene` | 93 MB `.rvt` | 14.3 s | **3.2 s** |
| `export-scene` | 453 MB `.rvt` | 40.6 s | **9.7 s** |
| `export-scene` | 1.28 GB `.ifc` | 18.0 s | **7.7 s** |
| `export-ifc` | 93 MB `.rvt`, 700 MB out | 80.9 s | **4.3 s** |
| `export-json --full` | 453 MB `.rvt` | 41.3 s | **10.5 s** |

The outputs are byte-for-byte what they were, which is the only claim worth
making about an optimisation to a decoder: every element of every corpus file
exports the same `export-json --full`, the same `.rvs`, and the same IFC apart
from the file name and timestamp its header states. The corpus gate's 164
measurements did not move.

Two things carry most of it. **Independent work runs on every core** -
inflating a partition's members, reading a record, reading a STEP statement,
tessellating an element, deflating a chunk - through
[`bim_core::work::map_in_order`](crates/bim-core/src/work.rs), which hands the
results back in the order the input came in so a conversion produces the same
bytes on one core and on thirty-two. Everything that decides what to keep still
runs on one thread, in file order. **Allocation is the other half**: the record
walk built three strings per string field whether or not a caller kept one, and
the parse tree of a large IFC cost seconds to free, which is why both binaries
set `mimalloc` as their allocator.

Peak memory rose by about a third - 4.0 GB to 5.3 GB on the 453 MB model -
because a partition is held in memory to inflate its members from, and members
are read a batch at a time. `MEMBER_PREPARE_BATCH` in `rvt-import` is the knob.

## CLI

```bash
cargo run -p groma-cli -- info model.rvt
cargo run -p groma-cli -- streams model.rvt
cargo run -p groma-cli -- dump-stream model.rvt BasicFileInfo --output BasicFileInfo.bin
cargo run -p groma-cli -- partitions model.rvt
cargo run -p groma-cli -- schema model.rvt
cargo run -p groma-cli -- elem-table model.rvt
cargo run -p groma-cli -- partition-id-probe model.rvt
cargo run -p groma-cli -- schema-prefix-probe model.rvt
cargo run -p groma-cli -- marker-envelopes model.rvt --dump 8
cargo run -p groma-cli -- member-framing model.rvt --dump 4
cargo run -p groma-cli -- dump-member model.rvt Partitions/81 44 --output member.bin
cargo run -p groma-cli -- schema model.rvt --class ElementHeader
cargo run -p groma-cli -- records model.rvt
cargo run -p groma-cli -- bodies model.rvt --class ElementHeader --count 8
cargo run -p groma-cli -- element model.rvt <element-id> --bytes 32
cargo run -p groma-cli -- inspect model.rvt
cargo run -p groma-cli -- names model.rvt
cargo run -p groma-cli -- parameters model.rvt --class FamilyInstance
cargo run -p groma-cli -- export-json model.rvt --output model.jsonl
cargo run -p groma-cli -- export-json model.rvt --full --output model.jsonl
cargo run -p groma-cli -- export-ifc model.rvt --output model.ifc
python -m ifcopenshell.validate model.ifc --rules
cargo run -p groma-cli -- export-scene model.rvt --output model.rvs
cargo run -p groma-cli -- export-scene ar.ifc st.ifc mep.ifc --output site.rvs
cargo run -p groma-cli -- export-ifc ar.ifc st.ifc mep.ifc --output site.ifc
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
parameter values, the built-in names its own release publishes,
model-defined parameter specs,
known canonical-unit conversions, deletion/lock flags, and the byte location
it came from. Independently bounds-verified straight pipes additionally carry
a metric swept-disk axis and radius; owner-verified, single-line pipe-fitting
centerlines carry an axis-only metric representation. A diagnostic
`FamilyInstance` placement is included only when one orthonormal
`m_instOrigin`/`m_RefDir`/`m_zAxis` candidate is unique inside independently
recovered owner bounds; it is not yet promoted to IFC. `--full` writes every
decoded section instead of the export's selection: a leading model line - the
release, schema size, partition list, the size of each recovered section and
the class histogram indexing the element lines - and, on every element, the
whole body decoded from its own `GElement` record (each face's plane or
cylinder, its outer loop and holes, and every edge's line or arc), the boxes
that body was checked against, each face and edge the decode could not read
with the reason it gave, and the centerline readings kept as candidates. It is
a superset: the element lines carry the same fields with or without it.
`names` reports where each class keeps its
string and how consistently, so a name read at a calibrated offset can be told
apart from one found by scanning.
`export-ifc` writes an IFC4 Design Transfer View file with
`IfcProject -> IfcSite -> IfcBuilding -> IfcBuildingStorey`, metric units,
deterministic 22-character GlobalIds, and typed elements. The current
conservative mapping covers pipe segments/fittings and sanitary/air/fire-
suppression terminals by category, and the architectural system families by
class alone - `IfcWall`, `IfcSlab`, `IfcRoof`, `IfcStair`, `IfcStairFlight`.
A `RoomElem` becomes an `IfcSpace` named by its room number and called by its
room name, decomposed by the storey it sits on rather than contained in it.
Unknown class/category pairs remain `IfcBuildingElementProxy` instances. Verified straight pipes receive an
`IfcPolyline` axis and `IfcSweptDiskSolid` body. Pipe fittings with one
unambiguous straight `PipeFittingCenterLine` receive an `IfcPolyline` axis but
no invented body. Bounds-verified, right-handed `GInstance` transforms become
storey-relative `IfcLocalPlacement` values; world-space geometry is converted
back into that local frame. A decoded body reaches the file by one of two
verified routes: on its own record, when its extent reproduces that record's
own bounds block and is therefore already placed - which is what a wall, a
floor or any other system family has - or through a family symbol whose box,
carried through the instance's transform, agrees with the instance's own.
Incomplete bodies fall back to a verified box, and other geometry is omitted
rather than approximated.
Project storeys are `Level` records without a family reference; family-local
reference levels are not promoted to building storeys.
By default it includes live categorized records with a recovered level;
`--include-unplaced` also includes categorized records without one. Parameter
arrays are written only when their value kinds agree with the dynamic set
classes referenced by the element and exactly one matching run is present;
legacy unbound results from unsupported releases stay out of IFC. Pass
`--model-namespace <UUID>` to keep IDs stable if the source file moves,
otherwise the canonical RVT path determines the namespace.

The export is configurable the way Revit's own IFC setup is, and the setup can
be saved and reused:

```bash
cargo run -p groma-cli -- export-ifc model.rvt \
    --length-unit millimetre \
    --class-mapping export-classes.txt \
    --write-settings setup.json
cargo run -p groma-cli -- export-ifc other.rvt --settings setup.json
```

`--length-unit` writes every length in metres (the default) or millimetres,
which is what Revit's own export of the corpus writes; areas and volumes stay
metric in both, as they do there. `--no-types` leaves out the `IfcTypeProduct`
behind each element - by default each Revit type becomes one, related to its
elements by `IfcRelDefinesByType`, and the type's parameters are stated on it
once instead of on every element of it. `--no-ifc-common-property-sets` leaves
out IFC's own `Pset_..Common`, which carries `Reference`, the element's type
name: joined to the IFC Revit itself exported from AR S1 on the Revit element
id, that is what Revit puts there for 11 545 of the 11 895 products carrying
one. Nothing else from those sets is written - no parameter this decode
recovers agrees with `IsExternal`, `LoadBearing` or `ExtendToStructure` on any
element where they vary. `--no-revit-property-sets` and
`--no-revit-type-property-sets` leave out the Revit parameters themselves.

`--base-quantities` writes IFC's own `Qto_..BaseQuantities` - Revit's *Export
base quantities*, off by default as it is there. The numbers are measured from
the solid this file carries rather than read from the source, because Revit
computes its own at export time and stores none of them: joining every numeric
parameter the decode recovers against every quantity in Revit's export of AR S1
found no carrier for a single one. Only a closed shell of planar faces is
measured, since a curved face would have to be tessellated and a tessellation
is an approximation nothing here bounds. Measured against Revit's own file on
the 8 047 such solids both hold, 7 482 reproduce its `NetVolume` to within a
thousandth; the 565 that do not are 558 walls whose body we export larger than
Revit exports its own, which is a difference in the body rather than in the
measurement.

`--class-mapping` reads a class mapping table in the tab-separated form Revit's
*IFC Options* dialog reads and writes - a category, an empty subcategory
column, the IFC class to write it as, and the predefined type - and it decides
ahead of the built-in mapping. `Not Exported` keeps a category out of the file
entirely. It is the *export* table, not the import one: Revit ships two files
that look alike, and `importIFCClassMapping.txt` - IFC class, predefined type,
then the Revit category - is the other direction, for reading an IFC in.
Passing that one here says so rather than failing row by row. Two further
things differ from Revit's own export table: the category is named by its
`BuiltInCategory` (`OST_Walls`, or the number `-2000011`), because that is what
a decoded model carries and no display name in any language is recoverable from
the file; and a row naming a subcategory is refused rather than applied to the
whole category. The class and predefined type are checked against the IFC4
schema itself - see `crates/ifc-export/src/ifc4_entities.rs`, generated by
`scripts/generate_ifc4_entities.py` - so a table naming a class that does not
exist, or a kind an entity does not declare, fails before the model is read.

`--settings` reads all of the above from a JSON file and `--write-settings`
saves the setup a run used; a flag beside `--settings` overrides what the file
says. A key the exporter does not have is an error rather than something
quietly ignored. The file also carries what Revit's *Project Address* tab
holds, which has no flag of its own:

```json
{
  "length-unit": "millimetre",
  "class-mapping-file": "export-classes.txt",
  "project": {
    "name": "SRG-DP-RP",
    "long-name": "Residential complex, phase 2",
    "phase": "Detail design",
    "site-name": "Plot 219B",
    "building-name": "Section 1",
    "address-lines": ["Raiymbek 219B"],
    "town": "Almaty",
    "postal-code": "050000",
    "country": "KZ"
  }
}
```

Nothing there is invented: a name left out keeps what the exporter wrote
before - the source file's own stem for the project, `Site` and `Building`
below it - and an address left out is no `IfcPostalAddress` at all, because
none of the three is recoverable from a decoded model today.

The same settings are available over HTTP. `POST /export-ifc?name=<model>` with
the model as the body starts an export and answers with a job to poll at
`/jobs/{job}`; the file is then at `/exports/{name}.ifc`. The query parameters
are the flags by name - `length-unit`, `base-quantities`, `no-types`,
`no-revit-property-sets`, `no-revit-type-property-sets`,
`no-ifc-common-property-sets`, `class-mapping` -
and a parameter this server does not have is a 400 rather than a file that is
quietly not what was asked for.
## Rooms in the viewer

A room bounds volume; it is not a thing you can touch. The viewer draws one as
a faint translucent shell that never hides what stands inside it, and its body
is not clickable at all - a room encloses everything in it, so clicking one
would select the room instead of what is being pointed at.

A room is selected by the ring at the centre of its extent. Rings are
depth-tested in the solid view, so you can select a room where you can see one,
and drawn through everything in X-ray, which is the mode for seeing what is
behind something.

## Federating several files

`export-scene` and `export-ifc` accept more than one source file and read them
as one model:

```bash
cargo run -p groma-cli -- export-ifc ar.ifc st.ifc mep.ifc --output site.ifc
```

Every identifier is qualified by the file it came from - `ar/1G4h...` rather
than `1G4h...` - covering element ids, the level and type each element names,
relation endpoints and the references stored in property values. Without that,
two files that each number an element `1234`, or each call a storey
`level-guid`, would collapse into one. The qualification is applied whenever
there is more than one file, whether or not a collision actually occurred, so
an identifier's meaning never depends on what else was federated with it.

A **single** file is never renamed. Its identifiers, and every IFC `GlobalId`
derived from them, reach the output exactly as its reader stated them.

The scene records one entry per file in `documents`, and each element indexes
the one it came from, so the viewer shows a **Files** panel for a federated
model and hides one discipline at a time.

The server takes a federation as one upload per file under a shared set name,
the last marked complete:

```bash
curl -X POST --data-binary @ar.ifc  'localhost:8800/upload?name=ar.ifc&set=tower'
curl -X POST --data-binary @st.ifc  'localhost:8800/upload?name=st.ifc&set=tower'
curl -X POST --data-binary @mep.ifc 'localhost:8800/upload?name=mep.ifc&set=tower&complete'
```

Each of the first two replies with how many files are held; the last starts one
job that reads them all and answers with it. `POST /export-ifc` takes the same
three parameters. Dropping several files on the viewer's model panel does this
for you.

**Coordinates are not reconciled.** Each file's geometry arrives in whatever
world system that file stated. Files exported from one coordinated project
share a survey point and need nothing done; files that do not are reported -

```text
warning: ar and st state no overlapping geometry, so they are probably about
different origins; nothing was moved
```

- and left alone, because nothing here knows the transform that would
reconcile them. Reading `IfcMapConversion` and true north is the work that
would change that.

`export-scene` writes the binary scene a viewer loads: the same recovered
elements and levels `export-ifc` maps, the triangles their geometry tessellates
to, and their properties. Geometry is tessellated once, in Rust, to a chord
tolerance given in millimetres (`--chord-tolerance-mm`, 4 by default), so a
browser copies buffers to the GPU rather than parsing them. Elements are
ordered along a Morton curve and cut into chunks (`--chunk-triangles`), which
is what lets positions be sixteen-bit offsets inside a chunk's own box and lets
a viewer cull or stream a chunk whole. Properties sit in separate blocks of
`--property-block` elements, fetched only when something is selected. Every
section is raw deflate, which a browser decompresses natively through
`DecompressionStream("deflate-raw")`; the manifest is located by a 24-byte
trailer, so a reader needs two range requests before the first triangle. A face
the tessellator cannot read is counted and reported, never replaced by a box.
The viewer provides perspective and orthographic plan/front/right views, an
X-ray mode with a translucent ghosted shell and depth-tested edges on the dark canvas,
persistent point-to-point metric measurements (including multiple rulers), and
a relationship graph for the selected element. The graph combines the
relations the source file states with the element's recovered level, type,
category, Revit class, and IFC export class, and each related element is a
node you can open to re-root the graph there. From an IFC those relations are
read outright: which wall hosts a door or window (composed from the stated
`IfcRelVoidsElement` and `IfcRelFillsElement` pair, since a file never names
the two directly), which spaces an element physically bounds, and what an
assembly aggregates. Nothing is inferred from geometry, and an edge whose
either end is not an element - an opening, a storey, a group - is dropped
rather than invented. An RVT states no relations yet, so there the graph shows
what the element is. Press `2`/`3` to switch between plan and 3D, `X` to
toggle X-ray, `M` to enter or leave measurement mode, and `F` to frame the
selection or model.

The same page is also the workspace, and it has three screens behind three
routes. `/files` lists every scene the server holds as a card - a preview, the
model's name, whether it came from an RVT or an IFC, the application that
wrote it, and its element and level counts - and can be searched, sorted, and
filtered by раздел. A раздел is named by its own code - АР, КЖ, КМ, ОВ, ВК,
ЭОМ, ГП - and is not a field in any of these files: a name carrying a code
(`..._AR01_...`, `КЖ`, `OV`, `ВК`) is read as the engineer's own answer, and a
name that carries none falls back to what the model actually holds, weighted
by how much of it there is - and then only to the base code of a family,
because a category can say "structural" but never "КЖ rather than КМ". A file dropped on
that screen is converted to a scene and opened in 3D as soon as the server has
read it, because listing models is what someone was doing when they dropped
one. Several models can be ticked and opened together - one tab each, and only
the one on screen fetches geometry - which is how the architecture, the
structure and the services of a building are read side by side; `＋` in the tab
strip goes back for another.

`/convert` is the conversion screen, and it opens nothing. It walks three
steps in order - the files, then the output, then start - because the order is
not ceremony: the output decides which files are even allowed (JSON reads one
RVT), and the summary before the button is the last chance to see what a
conversion that runs for minutes is about to be given. A step that cannot be
honoured is refused rather than half-entered. Confirming starts a numbered
conversion that joins a queue below: how many files it holds, how far
it has come, and how long it has taken. Opening a queue card shows every
source file of that conversion on its own row, each with its own progress, the
stages it has finished with their seconds, what it produced and what it can be
downloaded as. `/viewer` draws the models that are open, one tab each.

What a card shows is read from that scene's own manifest by range, so listing
a 450 MB scene costs the same two short requests as listing a small one, and
the server never inflates a scene to describe it. A preview is rendered by the
viewer itself the first time a model is opened and cached through
`PUT /previews/{scene}`, so it is a picture of the real geometry rather than
an approximation of it.

Being one page is what makes it embeddable: another application mounts a
single URL in an `iframe` and gets the library, the conversions and the 3D
view, with `/viewer?tabs=NAME` linking straight to one model. Every route the
page calls is resolved against the directory the page was served from, so the
whole viewer also works behind a path prefix - proxied at `/groma/viewer`
inside another application - and it degrades rather than breaks where an
embedder denies it history or storage access.

The product shell is written in React (`web/src/viewer-ui.jsx`) and bundled
with Anime.js into `web/viewer-ui.js`; the WebGL reader remains a focused
inline engine in `web/viewer.html`. Rebuild the UI before compiling the Rust
server whenever the React components change:

```bash
npm ci
npm run build:web
cargo build --release -p groma-api
```

Unknown stream bytes are always available through `dump-stream` and the
`rvt-container` API.

## Workspace

Readers, one group per source format:

- `rvt-container`: physical CFB streams and known compression/framing
- `rvt-schema`: generic schema definitions
- `rvt-model`: loss-preserving serialized-object IR
- `rvt-import`: semantic reconstruction of an RVT into `bim-core`
- `revit-catalog`: versioned Revit identifiers and Forge unit conversion
- `ifc-import`: reading ISO 10303-21 into `bim-core`

Shared, and depended on by both sides:

- `bim-core`: format-independent BIM types
- `bim-convert`: what format a file is, and what an element is

Writers:

- `ifc-export`: IFC4 model builder, GlobalId and STEP writer
- `bim-mesh`: tessellation of canonical BIM geometry into triangle meshes
- `scene-pack`: the binary viewer scene format

Applications:

- `groma-cli`: command-line interface
- `groma-api`: HTTP server and the viewer it serves

Every reader ends at `bim-core` and every writer starts there, so a new source
format is a reader crate plus an arm in `Format::sniff` - not a change to any
export.

See [`docs/architecture.md`](docs/architecture.md) and
[`docs/format-notes.md`](docs/format-notes.md) for current boundaries and
evidence.

## Status

groma is experimental, and at 0.9 it carries identifier tables for two Revit
releases, 2023 and 2026, of which only 2023 has been measured against real
models. It recovers the object graph's records, identifiers,
classes, selected fields, level elevations, names, and schema-bound parameter
sets, plus independently checked straight-pipe bodies, straight fitting axes
and boundary representations. On the reference model 56.4% of the products
Revit's own export gives a shape now carry one, every emitted body builds in
IfcOpenShell, and groma never writes RVT files.

The JSON-lines diagnostic exporter and a conservative IFC4 exporter are
implemented. Coordination IFC still requires most element geometry, and legacy
parameter candidates from unsupported schemas stay out of IFC. See
[`docs/architecture.md`](docs/architecture.md).

The live metadata sample is checked with IfcOpenShell's schema validator and
IFC4 EXPRESS rules in addition to the Rust test suite.

## License

groma is free software under the [GNU Affero General Public License v3.0
only](LICENSE).

Section 13 is the clause to read before deploying it: if you modify groma and
let users interact with it over a network — including through `groma-api` or any
service built on these crates — you must offer those users the source of your
modified version.

groma is an independent clean-room implementation. It contains no Autodesk code
and is not affiliated with or endorsed by Autodesk, Inc.; "Autodesk" and "Revit"
are their trademarks, used here only to say which format groma reads. See
[`NOTICE`](NOTICE) for the boundaries this project keeps, including the two that
contributions must respect: no proprietary model content in the repository, and
no decoding of the inter-member protection block.
