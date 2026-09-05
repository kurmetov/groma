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
