# Architecture

Rivet is deliberately split at format boundaries so that uncertain
reverse-engineering work cannot leak into the canonical BIM model.

```text
                  readers                                writers

.rvt bytes -> rvt-container  (CFB, streams, framing)
           -> rvt-schema     (generic type/field declarations)
           -> rvt-model      (serialized objects, unknown bytes kept)
           -> rvt-import     (semantic reconstruction)          \
              + revit-catalog (public names and Forge units)     \
                                                                  >- bim-core -> ifc-export  (IFC4, ISO 10303-21)
.ifc text  -> ifc-import     (ISO 10303-21 -> semantic model)    /              -> scene-pack (viewer scene, via bim-mesh)
                                                                /               -> JSON
             bim-convert     (which format this is; what an element is)
```

Every reader ends at `bim-core` and every writer starts there. `bim-convert`
sits beside both: it answers what a file is, from its own leading bytes, and
what an element is, from the vocabulary its source named it with. Those two
questions used to be answered once per caller - a `looks_like_step` in the CLI
and a private `Format` in the server, a class/category table reached through
the IFC writer - which is what made a second source format a change to every
export rather than a new reader.

Adding a format is therefore: a reader crate that produces a `BimModel`, an
arm in `Format::sniff`, and an arm in the CLI's `read_source`. Nothing in a
writer, and nothing in the server.

Several readers can feed one conversion. `bim_core::federate` assembles their
models into one, qualifying every identifier by the document it came from so
that two files numbering an element `1234` stay two elements. It does not
reconcile coordinates - it reports documents that state no overlapping
geometry and moves nothing.

## Current milestone

`rvt-container` validates the CFB signature before handing the file to the
memory-safe `cfb` crate. It records stream metadata once and opens streams only
for read access. In-memory reads are bounded; callers that need an exact raw
copy can stream it to a writer.

Known gzip framing can be decoded independently of stream semantics. Prefix
bytes at the currently observed offsets are preserved in the result. Unknown
framing is returned unchanged.

Large database streams may contain 65,249-byte stored pages composed of 64,896
payload bytes and 353 checksum/ECC bytes. Stored-stream reads remain byte-exact.
The schema CLI first tries the raw inflation result and strips full-page
trailers only when strict schema parsing fails, so a page-layout assumption can
never replace a successful deterministic parse.

Known database streams require a narrower rule. `Global/ElemTable` is prepared
with checksum-page cleanup before inflation whenever it contains a complete
stored page. Testing showed that raw DEFLATE can otherwise terminate without an
error yet return corrupted output that still resembles a valid record table.

Partition inspection is streaming and bounded. It makes one checksum-clean
pass to locate gzip candidates, then seeks back to validate each independent
member while retaining at most a 16-byte structural prefix and otherwise
discarding decoded bytes after counting them. Member and aggregate
decode ceilings prevent compressed data from forcing unbounded memory or work.
Both byte-exact stored offsets and checksum-clean logical offsets are retained;
no object semantics are assigned at this layer.

An optional experimental scan counts an exact schema-index/zero prefix during
the same streaming pass. The scan is disabled by default, accepts an explicit
inclusive index range, and reports candidates rather than record boundaries.

Member framing is now deterministic. `rvt-container` parses the fixed 40-byte
descriptor that precedes each member and exposes it with the raw bytes intact;
it never repairs a descriptor that disagrees with the member it describes.
`rvt-model` walks a decoded member as length-prefixed records. The strict walk
requires an exact tiling; the continuation-aware walk additionally carries the
outstanding byte count of a record that runs past the member into the next
member of the same partition, and reports what each member received and still
owes. The descriptor's record count and body-byte total are used to check the
walk, never to steer it.

`BasicFileInfo` release extraction is intentionally conservative. Layouts 10,
13, and 14 have explicit readers. An unrecognized layout remains readable but
produces an unknown release rather than a guessed value.

`rvt-schema` deterministically tiles the decoded `Formats/Latest` stream into
generic class and property definitions. It preserves original name bytes,
unknown header words, GUID bytes, unresolved references, index mismatches, and
trailing bytes. It contains no `Wall`, `Door`, or other Revit-specific class
catalog. `bim-core` now distinguishes source-system identifiers, unknown units,
explicitly unit-bearing numbers, levels, and element relationships without
depending on an RVT crate. `rvt-model` recognizes only corpus-backed
`Global/ElemTable` record layouts. It validates an initial run of project-record
markers, reports declared/parsed count differences, and retains the complete
decoded byte sequence so unknown header, record, and trailing fields remain
recoverable. Candidate IDs are an index for future joins; the table does not
provide physical partition offsets.

Record headers are decoded into `RecordHeader`: an identifier, a body length, a
schema class index, and the words whose meaning is not established, which keep
neutral names. Parameter sets are read with the layout the schema declares for
`ParamValueDouble`, `ParamValueInt`, `ParamValueAString` and
`ParamValueElementId`; only where the run starts is searched for. Bodies stay
raw bytes except for that, the strings, and the
`ElementHeader` identifier block, where the category and family reference are
corpus-verified; the neighbouring `ElementId` slots are returned as unverified
values rather than named fields. `LevelFields` additionally reads a schema-
resolved, structurally validated `Plane` and exposes its origin Z as internal
feet. Project/shared parameter definitions expose their Forge spec type ID, so
positive `paramId` values can be joined to a dimension without mistaking
document display units for storage units. The reader resolves the four dynamic
`ParamValueSet*` class indexes, identifies which typed sets are referenced
before `m_id`, and accepts only one post-tail run with exactly those value
kinds and release-validated parameter IDs. The older unbound scan remains
diagnostic-only for unsupported schemas.

## Export roadmap

JSON is the first exporter and IFC is a required target, not an optional one.
The JSON exporter currently lives in the CLI and emits only fields the readers
actually recovered, omitting anything unknown rather than emitting a default.
Its normalization step now creates `bim-core` categories, properties, external
identifiers and unit-bearing numbers; collection will move out of the CLI as
the remaining record-specific normalization moves into `rvt-model`.
The dependency direction is fixed: exporters read `bim-core` only. Element
identity, category, family/type/instance hierarchy, parameters with units,
levels, spaces, and host-child relations must therefore be reconstructed in
`bim-core` in a form that satisfies both outputs, because an IFC exporter that
reaches back into RVT structures would reintroduce the coupling this split
exists to prevent. Geometry stays out of both exporters until the object graph
and semantics are reliable.

Built-in names are external catalog data, not RVT serialization.
`revit-catalog` implements the Revit 2023 and 2026 `BuiltInParameter` and
`BuiltInCategory` tables, selected from `BasicFileInfo.release`: stable enum
names are the machine-readable default and unknown codes retain a numeric
fallback. A release keeps its own tables rather than borrowing a neighbour's -
2026 renames twelve parameters and one category that 2023 spells differently,
and the reading of a code has to be the one its own release publishes. A
release with no tables gets none: `Catalog::for_release` answers `None`, and
the reader says so rather than leaving a thinner model unexplained. It also converts a known Forge spec from Revit's internal base units
to the registry's canonical storage unit. Catalog lookups never erase the
source code or become a prerequisite for lossless parsing.

`ifc-export` implements the buildingSMART 22-character UUID encoding,
deterministic RFC 4122 version-5 identities, STEP references, typed values,
enumerations, lists, finite-number checks, apostrophe/backslash escaping, and
UTF-16 `\X2\` strings. Its high-level builder emits an IFC4 Reference View
graph: project, site, building, metric unit assignment, storeys with recovered
elevations, spatial decomposition and containment, typed elements with a proxy
fallback, and property sets for trusted `bim-core` values. A declarative source
class/category table maps `RbsPipeCurve` pipes,
pipe fittings, plumbing fixtures, duct terminals and sprinklers to the
corresponding format-neutral `BimElementType`; the entity dispatcher emits
`IfcPipeSegment`, `IfcPipeFitting`, `IfcSanitaryTerminal`, `IfcAirTerminal` or
`IfcFireSuppressionTerminal`. Unknown or
unsupported numeric units become labelled values rather than guessed IFC
measures, and a storey without an explicit metric elevation is rejected.

The first geometry slice is deliberately narrow. `rvt-model` accepts a
straight `RbsPipeCurve` only when its schema-resolved curve-driver line and
an outer radius solved from the independently stored `GElement` bounds
reproduce those bounds on every axis. The stored width/diameter is retained as
nominal evidence rather than assumed to be the physical outside diameter. `bim-core`
represents that result as a metric line directrix plus swept-disk radius. The
IFC exporter writes `IfcPolyline` axis geometry and an
`IfcSweptDiskSolid`/`AdvancedSweptSolid` body relative to the element's storey;
an unverified candidate keeps its typed metadata but no representation.
For `PipeFittingCenterLine`, `rvt-model` accepts only a one-item center-curve
collection whose schema-resolved `GLine` has a finite, non-empty unit-direction
domain. Its `GInfo` element tag must resolve uniquely to an
`OST_PipeFitting`, and the helper itself must carry
`OST_PipeFittingCenterLine`; both endpoints must also lie inside the owner's
independently serialized `GElement` bounds. `bim-core` retains this as an axis-only line and
the IFC exporter writes only the `Axis`/`Curve3D` representation: it does not
infer a fitting radius or body from the centerline.

The CLI bridge is intentionally conservative. It selects live categorized
records with a recovered level association, with an option to include unplaced
categorized records. For storeys it selects top-level `Level` records without
a family reference, excluding reference levels contributed by loaded family
documents. It writes source ID, class and category properties and promotes
only schema-bound parameter runs. Known units become IFC measures; unresolved
units become labels rather than guessed measures. Curved and multi-branch
fitting axes, fitting bodies, terminals and general element geometry remain
separate future layers.

The raw `FamilyInstance` frame remains diagnostic. The reader finds the
schema-declared consecutive `m_instOrigin`,
`m_RefDir`, and `m_zAxis` fields, requires unit orthogonal directions, and can
cross-check the origin against a separately serialized near-duplicate bounds
pair. Only 7 of the 75 mapped family instances in the first live export pass
that gate, so treating the raw origin as a project/world placement would be
unsupported. These candidates are visible in JSON but are not used for IFC.

The independently stored `GElement` record provides the project-space bridge:
a schema-resolved `GInstance` contains `InstanceInfo.m_Trf`, represented by a
3×3 basis and origin. Promotion requires one finite orthonormal right-handed
transform after the `GInstance` marker and an origin inside the independent
element bounds. The first live model yields 6,524 verified transforms, covering
66 of 75 mapped family instances. `bim-core` retains world origin, local X and
local Z; the IFC exporter writes a storey-relative `IfcLocalPlacement` and
converts any world-space element geometry back to that local frame.

## Invariants

1. No public API mutates an RVT file.
2. Every bounded allocation has an explicit limit.
3. Unknown bytes remain accessible.
4. Schema and object semantics require corpus-backed tests before promotion.
5. `bim-core` never depends on an RVT crate.
