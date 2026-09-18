# RVT format notes

This is an evidence log, not a specification. Confidence labels mean:

- **high**: standardized structure or reproduced by multiple independent
  implementations/corpora;
- **medium**: consistent public corpus evidence, but incomplete semantics;
- **low**: hypothesis that must not drive production semantics yet.

## Container

| Source/project | Stream/structure | Current understanding | Confidence | Independently verified here |
|---|---|---|---|---|
| [Microsoft MS-CFB](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/50708a61-81d9-49c8-ab9c-43c98a795242) | Whole file | RVT uses a hierarchical CFB/OLE container. The standardized signature is `D0 CF 11 E0 A1 B1 1A E1`. | High | Yes: synthetic traversal plus public Autodesk 2018, 2022, and 2026 family samples. Project `.rvt` corpus is still pending. |
| [`phi-ag/rvt`](https://github.com/phi-ag/rvt) | Stream tree | Observed top-level streams include `BasicFileInfo`, `Contents`, `Formats/Latest`, `Global/*`, `Partitions/*`, `PartAtom`, `RevitPreview4.0`, and `TransmissionData`. | High | Yes: all three public family samples exposed the same 13-stream inventory. Independently corroborated by `rvt-rs` and Reviter. |
| [`rvt-rs`](https://github.com/DrunkOnJava/rvt-rs) | Stream tree | The stream hierarchy is reported stable across a Revit 2016–2026 family corpus, though individual streams may be absent. | High | Partly: releases 2018, 2022, and 2026 were opened and enumerated locally. |

## Metadata and framing

| Source/project | Stream/structure | Current understanding | Confidence | Independently verified here |
|---|---|---|---|---|
| [`phi-ag/rvt/src/info.ts`](https://github.com/phi-ag/rvt/blob/main/src/info.ts) | `BasicFileInfo` | Starts with a little-endian layout version. Known public layouts are 10, 13, and 14. Strings use length-prefixed UTF-16LE; layouts 13/14 place a four-character release year after marker `04 00 00 00`. | Medium | Yes for release extraction on public 2018, 2022, and 2026 family samples. Unknown layouts are not guessed. |
| [`rvt-rs`](https://github.com/DrunkOnJava/rvt-rs) | Compressed service streams | Observed compressed streams use a gzip header followed by raw DEFLATE and may omit the normal CRC32/ISIZE trailer. | High | Synthetic trailer-independent inflation is tested; the gzip member at byte 8 of the real 2026 `Global/ElemTable` stream was confirmed. |
| `phi-ag/rvt` and `rvt-rs` | `Global/*`, `Formats/Latest` | Some compressed members begin after an application prefix; offsets 0 and 8 are observed, while four-byte wrappers occur on other streams. | Medium | Prefix preservation and offsets 0/4/8 are covered synthetically. Per-stream framing map pending. |
| Independent probe (this repository) | `BasicFileInfo` labelled block, `Global/History` document fields | `BasicFileInfo` carries a UTF-16LE `Label: value` block whose code units are *not* aligned to the stream - on the corpus they sit at odd byte offsets. It names the document's own identity in plain text: `Unique Document GUID` changes with every save, and `Unique Document Increments` counts the saves. `Global/History` states the same GUID structurally as the newest episode, and its `DocumentHistory` fields carry five more 16-byte GUIDs at payload offset 22, past the two-byte class-index tag the stream opens with: `m_creationGUID`, `m_detachGUID`, `m_upgradeGUID`, `m_previousUpgradeGUID`, `m_saveAsGUID`. The creation and detach GUIDs are lineage, not identity - a template and a central model respectively. | High for the two labelled fields and the newest episode; high for the five GUIDs' offset, and their *meaning* is the class declaration's, not a measurement | Yes: over 74 private Revit 2023 project files the newest episode equals the labelled `Unique Document GUID` 74 out of 74. Against sha256 as ground truth, sharing that GUID and being byte-identical are the same 9 of 2701 pairs, with no pair satisfying one and not the other - including two saves of one model, which differ in both. The five GUIDs' alignment is pinned by shape: all 370 reads at offset 22 are a well-formed RFC 4122 version-4 GUID or the nil GUID, against 2 of 370 at offset 20 and 0 of 370 at 24. The same 74 files state only 19 distinct creation GUIDs and 61 detach GUIDs against 67 document GUIDs, which is what rules both out as an identity. |
| [`reviter`](https://github.com/ahzs645/reviter) and [`rvt-rs`](https://github.com/DrunkOnJava/rvt-rs) | Large database streams | Complete stored pages are 65,249 bytes: 64,896 payload bytes followed by 353 checksum/ECC bytes. Removing only complete-page trailers before inflation prevents silent drift at page boundaries. | High for the page sizes; medium for stream applicability | Yes: all three public 2018, 2022, and 2026 family `Formats/Latest` samples fail strict parsing after raw inflation and recover at the same logical boundary after page cleanup. Synthetic exact-page and short-final-page behavior is tested. |

## Schema and object data

| Source/project | Stream/structure | Current understanding | Confidence | Independently verified here |
|---|---|---|---|---|
| `rvt-rs` and Reviter | `Formats/Latest` | Contains a generic serialized class inventory with base classes and field declarations. Class/tag values can drift by release. | High for inventory; medium for individual encodings | Partly: deterministic record tiling and failure cases are covered by synthetic fixtures; validation against the public 2018, 2022, and 2026 family samples is recorded below. |
| `rvt-rs` public corpus notes | `Global/ElemTable` | The decoded stream starts with two little-endian `u16` counts. Observed families use 12-byte implicit records from offset `0x30`; observed 2023/2024 projects use 28/40-byte records with an initial run of sentinel-valued fields. The same field may change value later, so it is not a universal record delimiter. The table contains candidate IDs, not partition byte offsets. Remaining fields are unknown. | Medium | Synthetic layouts and initial-marker validation are tested. Project-corpus validation is local-only until a redistributable fixture is available. |
| `rvt-rs` and Reviter | `Global/Latest`, `Partitions/*` | `Global/Latest` holds document-level state; bulk element instance data is associated with partition streams. Complete instance framing is not publicly established across releases. | Medium | No. Object decoding remains explicitly unavailable. |
| Independent probe (this repository) | `Partitions/*` | Every compressed member is preceded by a fixed 40-byte descriptor. Verified fields: `+4` decoded size of the previous member, `+10` stored span of the previous member, `+20` record count of this member, `+24` stored span of this member (gzip bytes + 16), `+28` total record-body bytes, `+32` format tag (101/102/103). `+14` is `0x0E4E` throughout, `+8` is `0x0E47` except in a partition's first descriptor, and `+18`/`+36` are zero. A partition stream starts with four bytes followed by the first descriptor. | High for the sizes and the chain; medium for the unnamed words | Yes: `openrvt member-framing` on three private Revit 2023 project models. all 34,077 members satisfy `+24 == gzip bytes + 16`; every inter-member gap is exactly 40 or 228 bytes; all 40-byte gaps also satisfy both back-pointers. Synthetic fixtures cover the parser. |
| Independent probe (this repository) | `Partitions/*` decoded members | A decoded member is a sequence of length-prefixed records with no separators: format tag 101 uses a 12-byte header whose `u32` at `+4` is the body length; tags 102 and 103 use a 16-byte header whose `u32` at `+8` is the body length. A record body may continue into the next member of the same partition, so the walk carries the outstanding byte count forward. | High | Yes: all 34,077 members in three private Revit 2023 project models walk without an error, and for every one of them the recovered record count equals the descriptor's `+20` and the body total equals `+28`. 7.59M records recovered. 2,377 members end inside a record and exactly as many resume with the matching carry. |
| Independent probe (this repository) | Member record headers | A record header is `[id:u32]` then, at `+4` (narrow) or `+8` (wide), the body length, then a trailing word read as `[schema class index:u16][unknown:u16]`. A wide header additionally carries an undecoded `u32` at `+4`. Each element is written as three records, one per descriptor format tag: tag 101 always carries class `ElementHeader`, tag 102 carries the element's own class, tag 103 carries `GElement` or `SerializedDummy`. | High for the identifier and class index; the remaining words stay unnamed | Yes: across three private Revit 2023 project models the lead `u32` covers 99.998%, 99.858%, and 100.000% of the `Global/ElemTable` candidate IDs, and the class index resolves against the decoded schema for 99.76-99.79% of all 7.59M records. Record leads ascend within 99.6% of members. |
| Independent probe (this repository) | `ElementHeader` record body | The body opens with the identifier block the schema declares: a `u16` (observed zero), then six `ElementId` slots of four bytes each. `+2` is the Revit category code and `+6` is a reference to the owning family element; `-1` means unset. Alignment beyond `+10` is not established. | High for `+2` and `+6`; the later slots stay unverified | Yes: over 60,000 header bodies, a per-byte scan puts the only category-range peak at `+2` (19.1%, neighbouring offsets 0%) and the only `Global/ElemTable` peak at `+6` (25.0%). Across the three models 17.9-21.6% of headers carry a category and 79.4-90.6% a family reference. |
| Independent probe (this repository) | `Element` base-class body | A body opens with object encodings: a null pointer is `00 00`, an inline dynamic object can begin `ff ff` plus a `u16` schema class index, and another reference form is `ff ff ff ff` plus that index. The block is followed by one four-byte word (observed `01 00 00 00`) and then `m_id`, after which the layout is the fixed tail the schema declares: seven `ElementId` slots (`m_assocLevelId`, `m_famId`, `m_unplacedOwnerId`, `m_ownerDBViewId`, `m_createdPhaseId`, `m_demolishedPhaseId`, `m_designOptionId`) and three `Bool` flags. | High for the fixed tail and dynamic class markers; other pointer semantics remain partial | Yes: walking null/reference forms lands exactly four bytes before `m_id` for 97.4-100% of 8,000 records across ten classes, and the walk reaches `m_id` without any search for 90.6-92.1% of the 2.5M element bodies in three models; inline forms fall back to a bounded identifier search. On 4,000 `FamilyInstance` bodies `m_assocLevelId` holds 50 distinct values of which 97.4% are `Global/ElemTable` IDs, `m_demolishedPhaseId` is always unset, and the flags are always boolean. |
| Independent probe (this repository) | Strings inside record bodies | Strings are `[count:u32][UTF-16LE code units]`, the same encoding already verified in `BasicFileInfo` and `Global/PartitionTable`. Where a class keeps its string is class-dependent: some hold it at a fixed offset after the `Element` tail, others behind variable-length fields. | High for the encoding; the position is calibrated per class, not derived from the schema | Yes: readable names come out of `Level`, `Family`, `FamilySymbol`, `CategoryElem`, `ParamElemFamily`, `FontElem` and `RbsPipeCurve` records in the project corpus; the strings themselves are model content and stay out of this repository. 106 of 271 classes place their first readable string at one offset in at least 90% of records. Bodies also contain unit identifiers such as `autodesk.unit.unit:meters-1.0.0`. |
| Independent probe (this repository) | Parameter values | `Element` declares four typed parameter sets (`m_pParamValueSetDouble/Int/AString/ElementId`) and the corresponding `ParamValueDouble/Int/AString/ElementId` classes give their entry layout: `[value:f64][paramId]`, `[paramId][value:i32]`, `[paramId][string]`. Each set is `[count:u32]` followed by its entries. A negative identifier is a built-in parameter code; a positive one may identify a parameter element. | High when bound to schema-resolved set-class references; low for the retained legacy scan | The reader first inspects dynamic `ParamValueSet*` class references before `m_id`, then accepts exactly one post-tail run with those value kinds and release-validated IDs. This rejects the `Family` reference-list and `-65536` counterexamples. Across three complete 2023 models, 414,176 bodies yielded 1,415,562 values. A distinct-element export of the first model retained 197,315 values on 67,751 owners, with no unknown IDs or extreme doubles. |
| Independent probe (this repository) | `Level` / `DatumPlane.m_pSurface` | A level's elevation is `Plane.m_origin[2]`, stored in Revit internal feet. The dynamic `Plane` class index is resolved from `Formats/Latest`; the reader validates the complete inherited `Surface` envelope/orientation and orthonormal plane axes rather than accepting a plausible `f64`. | High for the Revit 2023 corpus | Yes: exactly one valid plane in 269/269, 293/293 and 773/773 `Level` bodies. Converted values reproduce the expected floor sequences and negative basement elevation in metres. |
| Independent probe (this repository) | `RbsPipeCurve` curve-driver line | A circular `RbsCurve` carries equal width/diameter and height values in internal feet. The value can be nominal rather than physical: one `Ø40` pipe has 40 mm there but a 48 mm outside envelope. Its straight built-in curve stores a two-`f64` parameter domain, a three-`f64` origin and a unit three-`f64` direction; evaluating the endpoints over the domain gives the pipe axis. The outer radius is solved independently from the duplicated six-`f64` `GElement` bounds, and promotion requires every usable axis to yield the same radius and reproduce the complete analytic swept-disk bounds. | High for straight pipes in the Revit 2023 plumbing corpus; unverified for other releases and curved pipes | The first complete model contains 6,737 structurally valid straight-pipe candidates; 6,735 have parseable `GElement` bounds and 6,510 reproduce them within `1e-8` feet. The two without bounds and 225 mismatches remain geometry-free. All 77 level-associated pipe segments selected by the default IFC export pass the gate. |
| Independent probe (this repository) | `PipeFittingCenterLine.m_CenterCurves` | A one-item fitting center-curve collection embeds a schema-resolved `GLine`. Its `GInfo.m_tag` equals the owning fitting element ID in the checked corpus; the line carries the same two-parameter domain, origin and unit direction structure as other `GLine` values. Promotion additionally requires the helper category `OST_PipeFittingCenterLine`, an existing `OST_PipeFitting` owner, exactly one qualifying helper for that owner, and both endpoints inside the owner's independent `GElement` bounds. This supports an axis only; it supplies no fitting body or radius. | High for single straight pipe-fitting centerlines in the Revit 2023 plumbing corpus; unverified for other releases and non-linear/multi-curve centerlines | The first complete model contains 2,010 structurally valid single-line fitting centerlines. 31 pass every association/bounds gate for the 48 pipe fittings selected by the default IFC export and are emitted as axis-only geometry. The other 17 selected fittings remain without geometry. |
| Independent probe (this repository) | `FamilyInstance.m_instOrigin/m_RefDir/m_zAxis` | The class schema declares three consecutive three-`f64` fields. The diagnostic reader accepts finite origins and unit orthogonal directions, then requires exactly one candidate whose origin lies inside a separately serialized bounds pair. Family-instance bounds may differ below `1e-8` feet between their two copies, so this placement-only check is separate from the exact bounds required for pipe bodies. The coordinate-space relationship to project geometry is not established. | Medium for locating candidate frames; insufficient for IFC placement | The first complete model has 9,940 family instances with at least one orthonormal candidate and 573 with exactly one candidate inside owner bounds. Only 7 of the 75 mapped family instances in the default IFC export pass, so these values remain diagnostic JSON and are not used as IFC object placements. |
| Independent probe (this repository) | `GElement` / `GInstance.m_instanceInfo` / `InstInfoBase.m_Trf` | A schema-resolved `GInstance` embeds a `Trf` consisting of a 3×3 basis and three-coordinate origin. The reader accepts exactly one finite, orthonormal, right-handed transform after the marker and independently requires its origin inside the element's `GElement` bounds. The basis rows are local X/Y/Z expressed in project coordinates. | High for rigid, non-reflected `GInstance` transforms in the Revit 2023 corpus; reflected/scaled instances remain unsupported | The first complete model contains 6,524 bounds-verified transforms. They cover 66 of the 75 mapped family instances in the default IFC export. IFC writes them as storey-relative `IfcLocalPlacement`; after transforming IFC-local axes back to world coordinates, all endpoints for the 108 represented elements match the pre-placement reference export within `1e-9`. |
| Independent probe (this repository) | Parameter specs and units | A project/shared value points by positive `paramId` to a `ParamElem`; its `m_pParamDef` carries an `autodesk.spec.*` Forge type ID. `AUnits` / `FormatOptions.m_unitTypeId` describe document display formatting and the definitions in `Global/Latest` form an embedded Forge registry, not per-value tags. Conversion uses the spec plus Revit's internal base-unit system. | High for schema-bound project/shared values and registry conversion | The 2023 catalog contains 151 unique measurable spec keys with canonical unit, scale and offset. Length, area, volume, angle, absolute temperature and derived quantities have tests. A full-model audit produced 1,714 converted values. Unknown specs and built-in doubles without a recovered data type stay unconverted. |
| Independent probe (this repository) | `Partitions/*` | A schema-resolved `[class index:u16][zero:u16]` marker for `GElement` does not delimit records. Bounded envelopes around each candidate place most of them inside ascending `u32` runs, no candidate distance dominates, and the post-marker `u32` matches `ElemTable` IDs only intermittently. | Medium (refutation) | Yes: `openrvt marker-envelopes` on three private Revit 2023 project models; the capture path itself is covered by synthetic fixtures. |

## Open questions

- A continuation shifts exactly four body bytes from the receiving member to
  the member that handed the record on, so `+28` differs from the physically
  stored body total by `+4`/`-4` around a boundary. The rule holds for every
  continuation in the corpus, including members that both receive and hand on
  a record, but its cause is unknown.
- The 188-byte block inside a 228-byte inter-member gap has a high-entropy body
  and the UTF-16 text "Data generated". It is preserved and measured, never
  interpreted: if it is a protection mechanism, decoding it is out of scope.
- Each partition stream ends with 259-281 unexplained trailing bytes.
- Most record bodies remain opaque. The known exceptions are the
  `ElementHeader` category/family block, the fixed `Element` tail, calibrated
  names, schema-bound parameter sets, the validated `Level` plane, and
  independently bounds-checked straight `RbsPipeCurve` geometry, and
  owner-verified single-line `PipeFittingCenterLine` axes. Other geometry and
  the rest of `ElementHeader` beyond `+26` remain unverified (six
  `f64` resembling a bounding box follow at `+48`).
- Dynamic parameter-set class markers in the leading object block are now
  interpreted, but the complete pointer grammar is not: its field count varies
  within a class, and the four-byte word between the block and `m_id` remains
  unexplained.
- Level coverage is a property of the models, not of the reader: 89% of
  `FamilyInstance` in the plumbing model carry a level against ~40% in the
  other two, while element bodies are read for essentially every record.
- A name is accepted only if it decodes as UTF-16, is at least two characters,
  contains only letters/digits/name punctuation, and keeps all its non-ASCII
  characters inside one 256-code-point block. That rejects chance decodings of
  arbitrary bytes, but it is a filter, not a layout rule: names taken by
  scanning are marked `"name_source":"scan"` in the export.
- For supported schemas, parameter value kinds must match the dynamic
  `ParamValueSet*` classes referenced before `m_id`; every positive identifier
  must resolve to a parameter-definition element, every negative code must be
  in the release catalog, and exactly one post-tail run may match. The older
  unbound scan remains diagnostics only.
- Positive parameter IDs can resolve to model-defined names and Forge specs
  when the candidate is genuine. Built-in parameter names now resolve through
  the Revit 2023 catalog, but their data type is still missing: Autodesk's API
  exposes it through `Definition.GetDataType()` on a concrete parameter, not
  through the `BuiltInParameter` enum table.
- Category codes now retain the number and add the versioned
  `BuiltInCategory` enum name. A localized `CategoryElem` join is still open.
- The `u16` sharing the trailing word with the class index takes small
  enumerated values (0, 2, 65535, 1 dominate) and is not explained.
- The wide header's `u32` at `+4` is undecoded; one value repeats for hundreds
  of thousands of records, so it is not a per-record hash.
- The tag-to-role mapping (101 header, 102 element class, 103 geometry-like)
  is corpus evidence from one release, not a specification.

## Implementation consequences

- Use the maintained Rust `cfb` crate instead of reimplementing FAT, mini-FAT,
  directory trees, and sector chains.
- Keep raw stream extraction as a first-class API.
- Return unknown framing unchanged and retain recognized prefix bytes.
- Never derive element semantics from the tentative `ElemTable` record layout.
- Add representative, redistributable fixtures before promoting any schema or
  object hypothesis.

## Schema corpus validation

The samples below are Autodesk `rac_basic_sample_family` files mirrored by
[`phi-ag/rvt`](https://github.com/phi-ag/rvt/tree/main/examples/Autodesk).
They are external validation inputs and are not committed to this repository.
`Classes` includes inline definitions; `top-level` counts stream-level class
records only.

| Release | SHA-256 | Stored / decoded `Formats/Latest` bytes | Classes / top-level | Declared / parsed properties | Result |
|---|---|---:|---:|---:|---|
| 2018 | `301fd82d5ee48158015e36a87246f32477f6c1f8d078f7e8acccfc5c1f89688f` | 135,674 / 418,793 | 4,024 / 3,114 | 10,989 / 11,070 | Exact terminator; 0 trailing bytes, unresolved references, or index mismatches |
| 2022 | `e6373e7632a738bee3f39a1dc8a43aad4ef5eef26cf2d681f005434a9939befa` | 149,689 / 453,245 | 4,307 / 3,338 | 11,709 / 11,796 | Exact terminator; 0 trailing bytes, unresolved references, or index mismatches |
| 2026 | `00425e9d0766c3d520df344189375970c5219bd5f48e58b399484132fdb41e8d` | 165,553 / 496,597 | 4,690 / 3,604 | 12,558 / 12,639 | Exact terminator; 0 trailing bytes, unresolved references, or index mismatches |

Raw inflation first diverged inside a property record for every sample. After
removing the 353 checksum/ECC bytes from each complete stored page, the same
strict parser consumed each schema to its eight-byte zero terminator. No field
search, resynchronization, or plausible-name heuristic is used.
