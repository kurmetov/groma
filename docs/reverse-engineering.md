# Reverse-engineering workflow

Rivet accepts only clean-room, reproducible observations.

## Evidence record

Every proposed decoder change must include:

1. source file and Revit release;
2. exact stream name and raw byte range;
3. observed encoding and competing explanations;
4. confidence level;
5. a regression fixture or a minimal synthetic equivalent;
6. whether a second implementation or independent export corroborates it.

Fixtures must have clear redistribution terms. Private models should be reduced
to non-identifying byte fixtures when possible and must never be committed by
default.

## Promotion rule

A hypothesis can be exposed as a typed semantic field only after a deterministic
parser and test exist. Until then, store the bytes as `Unknown` and report the
offset and source stream. A successful parse must consume the documented record
boundary; searching for plausible strings or numbers does not by itself prove
ownership or meaning.

## First corpus pass

For every candidate file, record:

```text
sha256, file size, Revit release, stream count, partition count,
known stream presence, raw and decoded stream sizes, parse errors
```

The Phase 1 exit criterion is successful `info` and `streams` execution on
multiple releases plus negative tests for truncated, malformed, and oversized
containers.
## Experimental probes

Experimental correlations must remain visibly separate from accepted format
semantics. `rivet partition-id-probe model.rvt` compares each independently
inflated partition member's first little-endian `u32` with candidate IDs from
`Global/ElemTable`. It reports aggregate overlap only. A low overlap refutes
the simple one-member-per-element hypothesis; a high overlap is evidence for
further study, not by itself proof of field semantics.

`rivet schema-prefix-probe model.rvt` resolves one exact class name (default:
`GElement`) through the strict schema and counts byte windows shaped as
`[class index:u16][zero:u16]` while partition members are already being
stream-decoded. Restricting the probe to an independently supported root marker
avoids treating every embedded schema type as a possible record boundary. It
still reports candidates, not objects: field ownership requires stronger
validation.

`rivet marker-envelopes model.rvt` keeps the schema-resolved marker of one
exact class and stores a bounded byte window around every candidate together
with its partition, member index, and offset inside that member's inflated
stream. Capture stays streaming and budgeted: context is limited to 4,096
bytes per side and to a fixed number of envelopes per partition, and
candidates beyond that budget are still counted, so a truncated capture is
visible instead of silently narrowing the evidence. The command prints the
distributions a record boundary would have to explain: leading eight-byte
patterns, distances between consecutive candidates inside one member, the
post-marker `u32` and its overlap with `Global/ElemTable` IDs, and whether the
marker is one element of a strictly ascending `u32` run at stride 4, 8, 12,
or 16.

### Result: `GElement` markers are not record boundaries

Observation. On a local three-model Revit 2023 project corpus the captured
envelopes place 40%, 88%, and 85% of candidates inside an ascending `u32` run,
with runs at every tested stride. No candidate gap dominates: the most common
distance holds 26%, 30%, and 11% of the sampled gaps per model, and leading
eight-byte context is highly variable (332 distinct patterns in 550 envelopes
for the smallest model).

Hypothesis. `[GElement index:u16][zero:u16]` delimits serialized element
records inside partition members.

Experiment. Capture bounded envelopes for every candidate and test the
neighbourhood for record-like regularity.

Result. Refuted at this granularity. The `GElement` index is a small integer,
so the four-byte pattern mostly matches ordinary counter or identifier data
inside numeric tables. The post-marker `u32` overlaps the `ElemTable`
candidate-ID set for 10-53% of samples depending on the model, which is
consistent with those bytes being neighbouring identifiers in such a table
rather than a field owned by a record header.

Confidence. Medium for the refutation, based on three private project models
of one release; no synthetic fixture can settle it, so the deterministic
capture path is covered by tests while the corpus numbers stay local.

## Member framing

`rivet member-framing model.rvt` validates two structures at once and reports
only what it could confirm: the fixed descriptor in front of every compressed
member, and the record array inside the decoded member.

### Result: a 40-byte member descriptor

Observation. Members inside a partition stream are never adjacent. Across a
local three-model Revit 2023 project corpus every gap between the end of one
member and the start of the next is exactly 40 or 228 bytes, and every
partition's first member starts at byte 44.

Hypothesis. A fixed-size descriptor precedes each member and declares its size.

Experiment. Read the 40 bytes before each member and test candidate fields
against the sizes the inflater actually measured.

Result. Confirmed for the size fields. `+24` equals the member's gzip bytes
plus 16 for 34,077 of 34,077 members. Where the gap is 40 bytes, `+4` equals
the previous member's decoded size and `+10` equals the previous descriptor's
`+24` in every case, so descriptors form a backward chain. The 228-byte gaps
carry an additional 188-byte block, containing a high-entropy body and the
UTF-16 text "Data generated", whose purpose is unknown; the last 40 bytes of
such a gap are still a normal descriptor, but its back-pointers do not refer to
the immediately preceding member. `+14` is `0x0E4E` throughout, `+8` is
`0x0E47` except in a partition's first descriptor, `+18` and `+36` are zero,
and `+0` varies per member with an unknown derivation.

Confidence. High for the sizes and the chain; the unnamed words stay unnamed.

### Result: length-prefixed records inside a member

Observation. `+32` of the descriptor takes only the values 101, 102, and 103,
and `decoded size - (+28)` is an exact multiple of `+20` with a quotient of 12
for tag 101 and 16 for tags 102 and 103.

Hypothesis. The member is a sequence of records, each a fixed header of that
width plus a variable body, with no separators.

Experiment. Walk the payload as length-prefixed records for every candidate
header width and length-field position, and accept only a walk that consumes
the payload exactly and produces the declared record count.

Result. Exactly one layout per tag survives: tag 101 uses a 12-byte header with
the body length as the `u32` at `+4`, tags 102 and 103 use a 16-byte header
with the body length as the `u32` at `+8`. 30,375 of 34,077 members tile
exactly on their own; the rest fail with a body that runs past the payload and
are concentrated at the 128 KiB member size.

Confidence. High for members that tile, since two independent descriptor fields
predict the walk's output.

### Result: records continue into the next member

Observation. Every walk failure is a body that runs past the end of a member at
or near 128 KiB, which is the largest decoded member in the corpus.

Hypothesis. A record body continues in the next member of the same partition,
so the outstanding byte count must be carried forward.

Experiment. Thread the outstanding count between members in partition order:
each member's walk starts after the carried tail and reports what it still
owes. Confirmation requires that a member ending inside a record is always
followed by one that accepts exactly that carry.

Result. Confirmed. All 34,077 members walk without an error, the recovered
record count equals `+20` for every member, and 2,377 members end inside a
record while exactly the same number resume with the matching carry; no carry
is ever dropped. 7.59M records are recovered with no heuristic anywhere in the
path. One detail stays unexplained: a continuation shifts exactly four body
bytes from the receiving member to the member that handed the record on, so
`+28` needs a `+4`/`-4` correction at a boundary. With that correction `+28`
matches for every member in all three models, including members that both
receive a tail and hand one on, where the corrections cancel.

Confidence. High for the framing. The four-byte correction is measured on the
whole corpus but not explained, and is documented as such rather than folded
into a field name.

## Record headers and the element bridge

### Result: the record header carries an element identifier and a class index

Observation. Record leads ascend inside 99.6% of members, which is what a
sorted key looks like.

Hypothesis. The first `u32` of a record header is an element identifier, and
one of the remaining words is a schema class index.

Experiment. Cross-check every lead against the `Global/ElemTable` candidate-ID
set, and try to resolve each remaining header word against the decoded schema.

Result. Confirmed. The lead `u32` covers 99.998%, 99.858%, and 100.000% of the
`ElemTable` IDs in the three project models. The trailing header word resolves
as `[class index:u16][unknown:u16]` for 99.76-99.79% of all 7.59M records, and
the same rule holds for all three descriptor format tags, which is why it is
read as a `u16` pair rather than as a `u32`.

Each element is written as exactly three records, one per format tag. Tag 101
is always `ElementHeader`. Tag 102 carries the element's own class, and its
distribution is a plausible model inventory: `FamilyInstance`, `CurveElem`,
`RbsPipeCurve`, `PipeFittingCenterLine`, `SketchPlane`, `CategoryElem`. Tag 103
carries `GElement` or `SerializedDummy`. This also explains the earlier
`GElement` refutation: the class does appear as a record class, just never as a
byte-pattern record boundary.

Confidence. High for the identifier and the class index. The companion `u16`,
the wide header's `+4` word, and the record body remain undecoded, and the
tag-to-role mapping is evidence from one release.

### Result: category and family in the `ElementHeader` body

Observation. `ElementHeader` bodies share a fixed opening: two zero bytes, a
word that is `0xffffffff` for most records, then a long run of `0xff`.

Hypothesis. The body starts with the six `ElementId` properties the schema
declares for the class, four bytes each, `-1` meaning unset.

Experiment. Scan every byte offset in 60,000 bodies for two independent
signals: values inside the Revit category range, and values present in the
`Global/ElemTable` candidate-ID set.

Result. Both signals are single sharp peaks at the offsets the schema order
predicts. `+2` holds category-range values for 19.1% of records with 0% at the
neighbouring offsets, and `+6` matches `ElemTable` for 25.0%, again with 0%
next to it. Across the three models 17.9-21.6% of headers carry a category and
79.4-90.6% a family reference, with 269, 830, and 1,423 distinct category
codes. `rivet element <id>` now reports both.

Confidence. High for `+2` and `+6`. The four later `ElementId` slots are read
at the alignment the schema implies but show no independent signal, so they are
exposed as unverified values. The rest of the body, including the six `f64`
around `+48` that look like a bounding box, is untouched.

### Result: the `Element` tail after the `m_id` anchor

Observation. Bodies of different classes place `m_id` at different offsets
(`+34` for `CategoryElem`, `+38` for `CurveElem`, `+42` for `FamilyInstance`),
and the bytes before it are runs of `00 00` interleaved with
`ff ff ff ff` plus a `u16` that always resolves to a schema class index.

Hypothesis. The body opens with a variable-length block of object pointers, and
everything after `m_id` follows the schema's declared field order with fixed
widths: `ElementId` as four bytes, `Bool` as one.

Experiment. Anchor on the identifier the record header already provides, read
the seven `ElementId` slots and three flags the schema declares after `m_id`,
and check each slot against `Global/ElemTable`.

Result. Confirmed for the tail. On 4,000 `FamilyInstance` bodies the anchor is
unique, `m_assocLevelId` is set for 80.3% with only 50 distinct values of which
97.4% are known IDs, `m_createdPhaseId` collapses to a single value,
`m_demolishedPhaseId` is always unset, and the three flags are always 0 or 1.
Level coverage across classes behaves the way a BIM model should: 89.2% of
`FamilyInstance` carry a level and none of `GStyleElem`, `CategoryElem`, or
`FontElem` do.

Confidence. High for the tail; the pointer block before `m_id` stays undecoded,
which is the current limit on coverage.

### Result: the pointer block can be walked, which removes the search anchor

Observation. The bytes before `m_id` are runs of `00 00` interleaved with
`ff ff ff ff` plus a `u16` that always resolves to a schema class index.

Hypothesis. A null pointer occupies two bytes and a reference six, so the block
can be walked without knowing where it ends.

Experiment. Walk the block by that rule on ten classes and check where the walk
stops relative to the `m_id` offset found independently.

Result. The walk stops exactly four bytes before `m_id` for 97.4-100% of 8,000
records, so the reader now reaches the identifier by walking and validates it
against the identifier the record header already carries, instead of searching
the body for it. Across three models the walk succeeds for 90.6-92.1% of 2.5M
element bodies; the remainder still use the bounded search. The number of
fields the walk yields varies within one class, so the block is not yet mapped
onto the schema's property list, and the four-byte word before `m_id` is
unexplained.

Confidence. High for the walk and the anchor; the block's field semantics are
open.

### Result: names are length-prefixed UTF-16 inside the body

Observation. A `Level` body ends its `Element` tail and continues with a
`u32` whose value is exactly half the number of bytes that follow before the
next field, and those bytes decode as the level's name in UTF-16.

Hypothesis. Strings inside record bodies use the same `[count:u32][UTF-16LE]`
encoding already verified in `BasicFileInfo` and `Global/PartitionTable`.

Experiment. Read that encoding at the end of the `Element` tail across classes,
then scan the body when the fixed position yields nothing, and check what the
recovered strings are.

Result. Confirmed. Level names, family names, type names, category names,
parameter names, font names and pipe type designations all read back correctly
from their respective classes. The recovered strings are model content and are
not reproduced here. Bodies also hold unit identifiers of the form
`autodesk.unit.unit:meters-1.0.0`, which is the first sign that parameter
values and their units are reachable.

Where the string sits is class-dependent: 106 of 271 classes place their first
readable string at a single offset in at least 90% of their records, while the
rest hold it behind variable-length fields. The exporter therefore reads the
settled offset when a class has one and scans otherwise, marking which path was
used. Chance decodings are filtered by requiring at least two characters, only
name-like characters, and a single non-ASCII block.

Confidence. High for the encoding. The position is calibrated from the corpus
rather than derived from the schema, so a scanned name is evidence, not a
decoded field, and the export says which it is.

### Result: parameter values

Observation. `Element` declares four pointer properties named
`m_pParamValueSetDouble`, `m_pParamValueSetInt`, `m_pParamValueSetAString` and
`m_pParamValueSetElementId`, and the pointer block of a `FamilyInstance` body
holds references to classes 2980 (`ParamValueSetInt`) and 2977
(`ParamValueSetAString`) in exactly those slots.

Hypothesis. The sets are serialized later in the body, each as `[count:u32]`
followed by entries whose layout the `ParamValue*` classes declare.

Experiment. Take the schema layouts (`[value:f64][paramId]`,
`[paramId][value:i32]`, `[paramId][string]`), then look for a run of sets whose
identifiers are either built-in codes or identifiers of elements whose class
name starts with `Param`.

Result. Confirmed. In one verified element an integer set of one entry is
followed immediately by a text set of two, whose second identifier resolves to
a `ParamElemExternal` in the same model, giving that parameter its name.
Across the corpus this recovers 235,704 / 418,659 / 808,834 values; in the
smallest model 142,753 doubles, 83,207 integers, 6,216 strings and 3,528
references. The recovered names are project-specific shared parameters and are
model content, so they are not reproduced here.

Follow-up. The first guard — requiring two consecutive sets or three
parameters — turned out to be the wrong instrument: `Level` and `RbsPipeCurve`
store exactly one set with one entry, so they were dropped entirely. Measuring
163,260 recovered codes showed 99.6% inside `-1_200_000..-1_152`, so the guard
was replaced by a narrow built-in window (`-2_000_000..-1_000`). Coverage went
from 8.7% to 27.1% of elements and from 174,547 to 235,704 values in the
smallest model, while the out-of-cluster tail that a chance match produces fell
to 0.00% — 2 values out of 235,704. Per class: `Level` and `RbsPipeCurve` 100%,
`MaterialElem` 97%, `FamilyInstance` 94%, `CategoryElem` and `FontElem` 0%.

Confidence. High for the entry layout, which is read straight from the schema.
The run's position is still found by scanning, so a body whose parameters sit
behind a chance match could in principle be misread; the narrow code window and
the all-entries-must-validate rule make that unlikely but do not exclude it.

### Result: level elevation is the Z origin of the datum plane

Observation. `Level` declares no elevation value of its own;
`m_roomComputationElevationOffset` is a separate room-calculation adjustment.
Its `DatumPlane` parent instead owns `m_pFace` and `m_pSurface`, and the dynamic
surface class resolves through `Formats/Latest` to `Plane`.

Hypothesis. Revit derives `Level.Elevation` from `Plane.m_origin[2]` rather than
storing it in the element's parameter sets.

Experiment. Resolve the `Plane` class index from each file's schema, locate
that dynamic class marker in every `Level` body, then parse the inherited
`Surface` envelope/orientation fields and the `Plane` origin/X/Y vectors. A
candidate is accepted only when all numbers are finite, the orientation flag
is boolean, and the X/Y axes are orthonormal; zero or multiple candidates are
rejected. Convert origin Z from Revit's internal feet to metres only for the
corpus report, not while reading the RVT value.

Result. Confirmed on all 1,335 `Level` bodies in the three-model Revit 2023
corpus: 269/269, 293/293 and 773/773 bodies contain exactly one valid plane.
The named project floors form the expected metric sequences after multiplying
by 0.3048 (for example 0, 3.3, 6.3, ... metres in one model), including the
negative basement elevation. `rvt-model::LevelFields` now exposes the original
value explicitly as `elevation_feet`, and JSON exports it as
`elevation_internal_feet`.

Confidence. High for this release and corpus. The class index is schema-
resolved rather than hardcoded, and the full serialized plane shape provides
an independent structural check beyond a plausible floating-point number.

### Result: parameter values link to specs, not directly to display units

Observation. A double entry in a `ParamValueSetDouble` contains only
`[value:f64][paramId]`. Positive IDs resolve to `ParamElem` records. Their
`m_pParamDef` objects carry length-prefixed Forge type IDs such as
`autodesk.spec.aec.structural:massPerUnitLength-1.0.0`. Separately,
`Global/Latest` contains a Forge registry with 350 unit definitions and 161
spec definitions in the smallest model; the registry's spec JSON names
applicable and canonical storage units. The document schema also declares
`AUnits.m_formatOptionsMap` as `ForgeTypeId -> FormatOptions`, with
`FormatOptions.m_unitTypeId` describing display formatting.

Hypothesis. The semantic chain for a project/shared value is
`ParamValue.paramId -> ParamElem.m_pParamDef -> spec`; project display units
are a separate spec-to-formatting choice and do not describe the raw double's
storage.

Experiment. Scan only parameter-definition bodies for a single structurally
valid length-prefixed `autodesk.spec.*` identifier and join it back to stored
values by the already verified positive parameter element ID. Compare the
roles declared by `ParamDef`, `ParamDefValue`, `AUnits`, and `FormatOptions` in
the decoded schema.

Result. Confirmed for genuine project/shared parameters. `revit-catalog`
captures 151 version-insensitive measurable spec keys from the Revit 2023
registry. It recursively resolves primitive, derived, factor-prefixed and
absolute units and applies Revit's internal bases (feet for length; radians,
kilograms, seconds, amperes, kelvin and candelas for the remaining bases). JSON
emits the raw double plus `storage_value`, Forge unit ID and unit name only when
the spec is known. Unknown values remain raw.

The first full-model conversion audit also found a counterexample to the
original parameter-set scan: a `Family` body can contain a list of positive
`ParamElem` references that mimics a counted value array. The replacement
reader resolves the dynamic `ParamValueSetDouble/Int/AString/ElementId` class
indexes from `Formats/Latest`, finds which of those classes are referenced
before the element's `m_id`, and accepts only one post-tail run containing
exactly those value kinds with release-validated IDs. This corrected a real
misclassification from `int=1` to the schema-declared string `"3"`.

On a complete 2023 model the schema-bound reader recovered 197,315 values on
67,751 owners. No `-1`, unknown negative code, fallback parameter name, extreme
double, or ambiguous run survived; 1,714 project/shared doubles had specs and
were converted to canonical units. Results from the retained unbound fallback
for unsupported releases remain diagnostic and are not promoted to IFC.

The same body-level audit on all three corpus models accepted 71,398, 105,704
and 237,074 bodies respectively, recovering 202,842, 361,874 and 850,846
values. That is 1,415,562 schema-bound values in total.

Built-in negative parameter names resolve through the public Revit 2023
`BuiltInParameter` catalog. Their data type/spec is a separate issue. Autodesk
documents that
[`Definition.GetDataType()`](https://help.autodesk.com/cloudhelp/2026/ENU/Revit-API-MainReference/files/html/1c008d27-9e61-362c-308c-8b718ee0f8df.htm)
returns the concrete parameter's
data type and that some built-ins even return an empty type. The public enum
table itself does not contain that mapping, and built-ins have no `ParamElem`
record to join inside the model.

Confidence. High for conversion once a spec is established and for a run bound
to its schema-resolved set classes; low for the legacy unbound scan. Unit
conversion is never inferred from a nearby unit string or an English parameter
name.

### Result: release-specific names are external catalog data

The Revit 2023 catalog is selected only when `BasicFileInfo` reports release
2023. It contains 3,439 unique `BuiltInParameter` codes with English labels and
all 1,189 `BuiltInCategory` codes. Alias enum members sharing one integer are
collapsed to a deterministic canonical name. Fifteen obsolete/internal
parameter members that disappeared before Autodesk published numeric-value
tables are omitted instead of assigned guessed integers. Unsupported releases
and unknown codes retain `param_<number>` / numeric fallbacks.

The checked-in tables are generated by
`scripts/generate_revit_2023_catalog.py`. Membership and labels come from the
[Revit 2023 API reference](https://www.revitapidocs.com/2023/fb011c91-be7e-f737-28c7-3f1e1917a0e0.htm);
numeric values are intersected with Autodesk's published
[BuiltInParameter](https://help.autodesk.com/cloudhelp/2026/ENU/Revit-API-MainReference/files/html/fb011c91-be7e-f737-28c7-3f1e1917a0e0.htm)
and
[BuiltInCategory](https://help.autodesk.com/cloudhelp/2026/ENU/Revit-API-MainReference/files/html/ba1c5b30-242f-5fdc-8ea9-ec3b61e6e722.htm)
tables. The generator reads the Forge registry from an extracted 2023
`Global/Latest` stream; model content is not copied into the generated source.

### Result: IFC identity and STEP syntax no longer block the exporter

`ifc-export` encodes a 128-bit UUID with buildingSMART's published
[22-character IFC alphabet](https://technical.buildingsmart.org/resources/ifcimplementationguidance/ifc-guid/).
When Revit `UniqueId` is unavailable, callers can derive an RFC 4122 version-5
identity from an explicit model namespace and the source element ID. Reusing
the same namespace makes repeated exports stable; different model namespaces
prevent equal element numbers in different files from colliding.

The same crate writes complete ISO 10303-21 envelopes and checked entity
syntax, including references, lists, typed values, finite real numbers and
Unicode names. Its metadata builder now supplies the IFC4 project/site/
building/storey hierarchy, metric `IfcUnitAssignment`, spatial containment,
typed MEP elements with a building-element proxy fallback, and property sets
for trusted canonical values. The CLI derives a default model namespace from
the canonical RVT path or accepts an explicit UUID for path-independent
repeatability.

The first live model contains 269 structurally valid `Level` records, but 257
of them carry a family reference and have names such as "Ref. Level" or the
localized equivalent. The 12 top-level records without a family reference form
the expected project sequence from basement through roof. The CLI therefore
promotes only the latter to `IfcBuildingStorey`.

A full live export exercised those 12 project levels, 292 elements
and 509 recovered Revit properties. The source mapping produced 77
`IfcPipeSegment`, 48 `IfcPipeFitting`, 7 `IfcSanitaryTerminal`, 4
`IfcAirTerminal`, 16 `IfcFireSuppressionTerminal`, and retained 140 unknown
pairs as `IfcBuildingElementProxy`. All 77 pipe segments carry a recovered
straight axis and outer radius whose analytic swept-disk envelope independently
matches the element's duplicated `GElement` bounds. They are emitted as
`IfcPolyline` axes and `IfcSweptDiskSolid` bodies. Of the 48 pipe fittings, 31
also have an unambiguous one-line `PipeFittingCenterLine` whose `GInfo` tag
resolves uniquely back to the fitting and whose endpoints lie inside the
owner's independent `GElement` bounds; those receive an axis-only
`IfcPolyline`, without an inferred radius or body. Across the complete source
object set, 6,737 pipe records have a structurally valid straight-line
candidate, 6,735 also have parseable `GElement` bounds, and 6,510 reproduce
those bounds within `1e-8` feet. The two without bounds and 225 mismatches stay
geometry-free. The bounds check also caught a semantic distinction: an `Ø40`
pipe stores 40 mm as its nominal width/diameter while its physical outside
envelope is 48 mm, so the exporter derives the outer radius from the bounds
rather than silently treating the nominal value as geometry.

The earlier metadata-only result contained 2,634 STEP entities with
sequential entity numbers and no dangling references; all 607 rooted entities
and relationships had unique 22-character GlobalIds. Its typed values include
77 metric `IfcLengthMeasure` values; unresolved dimensions are labels rather
than guessed measures. IfcOpenShell 0.8.5 reports no schema or EXPRESS rule
violations. After adding the 77 pipe bodies and 31 fitting axes, IfcOpenShell
constructs all 108 represented elements when curve dimensionality is enabled
and reports no schema or EXPRESS rule violations.
Legacy parameter candidates and unverified geometry remain absent by design.

The next placement probe resolved the three fixed `FamilyInstance` fields
`m_instOrigin`, `m_RefDir`, and `m_zAxis` and required orthonormal axes plus a
unique origin inside a separate `GElement` bounds pair. Across the complete
model, 9,940 family instances contain a structural candidate and 573 pass the
uniqueness/bounds gate, but only 7 of the 75 mapped family instances selected
for IFC pass. The raw frame is therefore exposed only by the diagnostic JSON
export; no IFC placement is inferred until the family/project coordinate-space
relationship is independently established.

That relationship is available in the separate geometry record. A
schema-resolved `GInstance` embeds `InstanceInfo`, whose inherited
`InstInfoBase.m_Trf` stores a 3×3 basis followed by a project-space origin. A
second decoder requires a unique finite orthonormal right-handed transform and
checks its origin against the element's independent bounds. It recovers 6,524
transforms across the model and covers 66 of the 75 mapped family instances in
the default export. Those transforms now become storey-relative
`IfcLocalPlacement` values. Existing axes are converted from world coordinates
to their element-local frame; IfcOpenShell reconstructs all 108 world shapes
with every represented axis endpoint matching the pre-placement reference
export in world coordinates within `1e-9`.

## Three entangled readings in the record walk

### Result: the walk had three compensating two-byte errors, and each one measured worse alone

Observation. Face-bearing `GElement` records tiled exactly 88.0% / 84.8% /
71.9% of the time on SMALL / MEDIUM / BIG, and `rivet brep` showed that
97.5% / 99.97% / 99.8% of every excluded B-Rep face sat in a record that had
not tiled. The framing, not the geometry, was the whole of the gap. One
misread was located by hand: record 635645's tenth face named a first loop
whose `GInfo` header read `ffffffff 00000000 ffffffff 0400 0800`, where the
walk took `m_flags` as the two bytes `0400` because the lead word's top bit
was clear, and `m_nextLoop` then read as identifier 3276808 of class 0 rather
than identifier 50 of class 1311, an `EdgeLoop`.

Hypothesis. `GInfo.m_flags` has a width discriminator somewhere in its header
that the top-bit reading is missing.

Experiment. Label the width without assuming any rule. For a node whose first
declaration after the inherited `GNode.m_GInfo` is a reference whose target
class the schema fixes - `GEdgeLoop.m_nextLoop` and `GFace.m_pFirstLoop`, both
naming an edge loop - read six bytes at both candidate offsets and keep the one
that lands on a live reference of that class: one class index out of 4 418, so a
two-byte misread has no way to pass. Then cross-tabulate the label against every
candidate the header carries. `rivet flags-probe` is that instrument.

Result. The hypothesis is refuted, and there is nothing to find: across 73 354
labelled sites on SMALL and 123 473 on BIG, **every** one proves four bytes and
not one proves two. Within `EdgeLoop`, `m_tag`, `m_controlCommand`,
`m_categoryId` and both words of `m_flags` are byte-identical between the loops
the top-bit reading handled and the 711 it did not - `0xffffffff`,
`0x00000000`, `-1`, `0x0004`, `0x0008` in both groups. The lead word's top bit
is an ordinary flag, set on `Face` and `Edge` and clear on `EdgeLoop` and
`GFilling`, which is why a width rule built on it looked right.

The reason "read every `GInfo.m_flags` at four bytes" had previously explained
no record at all is that two other readings were absorbing the error. Under the
old rules a null `GEdgeLoop.m_nextLoop` cost six bytes, so two-byte flags plus a
six-byte null and four-byte flags plus a four-byte null both consumed the same
eight bytes and both tiled - the coincidence that hid the width. The raw words
show which is right: in all 67 691 loops of an exactly-tiled record on SMALL the
bytes read `0004 0008 0000 0000 <m_pFace>`, so at two bytes `m_nextLoop` is the
nonsense `id=8 class=0`, harmless only because class 0 does not resolve, while
at four it is a null identifier followed directly by `m_pFace`. Likewise a
`GFace` naming a filling had been given two extra unexplained bytes, and
`m_faceFlags_v9` had been skipped as a version-gated property; four bytes of
written `m_faceFlags_v9` plus two four-byte nulls is exactly two six-byte nulls
plus the two-byte skip.

All eight combinations of the three readings were measured on BIG's
face-bearing records. Each alone takes 71.9% to 0%, as do two of the three
pairs; the third pair reaches 21.9%. The three together reach **100%** -
13 886 / 7 712 / 10 534 of 13 886 / 7 712 / 10 534 across the corpus. Confidence
high: the change is a net deletion of three special cases, the width instrument
finds no site where the reading it takes disagrees with what the bytes prove,
and the 60-pair regression sweep moves no class down.

What is not established is how to interpret `GInfo.m_flags`. Its four bytes are
accounted for, but nothing in the corpus separates "a four-byte flags field"
from "a two-byte flags field followed by two undeclared bytes", because no
independent reading of the value exists to check it against. The walk reads the
declaration as declared and does not interpret the value.

## Attaching decoded bodies to elements

### Result: three verification gates, not the decode, were withholding the geometry

Observation. `rivet brep` resolved 12 330 / 7 222 / 8 945 bodies whose every
face came out, but the IFC export carried 8 / 1 / 43 closed solids and SMALL's
JSON put B-Rep faces on 136 elements. The bodies were decoded and not reaching
any element.

Hypothesis. The element -> symbol -> body -> placement hop is failing, and the
failure is in one of its gates rather than in the decode.

Experiment. Instrument the hop as a funnel, counting the instances that survive
each gate in turn, and report it from `rivet export-ifc`
(`report_symbol_link_funnel`). Then, for the gate that loses the most, measure
what it is actually rejecting rather than assuming.

Result. 6 956 / 2 549 / 571 instances name a symbol that carries a decoded
body - the bodies were one hop away throughout. Three gates stood in front of
them.

The first was category equality between instance and symbol, which took SMALL
from 6 956 to 416. Cross-tabulating (instance category, symbol category) over
the links the *bounds* cross-check accepts settled what it was doing: every
pair where both sides carry a category is equal - on BIG every accepted link
without exception - and every rejection is a case where one side's category was
not recovered. So the gate never fired on a real disagreement. Requiring only
that the categories not *disagree* took verified links from 186 / 50 / 157 to
2 677 / 1 972 / 354. The bounds cross-check is what carries the verification on
its own: all six coordinates of the instance's independently decoded box must
agree with the symbol's box carried through the instance's rigid transform to
within 1e-8 feet, which chance does not do.

The second was that 6 576 of SMALL's 7 014 placed family instances carry no
category at all, and the exporter drops an element with no category, body and
all. An instance now inherits its verified symbol's category - what Revit means
by a family instance's category, and what the cross-tabulation above confirms
independently - recorded as `category_source: "symbol"` in the JSON so it is
never confused with a declared one.

The third was that the exporter refused geometry to any element whose category
it could not map to an IFC type. Classification and geometric verification are
independent questions, and `IfcBuildingElementProxy` carries a shape
representation, so that exclusion is gone.

Together: SMALL goes from 168 to 2 586 elements with geometry and 8 to 695
closed `IfcAdvancedBrep` solids; MEDIUM 1 to 40; BIG 43 to 68.
`ifcopenshell.validate --rules` stays clean on all three and `create_shape`
builds every emitted body, 772 / 40 / 68.

One reading was deliberately refused. An incomplete body - a record where only
some faces resolved - is schema-valid as an open `IfcShellBasedSurfaceModel`,
and at the previous scale all 40 of them built. At corpus scale they do not: of
SMALL's 1 771 incomplete shells IfcOpenShell builds 831 and fails on 940, while
all 695 complete bodies build. Emitting a body the reference kernel refuses is
not a result, so an incomplete body falls back to the symbol's verified box.
Confidence high for the three gates, which are measured and independently
cross-checked; the fallback is a deliberate conservatism, not a finding.

### Result: BIG's "missing placement" was an export gate, not a placement failure

Observation. BIG's 53 exported bodies all geometrized but their composed world
origins spanned 0.05 x 0.19 x 0.00 m over 10 distinct positions - one pile at
the world origin - while SMALL's spread 35.5 x 12.5 x 7.5 m over 71. The
placement chain was three levels of identity and never reached a storey,
although the file carries 21 `IfcBuildingStorey` entities with correct
elevations.

Hypothesis. BIG's `GInstance` transforms are not being decoded, or are decoded
as identity.

Experiment. Read the decoded transform origins straight out of the JSON export
for both files, before any exporter logic touches them.

Result. The hypothesis is refuted and the transforms were never the problem:
843 of BIG's 1 031 transforms carry a non-zero origin (81.8%) and they span
108 x 141 x 33 m, a real building footprint. What was wrong is which elements
reached the export. Of BIG's 1 031 placed instances 461 have a recovered level
and 658 have a category, but only **88 have both**, and the export candidate
filter required a category *and* (by default) a level. The 53 that got through
were an unrepresentative slice that happened to sit near the origin, so the
file looked as though placement had failed everywhere.

Admitting an element with verified geometry without a recovered level - it is
contained in the building rather than a storey, which is what
`--include-unplaced` already did for every element - gives BIG 392 elements
with geometry over 268 distinct origins spanning 107 x 17 x 33 m, and MEDIUM
1 989 over 1 971 origins spanning 192 x 217 x 29 m, up from 40. Confidence
high: each placement is individually verified by the same 1e-8 foot bounds
agreement, `ifcopenshell.validate --rules` stays clean on all three files, and
`create_shape` builds every emitted body (772 / 1 721 / 174).

The pattern is now established well enough to state as guidance: every geometry
shortfall investigated in this project has turned out to be a verification or
selection gate in the exporter, not a gap in the decode. Measure the funnel
before touching the decoder.

### Result: a 50-file corpus, and the first ground truth we have ever had

Observation. Seven archives arrived holding 50 real `.rvt` files across four
disciplines - AR (architecture, 14 files), KJ (structural, 10), ВК (plumbing,
13), ОВ (HVAC, 13), 11 GB inflated, all Revit format 2023, build
`20250724_1515`. Two of them came with something the project has never had: the
IFC that **Revit itself** exported from the same models,
`AR_S1.ifc`, written by
`Autodesk - Revit 26.4.0.32` through `ODA SDAI 25.12`. That is a reference
answer, not another reading of ours.

The corpus is also the first time the reader met architecture at all: the three
files it grew up on are ВК, ЭОМ and КЖ.

Result, the parts that held. All 50 files open as CFB/OLE and report their
release. `inspect` resolves a class for 99.75-99.88% of records on every file,
anchors 90.6-93.0% of element bodies by the pointer block, and finds
94.1-100.0% of `Global/ElemTable`'s identifiers - on files up to 400 MB and
2.0 M objects, in 1-8 s at 28 MB resident, so the streaming design holds at
five times the old corpus's size.

Two defects fell out, both now fixed with tests:

- **All 10 KJ files panicked.** `inspect` and `element` sliced
  `payload[record.body_offset()..record.end()]` raw, and a record may be
  continued into the following member, so a declared end past this payload is
  an expected condition. `MemberRecord::body_in` now returns the guarded slice
  and all four call sites go through it. This is the `BRIEF.md` rule about
  `unwrap()` in parser code, in the form of an index.
- **`ifcopenshell.validate --rules` failed `IfcPropertySet.UniquePropertyNames`
  on 54 of AR S1's 189 098 property sets.** Distinct Revit built-ins share a
  display name - `ALL_MODEL_DESCRIPTION` and `PROPERTY_SET_DESCRIPTION` are
  both "Description", the four `STAIRS_ATTR_CALC_*` are all "Calculation
  Rules" - and their values differ, so dropping duplicates would lose data.
  Every member of a colliding group is now qualified by its parameter id
  (`Description [-1150481]`); a name that does not collide is untouched.

### Result: the placement chain is right, verified against Revit's own export

Observation. Our AR S1 bodies span X ∈ [-7.9, 38.6] m where Revit's whole
export spans X ∈ [-18.8, 0.2]. A 1 184-body subset cannot be wider than the
17 070-body whole, so something had to be wrong.

Experiment. Both files carry the Revit element id - ours in `Tag`, Revit's in
`Tag` - so match products by it and solve for the rigid transform between the
two sets of body centroids (Kabsch). 180 ids are common to both.

Result. One rigid transform explains all 180: a rotation of **-88.52° about
Z** and a translation of (-17.08, 30.47, 0.52) m, median residual 0.22 m over a
47 m model. Not a bug - Revit's header declares
`CoordinateBase: Общие координаты`, Shared Coordinates, so its export carries
the survey-point base and the project rotation while we emit internal project
coordinates. Z agrees directly, which is why only X and Y looked wrong.

Reproduced on the second reference file. AR S2, 122 common ids, gives
**-88.39°** about Z - the same rotation to within 0.13° - with a *different*
translation, (-18.15, 69.24, 0.09) m, median residual 0.38 m. That is exactly
what a shared-coordinate base predicts and no other explanation does: the
rotation is the project's true north, one value for the whole project, while
each section model's own origin sits somewhere else in the survey frame. Had
our transforms been wrong, two independently decoded files would not agree on
one angle.

This is the strongest confirmation the transform decode has had: it is checked
against Autodesk's own answer, by a route that is not ours, on two files, and
the residual is 0.5% of the model's extent. Confidence high. Composing the
shared-coordinate base is now a known, bounded piece of work rather than an
open question.

### Result: what the reference answer says is still missing

With the reference in hand the gaps stop being guesses. On AR S1, ours vs
Revit's:

| | ours | Revit | note |
|---|---|---|---|
| `IfcBuildingStorey` | 163 | 15 | 29 distinct (name, elevation), **15 distinct names** |
| products with a shape | 1 468 | 17 070 | 8.6% |
| detailed bodies (`Body`) | 1 184 | 17 070 | 6.9% |
| typed products | **0** | 16 404 | everything we emit is `IfcBuildingElementProxy` |
| `IfcWall` | 0 | 7 617 | we recover 13 208 `SWall` elements, 2 197 with a category |

Two selection gates, in the exporter, of the kind the previous entry predicted:

*Typing.* `SOURCE_MAPPINGS` covers only MEP categories - pipes, ducts,
plumbing, electrical - because ВК/ЭОМ/КЖ is what the reader grew up on, and a
test pins `Wall`/`OST_Walls` to `Unknown` deliberately. The data is there:
`OST_Walls` 2 325, `OST_Columns` 702, `OST_Windows` 715, `OST_Doors` 345,
`OST_Floors` 294, `OST_StairsRailing` 285 on this one file. Nothing is decoded
wrongly; the table simply has no architecture rows.

*Storeys.* 163 emitted collapse to 29 distinct (name, elevation) pairs and 15
distinct names, and Revit emits exactly 15. Our first 13 match its names and
elevations exactly; the excess is the same level repeated - "01 Этаж" at
elevation 0 is emitted 37 times - at times with a second elevation for one name
(3.3 m and 4.2 m for "02 Этаж"). We emit one storey per recovered `Level`
element and never ask whether two of them are the same storey.

The corpus-wide export sweep says this is not uniform, and the split is the
diagnostic: mean storeys per file are **ВК 11** (max 12) and **ОВ 13** (max
18) - sane, matching what a single-discipline model holds - against **AR 129**
(max 236) and **KJ 598** (max 1 236). The models that explode are exactly the
ones that carry links to the project's other sections (S1..S12), and the
duplicate-with-a-second-elevation pattern is what a linked model's own level
set looks like. So the fix is not a blanket dedup of identical pairs, which
would still leave the linked sets: it is to establish which levels belong to
the host model. Revit's export answers that too - it emits the host's 15.

Whole-corpus totals from that sweep, all 50 files, zero failures:
10 030 518 elements exported, 89 979 with verified geometry, 8 095 storeys.
Mean wall-clock per file 16 s (ВК) to 81 s (ОВ).

Also corrected, a verification method rather than a finding: `create_shape`
refuses a `Box`-only product without `keep-bounding-boxes` and an `Axis`-only
one without `dimensionality = 2`, and returns **local** coordinates unless
`use-world-coords` is set - so an aggregate over products measures nothing
about placement without it. An earlier note here claimed a Box cannot be
geometrized at all; that was wrong. With all three set, every product the
exporter emits builds: 11 868 across AR/KJ/ВК/ОВ, zero failures, and
`validate --rules` clean on all four.

### Result: the export was selecting almost exactly against the truth

Observation. Adding architecture rows to `SOURCE_MAPPINGS` was going to be the
obvious fix for "0 typed products". Before writing any, join our decode to the
reference export on the Revit element id and ask what the categories we recover
actually map to.

Result, and it refuted the plan. Of the 187 131 elements that carry a category
in our decode of AR S1, **not one** is a product Revit exports. The join is
empty. Yet 11 332 of Revit's 11 518 products (98.4%) are in our decode - they
simply carry no category, because a real instance keeps its category on its
type and declares none of its own. The exporter's candidate test was
`category.is_some() || verified_geometry`, so of those 11 518 products it
exported **382, or 3.3%**, while 99.8% of the 187 127 rows it did emit were not
products at all. Adding mapping rows would have typed nothing, because the
elements to type were never selected.

The inversion is measurable in both directions: 0% of the products Revit
exports declare a category, against 31-44% of the records of the same classes
that Revit does not export.

*What the class alone establishes.* Joined on the element id, six source
classes map to exactly one IFC entity each, with no spread at all: `SWall` is
`IfcWall` 7 610 / 7 610, `Floor` is `IfcSlab` 527 / 527, `StairsLanding` is
`IfcSlab` 12 / 12, `StairsRun` `IfcStairFlight` 22 / 22, `StairsElement`
`IfcStair` 11 / 11, `ProfileRoof` `IfcRoof` 3 / 3. `FamilyInstance` is
deliberately excluded: it spreads across eight entities - railing, opening,
proxy, column, window, member, plate, door - so its category is required and
the class may not stand in for it. That category is reachable through the
element's type for only 552 of 3 084, which is the gap that remains.

*Which records are model elements.* Three clauses, each independently
meaningful, keep **100% recall** on `SWall`, `Floor` and `FamilyInstance`
alike while dropping records of those classes that are not model elements: no
`owner_view_id` (a view-owned record is annotation or a detail item), a
`created_phase_id` (a model element is placed in a phase), and no declared
category (that marks a type or definition). Precision goes 57.8 -> 69.3% on
`SWall`, 44.5 -> 58.8% on `Floor`, 28.2 -> 56.9% on `FamilyInstance`. What is
still over-selected is not separable by any field this decode recovers.

Against the reference export, AR S1 now: products Revit emits that we also
emit **11 291 of 11 518 (98.0%, from 3.3%)**, and of those **77.2% carry the
same IFC entity Revit chose**. `IfcWall`, `IfcSlab`, `IfcStair`,
`IfcStairFlight` and `IfcRoof` each have 100% recall, at precision 69.1%,
59.4%, 100%, 100%, 100%. Every remaining disagreement is a `FamilyInstance`
falling back to a proxy: 1 188 railings, 746 openings, 248 columns, 186
windows, 134 members, 37 plates, 13 doors.

Reproduced on the second reference file without touching anything: AR S2
exports **10 918 of its 11 121 products (98.2%)**, **81.1%** of them as the
same IFC entity, 15 storeys against Revit's 13 with 12 exact matches, and 100%
recall on all five typed entities again (`IfcWall` 7 733, `IfcSlab` 556,
`IfcStair` 11, `IfcStairFlight` 22, `IfcRoof` 3).

One schema trap on the way: `IfcStairFlight` declares `NumberOfRisers`,
`NumberOfTreads`, `RiserHeight` and `TreadLength` between `IfcElement`'s eight
attributes and its `PredefinedType`, so the generic writer put the enumeration
in `TreadLength` and `validate --rules` failed. Those four are written unset
rather than invented.

### Result: a storey is a level something stands on

The 163 storeys were not separable by any field on the level record itself.
They are separable by what stands on them: taking the levels that the model
elements above actually reference gives 55 records covering 15 distinct
(name, elevation) pairs, and folding records that share a name and elevation -
two such records are one storey however often the file repeats them, one of
them appearing eleven times - gives **15 storeys, which is Revit's count
exactly**. Twelve are an exact (name, elevation) match.

The three that differ are the same names at a second elevation, each exactly
0.9 m above the matched one - "02 Этаж" at 3.3 and 4.2, "03 Этаж" at 6.3 and
7.2, "05 Этаж" at 12.3 and 13.2 - which is a linked model inserted at an
offset, not a decode error. The three of Revit's that we do not reach
("01 ПромЭтаж", "25 Кровля 5", "25 Кровля 6") are storeys no element we export
stands on.

Across disciplines: KJ S1 goes 565 -> 14 storeys, AR S1 163 -> 15, while ВК S1
stays at 12, which it already had right - the rule costs nothing where nothing
was wrong. `validate --rules` is clean on all three and every emitted product
still builds.

Confirmed over the whole corpus, 50 files, zero failures. Mean storeys per file
by discipline, before -> after: **AR 129 -> 14** (max 236 -> 19), **KJ 598 ->
16** (max 1 236 -> 24), ОВ 13 -> 11, ВК 11 -> 10. The two disciplines that
carry links to the project's other sections are the two that collapse; the two
that were already right barely move. 10 364 925 elements exported in total.

### Result: what the JSON export was missing, and what it cannot know

*References resolved.* `level_id`, `type_id` and `family_id` were bare
identifiers, so answering "which storey is this wall on" meant joining an
800 133-line file to itself. Each now carries the referenced element's name
too. They resolve well: `level_name` 20 015 / 20 015 (100%), `type_name`
10 826 / 10 934 (99.0%), `family_name` 480 420 / 501 005 (95.9%). A reference
whose target has no name keeps the identifier alone rather than gaining a
placeholder.

*Units are not in the file, and this is now settled rather than assumed.* Of
460 028 parameter values on AR S1, 94.6% are built-in parameters and **none**
carries a Forge spec, while project parameters do - 78.3% of them - because
their spec is scanned out of the parameter-definition element. Three places
were checked and none holds the built-in mapping: `Global/Latest` decodes to
1 061 registry objects, all of them unit, symbol, quantity, dimension and spec
*definitions*, with nothing naming a parameter; `Formats/Latest` contains no
`autodesk.spec` identifier at all; and the value record is `[id, value]` with
no room for one. It is Revit's own definition and has to come from Autodesk's
published tables, exactly as the built-in *name* table already does.

The reference export can verify such a table once there is one, but cannot
supply it. Matching elements by Revit element id and dividing Revit's exported
quantity by our raw value identifies a dimension wherever the two describe the
same thing: `-1001101` hits exactly 304.8 - feet to millimetres - on 7 476 of
7 632 samples, and `WALL_USER_HEIGHT_PARAM` on 295, being the wall's own height
only where it is not level-bound. That locked 3 parameters of the 256 the file
uses, because it can only speak for the ones Revit exports as quantities.

Also worth knowing: 249 of those 256 built-in identifiers have a catalogue
name. The 7 that do not are not a catalogue gap to fill by guessing - two of
them, `-1001101` and `-1001111`, sit on all 13 208 walls and account for
26 416 of the 26 731 unnamed values, and neither appears in Autodesk's
published enumeration. They stay `param_-1001101`, which is rule 12 working.

### Observation: rooms are decoded and go nowhere

`RoomElem` yields **554** elements on AR S1 against the reference export's
**553 `IfcSpace`**, and they are not thin: every one carries a level (and now
its name) and a `ROOM_NAME`, and 553 of them carry the project's own schedule
parameters - `Number`, `SP_назначение`, `SP_количество_комнат`, `SP_этаж`,
`SP_подъезд`, `SP_тип_помещения` and four area parameters. That is the data
behind "what equipment belongs to room 204", which `BRIEF.md` names as a target
query.

They are already complete in the JSON. What they never reach is the IFC: a
`RoomElem` declares no category, is not a building-element class and has no
verified geometry, so the candidate filter drops all 554 and the export
carries **0 `IfcSpace`** against Revit's 553. Emitting them needs a spatial
boundary or, failing that, a space with a placement and no shape - not
attempted here, but it is a bounded gap with a reference answer to check
against.

### Observation: a storey is a set of ids, and the API was answering by name

The storey rule folds the `Level` records that share a name and an elevation.
`/levels` reported the fold but published one id per storey and offered no
filter for it, so the only way to ask for a storey's elements was the level
*name* - and three names on AR S1 belong to two storeys each. Every count made
that way silently merged them: "05 Этаж" answered 3 355 elements for 3 320 at
12.3 m plus 35 at 13.2 m.

Nor is the published id enough on its own. The 15 storeys of AR S1 are named
by **152** `Level` records - "01 Этаж" alone by 37 - and the elements are
spread over all of them: counting only the id `/levels` published reaches
11 562 of the 15 149 placed model elements, losing **3 587** to the folded
records. A storey filter has to be the whole set.

With the set, the counts reconcile: 15 149 model elements on the 15 storeys
plus **2 228 carrying no level at all** is the 17 377 the summary reports, and
the 554 rooms land 24 / 56 / 59 per residential storey.

The second silent zero was the `model_elements` flag. It keeps the seven
building classes the exporter emits, and `RoomElem` is not one of them, so
`class=RoomElem&model_elements` answered `total: 0` on a model holding 554
rooms - a number an agent reads as "this model has no rooms". The filter is
right; saying nothing about what it removed was not.

Fixed in `rivet-api`: `/levels` publishes `level_ids`, `ambiguous_name` and
per-storey `model_elements` and `rooms` counts, `/elements?storey=` filters on
the whole folded set (an id that is not a storey is a 400, not an empty page),
`/summary` carries `model_elements_without_level`, and a page reports
`excluded_as_not_model_elements` whenever that flag is what emptied it.

### Result: the bodies the export never asks for are the building itself

Observation. `rivet brep` assembles 43 622 bodies from AR S1's face-bearing
records while the IFC carries 1 468 shapes, and the JSON puts geometry on 577
of 17 377 model elements - none of them a wall, a floor, a stair or a roof.
The symbol-link funnel accounts for only its own path: 3 140 instances name a
symbol, 2 358 of those symbols carry a body, 1 443 pass the bounds check. It
says nothing about where the other 40 000 bodies are.

Hypothesis. The classes with no geometry have no body in the file, and their
shape would have to be constructed from their parameters.

Experiment. A body is decoded from a `GElement` record, and that record carries
the element id it belongs to, so every body already names an owner. `rivet
body-owners` tallies the decoded bodies by the owning element's class, and
against each owner asks three further questions: does any instance name it as
a symbol, does the body's own extent reproduce the bounds block in the same
record, and is its centre away from the origin - a body in a symbol's local
frame sits at the origin, a placed one does not.

Result, and the hypothesis is refuted. On AR S1, 30 495 element ids own a body,
27 857 of them complete, 247 083 faces:

| owner class | ids with a body | records | complete | faces | named by an instance | planar bodies matching their own bounds | off-origin |
|---|---|---|---|---|---|---|---|
| `SWall` | 13 208 | 25 486 | 11 208 | 130 217 | 0 | 10 746 / 10 747 | 13 208 |
| `FilledRegion` | 9 873 | 9 926 | 9 873 | 9 873 | 0 | 9 437 / 9 872 | 6 763 |
| `FamilySymbol` | 2 751 | 2 759 | 2 507 | 52 919 | 224 | 602 / 648 | 1 011 |
| `Floor` | 1 183 | 1 339 | 991 | 17 405 | 0 | 1 182 / 1 182 | 1 182 |
| `RoomElem` | 553 | 1 130 | 552 | 7 631 | 0 | 552 / 553 | 553 |

Every wall, floor and room in the file owns its own solid, and those solids are
already placed: 10 746 of the 10 747 planar wall bodies reproduce their
record's own bounds block to within a micro-foot, and all 13 208 sit where the
building is rather than at an origin. `FamilySymbol` is the control and behaves
as the opposite: only 1 011 of 2 751 are off-origin, because a symbol's body is
in its own local frame and needs the instance transform - which is the one path
the exporter has.

Read against the model elements, the shortfall is one gate wide: **12 482 of
the 17 377 model elements own a body** (`SWall` 11 011 / 11 011, `Floor`
896 / 896, `StairsRun` 22 / 22, `StairsLanding` 12 / 12, `ProfileRoof` 3 / 3)
and 577 more reach one through a verified symbol, against **1 468 shapes
emitted**. The exporter reads geometry only through `verified_symbol_bounds`
and never looks at the body on the element's own record.

VK S1 (SMALL) says the same from the other discipline: 12 046 ids own a body,
6 737 of them `RbsPipeCurve` with 40 422 faces, all off-origin. Its pipes reach
the IFC by the swept-disk path rather than as B-Rep, and no `FamilyInstance`
model element there owns a body at all - 82 reach one through a symbol. The
pipe bodies are cylindrical, so the planar bounds check does not speak for
them and is not counted as if it did.

So the next step is not a decoder change and not a parametric wall builder. It
is to emit the element's own body where its extent reproduces its own bounds -
the same class of per-element verification the symbol path already uses, on
12 482 elements instead of 577. What that leaves open, and what the shared
coordinate base still owes, is unchanged.

### Result: emitting the placed body, and what it costs

The measurement above leaves one thing to do: read the body off the element's
own record instead of only through a symbol. Four pieces, each of which is a
gate the previous code did not have.

*Pairing.* A body and a bounds block both come from a `GElement` record, and
one id can carry several such records - 13 208 wall ids carry 25 486 between
them. The recovery kept whichever body came last and the bounds of whichever
record carried them last, which for a multi-record id crosses one record's
body with another's box. They are now paired within the record that produced
them, and the *placed* body - the one reproducing its own record's box - is
kept over an unplaced one, the larger of two placed ones over the smaller. An
id with no placed body still keeps the last, which is what every id did
before.

*The test.* A body counts as placed when every face of it is planar, the box
holds volume, and its extent reproduces the record's own bounds to within a
micro-foot. Planar only, because an arc bulges past the endpoints the extent
is taken from and a curved body would read as a disagreement; volumetric,
because AR S1 carries 9 873 `FilledRegion` records whose single flat face
would otherwise pass. Both exclusions are conservative and both are counted.

*Not on a type definition.* A `FamilySymbol` reproduces its record's box just
as exactly, and that box is in the symbol's own local frame - the placed test
cannot tell the two apart, because both are a body agreeing with its own
record. What tells them apart is the rule this project already established: a
record that declares its own category is a type or a definition, and not one
of the 187 131 such records on AR S1 is a product Revit exports. Those keep no
body. It is not a small exclusion - 2 705 bodies, most of them the 2 197
`SWall` records that carry a declared category - and it is the difference
between following the reference export and inflating the file against it.

*Emission.* A placed body needs no symbol and no transform - it is already in
the project coordinates every other geometry here is carried in - so it goes
through the same feet-to-metres conversion with an identity transform, and the
IFC layer expresses it in the product's own frame as it already does for a
symbol-placed body.

Result on AR S1, `--include-unplaced`, against the previous file of the same
configuration:

| | before | after |
|---|---|---|
| products | 203 927 | 203 927 |
| `IfcShapeRepresentation` | 1 468 | **9 622** |
| `IfcAdvancedBrep` | 1 184 | **9 338** |
| `IfcAdvancedFace` | 8 966 | **84 745** |
| file | 169 MB | 364 MB |

Every one of the 9 622 shapes builds: `create_shape` with world coordinates
returns `IfcWall` 7 435, `IfcSlab` 703 and `IfcBuildingElementProxy` 1 484
with **zero failures**, and `validate --rules` reports no issues. Against
Revit's own export of the same model, products carrying a shape go from
**8.6% to 56.4%** of its 17 070. ВК S1 is the regression control and does not
move: 695 `IfcAdvancedBrep`, the same 695 it had, its geometry coming by the
swept-disk and symbol paths as before.

What it does not reach, all of it counted: of the 9 008 walls and 896 floors
carrying a placed body, 7 435 and 703 emit a solid - the rest have at least
one face the assembly could not resolve, and an incomplete body still falls
back rather than ship an open shell the kernel refuses. 2 003 further walls
have a body that is not planar-with-matching-bounds. Stairs and roofs own
bodies that no bounds block confirms as placed, so they keep their box.
`RoomElem` carries 552 placed volumes and stays out entirely: a room is not a
building element and reaches the export as nothing at all, which is the
`IfcSpace` gap recorded above and is a typing change, not a geometry one.

### Result: rooms reach the IFC as the spaces they are

The rooms had been complete in the JSON for two entries now and reached the
IFC not at all. What kept them out was not the decode and, after the placed
body, not the geometry either: a room fails every clause of `is_model_element`
- it is not a building class and carries no phase - and declares no category,
so the export's candidate test never saw one. Its class alone establishes what
it is, exactly as `SWall` and `Floor` do, and against the same reference:
Revit's export of AR S1 carries **553 `IfcSpace`** and the decode yields 554
`RoomElem`.

A space is not an element, and the difference is structural rather than
cosmetic. `IfcSpace` is a spatial structure element: where `IfcElement` ends
its eight attributes with `Tag`, a space carries `LongName`,
`CompositionType` and its own `PredefinedType`, and its storey **decomposes**
it through `IfcRelAggregates` instead of containing it through
`IfcRelContainedInSpatialStructure`. Both are written here, and a test holds
the split - a space aggregated exactly once, contained never, while the wall
beside it stays contained.

Naming follows the record rather than a convention: `ROOM_NUMBER` is on 553 of
the 554 and becomes `Name`, `ROOM_NAME` is on all 554 and becomes `LongName`,
which is the split Revit's own export writes. The one room without a number
keeps its recovered name in both. `PredefinedType` stays `NOTDEFINED` because
nothing in the source says which kind of space it is.

On AR S1, `--include-unplaced`: **554 `IfcSpace`**, 552 of them carrying the
room's own volume as an `IfcAdvancedBrep`, products 203 927 -> 204 481 and
shapes 9 622 -> 10 174. `create_shape` builds all 10 174 - `IfcWall` 7 435,
`IfcSlab` 703, `IfcSpace` 552, proxy 1 484 - with **zero failures**, and
`validate --rules` reports no issues, which is the check that would have
caught a spatial element written with an element's attributes. AR S2
reproduces it untouched: 570 spaces, 10 252 shapes.

The two rooms without a volume are the two whose body did not resolve
completely; they are emitted as spaces with a placement and no shape rather
than with a guessed one.

### Result: an arc's bulge, and what it did not buy

The placed test admitted only bodies whose every face was planar, because an
arc bulges past the endpoints the extent was taken from. That exclusion is now
gone: the extent adds each arc's own extreme analytically. Along one axis an
arc traces `center + R cos(a - phase)`, so it reaches an extreme at `phase`
and `phase + pi`, and only an extreme the arc actually sweeps through counts -
elsewhere the endpoints already bound it. Every point the rule adds is a point
on the arc, so it can only fail to cover, never overreach. The endpoints bound
the rest: a planar face by its loops, and a cylindrical one too, whose
generators are straight lines between boundary points.

It bought almost nothing on AR S1: **3 model elements**, 9 944 to 9 947. The
hypothesis it was written against - that 2 003 walls were being refused for
curvature - was wrong, and the corrected count says where they actually are.
Of the 13 208 wall bodies only **10 748 carry an exact bounds block at all**,
and of those, **every single one** reproduces it (10 748 / 10 748). The other
2 460 have no box to be checked against. Curvature was never the gate; a
missing bounds block is, and that is the next lever.

Where it does change what can be verified is the disciplines made of
cylinders. On ВК S1 the bodies reproducing their own box go from 1 814 to
**8 876**, including 6 708 of 6 735 `RbsPipeCurve`. None of it reaches the
file, because a pipe already carries geometry by the swept-disk path, which is
checked first and is the better representation. It will matter wherever the
placed body is the only route a curved element has.

### Result: the corpus sweep after the placed body and the spaces

Fifty files, four disciplines, `--include-unplaced`, against the recorded
sweep taken before any of this:

| | files | with geometry, before | after | |
|---|---|---|---|---|
| AR | 14 | 12 183 | **79 919** | 6.6x |
| KJ | 10 | 17 788 | **23 833** | 1.3x |
| ОВ | 13 | 40 955 | 40 955 | unchanged |
| ВК | 13 | 19 053 | 19 053 | unchanged |
| all | 50 | 89 979 | **163 760** | 1.8x |

Zero failures. **No file carries less geometry than it did.** Storeys are 626
before and after, which is the check that the storey rule was not disturbed;
elements go 10 364 925 to 10 369 210, and the difference is exactly the 4 285
spaces (AR 4 255, KJ 30, and none in the MEP models, which hold no rooms).
167 314 shape representations for 163 760 elements with geometry - a pipe
carries an axis and a body - and 111 464 `IfcAdvancedBrep`.

The two MEP disciplines not moving is the expected shape of this change rather
than a disappointment: their geometry comes by the swept-disk and symbol
paths, which were already reaching what they can reach, and their models carry
no walls, floors or rooms for the placed path to find.

Verification beyond the file everything was developed on: KJ S1, a discipline
never checked before, exports 4 156 shapes - `IfcWall` 700, `IfcSlab` 134,
proxy 3 322 - and `create_shape` builds every one of them with
`validate --rules` clean.

`scripts/export_sweep.sh` is the sweep, counting each file and deleting the
IFC as it goes; only the counts are wanted and the corpus is 50 files of a few
hundred megabytes each.

### Result: the graph header's box places the bodies the exact block cannot

The entry above closed with the gate named: of the 13 208 wall bodies on AR S1
only 10 748 carry an exact bounds block at all, every one of those reproduces
it, and the other 2 460 were refused for having nothing to be checked against.
A `GElement` record carries other boxes, so the question was whether any of
them is the same box.

*What the exact block is, and why a record can lack one.*
`GElementBounds::parse` scans the whole body for an adjacent pair of
byte-identical six-`f64` blocks and takes it **only if there is exactly one**.
That uniqueness is what makes it trustworthy without knowing where it lives,
and it is also why it fails: a record with no such pair, or with two, yields
nothing. `GElementGraphFields::parse` instead reads a box at a *declared*
offset - immediately after `GGroup.m_subNodes`, whose length the record
states - so it does not care how many other blocks the body contains.

*The measurement.* For every body-bearing record, the largest of the six
coordinate differences between the body's own extent and each box the record
carries. Where a record carries both boxes, they are the same box:

| | AR S1 | KJ S1 | ВК | ЭОМ |
|---|---|---|---|---|
| bodies whose record carries both boxes | 14 808 | 2 678 | 9 840 | 3 720 |
| of those, graph box differing from the exact block | **0** | **0** | **0** | **0** |
| bodies whose record carries no exact block | 5 139 | 2 251 | 883 | 1 476 |
| of those, placed by the graph box | **2 642** | **806** | **160** | **26** |
| ... and by the near duplicate as well | 1 239 | 116 | 10 | 2 |
| ... by either, counting the overlap once | 2 642 | 806 | 160 | 26 |

Two readings of one box, agreeing 31 046 times out of 31 046. This is the
independent-oracle pattern the type link was accepted on: the graph header's
box is reached by a different route - a declared offset rather than a
whole-body scan - and lands on the same six numbers.

The agreement with the *body* is as sharp as the exact block's, which is the
part that decides whether a second tier is a reading or a guess. Of AR S1's
2 742 bodies whose record carries no exact block but does carry a graph box,
**2 642 reproduce it to within a micro-foot** and the remaining 100 spread
across every wider bucket - 4 within 1e-4 ft, 20 within 1e-2, 40 within a
foot, 36 beyond. A box that merely sits near a body is a different box, and
almost nothing here sits near.

The near duplicate that `GElementBounds::parse_near_duplicate` finds is
**refuted as an addition**: on all four files, every body it would place the
graph box already places. It stays in the measurement and out of the
placement.

*The tier.* `body_placement_box` is `exact.or(graph)`: a record carrying an
exact block is judged on it alone, and only a record carrying none falls
through to the graph header's box. Picking one box rather than trying both is
the point - a body its own record's exact block refuses must not get to ask a
second box for a better answer - and because the two boxes agree wherever both
exist, that discipline costs nothing.

*Result on AR S1*, `--include-unplaced`, against the same file exported at the
previous commit:

| | before | after |
|---|---|---|
| elements with verified geometry | 10 173 | **11 815** |
| `IfcAdvancedBrep` | 9 889 | **11 531** |
| `IfcAdvancedFace` | 92 415 | **108 857** |
| `IfcBoundingBox` | 284 | 284 |
| `IfcWall` / `IfcSlab` / `IfcSpace` products | 11 011 / 908 / 554 | unchanged |

No product appears or disappears and no box is traded away; 1 642 products
that carried a box now carry a solid. `create_shape` with world coordinates
builds **every** product of both files with zero failures - before `IfcWall`
7 435, `IfcSlab` 702, `IfcSpace` 552, proxy 1 484; after the same but
`IfcWall` **9 077** - so the entire gain is walls, and the entire gain builds.

*What it does not reach.* 2 642 bodies were newly placed and 1 642 became
solids. The other 1 000 have at least one face the assembly could not resolve
and fall back to their box rather than ship an open shell the kernel refuses.
That is the same backlog the placed path already had, and on AR S1 it is
dominated by one exclusion: 76 510 of 80 205 excluded faces are `"face has no
first loop"`. This tier moved the gate from "no box to check against" to
"the body is incomplete", which is a different problem and the next one.

### Result: a face with no loop has no boundary in its record either

The entry above ended on the exclusion that now dominates every other:
`"face has no first loop"`, 76 510 of AR S1's 80 205 excluded faces.
`rivet loop-owner-probe` is the measurement, on AR S1, KJ S1, ВК S1 and ОВ S1.

*What such a face is.* `assemble_face` reads the boundary from the face's own
first reference, `GFace.m_pFirstLoop`. The faces that fail carry a literal
`(id 0, class 0)` there - all 76 510 of them on AR S1, 1 740 on KJ, 1 139 on
ВК, 2 106 on ОВ - and are otherwise ordinary: four references like every other
face, a resolved surface on every one (76 472 `Plane` and 38 `CylSurf` on AR
S1), an identifier no other face shares, named by a `Geometry` node's
`m_pFaces` array exactly as the loop-bearing faces are, only from later slots.
Every record involved tiled its body exactly, so this is what the file says,
not where a walk drifted.

*Hypothesis 1: read the link from the loop's side.* `GEdgeLoop` declares
`m_pFace`, and `walk_loop` already refuses a loop whose `pFace` is not its
face, so the link exists in both directions and the missing one could be
recovered by grouping the loops by the face they name.

**Refuted, and the control is what refutes it.** On faces that *do* carry a
reference the inverse map reproduces it: 367 193 of AR S1's 380 906 are
claimed by exactly one loop and it is the referenced one, 13 281 by several
including it, and 16 disagree. On the faces that carry none, **no loop claims
a single one of them** - 76 510 of 76 510. The loops are not hiding behind a
sentinel; they are not in the record.

*They are still part of the solid.* Every `Edge` names the two faces it
separates, and on AR S1 82 009 edges separate a bounded face from a loopless
one while 11 669 join two loopless ones (KJ 4 182 / 1 051, ВК 2 777 / 517,
ОВ 6 848 / 1 418). So the shell the bounded faces make is open along those
edges, and dropping the loopless faces is not a repair.

*Hypothesis 2: rebuild the boundary from the edges.* `Edge.identifiers` is
`[pFace0, pFace1, next0, next1, prev0, prev1]`, so the ring around a face is
in the edges themselves and the `EdgeLoop` object is only an entry point into
it: walk from the edge no other edge of that face steps onto, and stop when
the chain names something that is not an edge.

That reading is right, and the loops prove it. The rebuild fails on no face of
any of the four files, and where a loop exists it lands on the same ring the
loop does - one ring, ending on the very loop that claims the face, starting
at the edge that loop declares: 331 175 of AR S1's 380 906 loop-bearing faces,
87 249 of 90 881 on KJ, 57 873 of 59 667 on ВК.

**And it recovers nothing.** Every ring that no loop of the record ends is one
edge long: 415 486 on AR S1, 15 310 on KJ, 9 305 on ВК, 27 030 on ОВ, against
a real ring's 2 to 11+ edges (366 865 of AR S1's are quads). Those chains end
on a **null** `m_next` - written as 0, not as an identifier - so they are not
holes and not boundaries: they are edges whose next link on that side the file
declines to give. (This paragraph first said the chains ended on an unresolved
identifier; that was a misreading of the probe, corrected in the entry below,
which measures where the terminators actually go.) And 60 886 of AR S1's
76 510 loopless faces have no edge naming them at all.

| | AR S1 | KJ S1 | ВК S1 | ОВ S1 |
|---|---|---|---|---|
| faces in face-bearing records | 457 416 | 92 621 | 60 806 | 295 657 |
| with a first-loop reference | 380 906 | 90 881 | 59 667 | 293 551 |
| with none | 76 510 | 1 740 | 1 139 | 2 106 |
| ... claimed by a loop anyway | 0 | 0 | 0 | 0 |
| ... with no edge naming them | 60 886 | 974 | 249 | 355 |
| rings no loop ends, of one edge | 415 486 | 15 310 | 9 305 | 27 030 |
| ... of more than one | 440 | 14 | 0 | 0 |

*Result.* Both routes to the missing boundary are closed by measurement: it is
not on the loop's side and it is not in the edge chain. Nothing in the export
changes - the exclusion stands and the ~1 000 placed-but-incomplete bodies on
AR S1 keep their boxes. The one thread left is the 440 rings on AR S1 and 14
on KJ that a real `EdgeLoopWithChainEnvelopes` ends without claiming the face;
everything else ends in a null.

What is left as an option, and it is a weaker one: the 15 624 loopless faces
on AR S1 that edges do name could have those edges ordered by their endpoints
rather than by a declared link, and closure checked geometrically. That is a
reconstruction rather than a reading, it reaches a fifth of the population,
and it is worth doing only if nothing better turns up.

Confidence: high on all of it. Each count is over four files of three
disciplines, every record involved tiled exactly, and the rebuild's control -
that it reproduces the loop wherever a loop exists - is what makes its silence
elsewhere evidence rather than a failure to find.

### Result: nothing is missing from the record - the boundary is a written null

The entry above ended by pointing at the identifiers those broken chains end
on. There are none: they end on zero. `rivet identifier-probe` asks the
question properly and the answer closes the direction rather than opening it.

*Why an identifier could have been missing.* A node reaches a record's node
stream by being *fully* referenced - identifier plus class - because that is
what `walk_record_inner` queues. A bare identifier (`GEdge.m_next`,
`GEdgeLoop.m_pFace`) names an object without queueing one. So an object that
nothing full-references is never written, and that is exactly why a face with
a null `GFace.m_pFirstLoop` has no loop: the only full reference to that loop
was the null. An identifier named by a bare reference and written by nothing
would be the trace of such an object, and would say where to look.

*There are no such identifiers.* Over AR S1's 43 637 face-bearing `GElement`
records, 1 843 876 identifiers are named by a bare reference and **every one
of them is written as an object by the same record** - zero unwritten. So are
KJ S1's 461 841, ВК S1's 248 034 and ОВ S1's 1 230 085. The graph each record
carries is closed: no
`Edge`, no `EdgeLoop`, no `GFilling` names anything the record does not also
write. The 415 486 "unresolved" terminators of the previous entry were the
literal 0 of a null link, which the probe had classified as an identifier
below the record's highest - and identifiers are not a dense counter, the
highest written is 0xffffffff, so that classification measured nothing.

*Nor is it in a sibling record.* An element writes more than one `GElement`
record and the export keeps one, so a record short of boundaries could have a
whole sibling. It does not:

| elements by what their records carry | AR S1 | KJ S1 | ВК S1 | ОВ S1 |
|---|---|---|---|---|
| whole in every record | 28 310 | 7 293 | 7 375 | 31 893 |
| short in **every** record | 2 197 | 134 | 162 | 471 |
| short in one and whole in another | 3 | 9 | 0 | 0 |
| loopless faces in elements with no whole record | 76 503 | 1 638 | 1 139 | 2 106 |
| ... and in elements that have one | 7 | 102 | 0 | 0 |

Between 1 409 and 6 324 elements per file write more than one record, so the
question had room to come out the other way, and on ВК and ОВ not a single
element is short in one record and whole in another.

*Result.* The boundary of a loopless face is not somewhere else in the file
under an identifier we failed to follow. The record says null in both places
that could carry it - the face's `m_pFirstLoop` and the adjoining edges'
`m_next` - and says it consistently, in records that tile exactly and whose
object graphs are closed. Whatever fills those nulls is not serialized beside
the geometry: it is either computed at load or written in a structure this
walk does not reach at all. Confidence: high, and this direction is closed
until something outside the `GElement` body suggests where else to look.

What remains for the exclusion is the weaker option already named: order the
edges of the 15 624 loopless faces that edges do name by their endpoints
rather than by a declared link, and check closure geometrically. It is a
reconstruction rather than a reading, and it reaches a fifth of the
population.


### Result: the loopless face's boundary, ordered by its endpoints

The weaker option the entry above named has been taken, and the reason it is
allowed is a control rather than an argument.

*The reading.* A face whose `m_pFirstLoop` is null is assembled from the edges
that name it - `GEdge.m_pFace` gives each edge's face on each side - ordered
into one ring by joining endpoints. The direction of each edge is not guessed
at: it is the same `(m_flags & 1 != 0) != (side == 1)` a declared loop is read
with. Each step must find exactly one unused edge starting where the last one
ended, every edge naming the face must be used, and the ring must close in 3D;
anything else refuses the face rather than picking.

*The control.* Ordering by endpoints is a reconstruction, so it is put to the
faces where the file already declares the answer: those must come out the same
ring, compared cyclically. On the three corpus files that is **80 761 / 39 959
/ 83 335 agreed, 0 / 0 / 20 refused, and 0 / 0 / 0 contradicted**. Not one face
in the corpus has the reconstruction produce a ring the file disagrees with,
which is the only outcome that would have refused it the licence to run at all
- a refusal costs the face and nothing else. (The 1 325 / 4 289 / 13 248 faces
reported "not comparable" are faces more edges name than their first loop uses.
That is not noise: they are the faces with holes, and the entry below reads
them.)

*What it buys.* `"face has no first loop"` falls 9 813 / 3 294 / 7 435 ->
4 587 / 539 / 1 336 and faces resolved rise 82 086 / 44 248 / 96 603 ->
86 821 / 44 589 / 98 618, with bodies whose every face resolved 12 330 / 7 222
/ 8 945 -> 12 337 / 7 229 / 8 972. The route's own refusals are counted under
their own reasons rather than folded back into the old one: an ambiguous corner
209 / 356 / 1 201, edges that do not order into one ring 87 / 1 813 / 2 202,
and a ring that does not close 191 / 219 / 646. What is left of the original
exclusion is a face no edge names at all.

### Result: a face's holes are the `m_nextLoop` chain, and the edges vouch for it

`GEdgeLoop.m_nextLoop` was readable but unread: the comment in `assemble_face`
still refused it a sentinel, on the strength of loop 162 of record 278446
carrying `(8, class 0)` there. That was the null read two bytes early under the
old `GInfo.m_flags` width, and with the width corrected the sentinel is an
ordinary null. A terminal loop writes it; a face with holes writes a live
reference to its next loop.

*The reading.* Follow the chain from the face's first loop, walking each link
with the same `walk_loop` as the first - so a loop naming a different face, or
one whose edges do not close, is refused by the checks already there. A chain
that stops keeps the loops already read and records why, so no face that
resolved before reading holes stops resolving now. 897 / 2 136 / 5 481 faces
carry a further loop, 1 058 / 4 799 / 15 083 further loops read.

*The check that does not come from the chain.* The edges name their face from
their own side, so the edges bounding a face are known without reading any loop
at all. A face whose loops use exactly those edges has had its whole boundary
read; one that leaves edges over has a hole nobody read. Reading the chain
moves faces from the second population to the first and can do nothing else:

| resolved faces | SMALL | MEDIUM | BIG |
|---|---|---|---|
| accounted by the first loop alone | 85 496 | 40 300 | 85 370 |
| accounted by every loop of the face | **86 242** | **42 179** | **90 399** |
| leaving edges no loop of theirs uses | 579 | 2 410 | 8 219 |
| using more edges than name them | **0** | **0** | **0** |

No arrangement of a wrong chain produces that, and the failures agree: every
chain that stopped early - 15 / 78 / 47 of them - stopped on the cylinder-edge
backlog, never on a link that was not an `EdgeLoop`, never on a loop naming
another face, never on a cycle.

*In the export.* `IfcAdvancedFace` already emitted `IfcFaceOuterBound` for the
first loop and `IfcFaceBound` for the rest, so the holes reached IFC with no
change to `ifc-export`: 1 496 / 259 / 979 `IfcFaceBound` entities where there
were none. Element and geometry counts are unchanged (2 774 / 1 989 / 392 with
geometry), `validate --rules` is clean, and `create_shape` still builds every
emitted body.
