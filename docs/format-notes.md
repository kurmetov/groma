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
| [`reviter`](https://github.com/ahzs645/reviter) and [`rvt-rs`](https://github.com/DrunkOnJava/rvt-rs) | Large database streams | Complete stored pages are 65,249 bytes: 64,896 payload bytes followed by 353 checksum/ECC bytes. Removing only complete-page trailers before inflation prevents silent drift at page boundaries. | High for the page sizes; medium for stream applicability | Yes: all three public 2018, 2022, and 2026 family `Formats/Latest` samples fail strict parsing after raw inflation and recover at the same logical boundary after page cleanup. Synthetic exact-page and short-final-page behavior is tested. |

## Schema and object data

| Source/project | Stream/structure | Current understanding | Confidence | Independently verified here |
|---|---|---|---|---|
| `rvt-rs` and Reviter | `Formats/Latest` | Contains a generic serialized class inventory with base classes and field declarations. Class/tag values can drift by release. | High for inventory; medium for individual encodings | Partly: deterministic record tiling and failure cases are covered by synthetic fixtures; validation against the public 2018, 2022, and 2026 family samples is recorded below. |
| `rvt-rs` public corpus notes | `Global/ElemTable` | The decoded stream starts with two little-endian `u16` counts. Observed families use 12-byte implicit records from offset `0x30`; observed 2023/2024 projects use 28/40-byte records with an initial run of sentinel-valued fields. The same field may change value later, so it is not a universal record delimiter. The table contains candidate IDs, not partition byte offsets. Remaining fields are unknown. | Medium | Synthetic layouts and initial-marker validation are tested. Project-corpus validation is local-only until a redistributable fixture is available. |
| `rvt-rs` and Reviter | `Global/Latest`, `Partitions/*` | `Global/Latest` holds document-level state; bulk element instance data is associated with partition streams. Complete instance framing is not publicly established across releases. | Medium | No. Object decoding remains explicitly unavailable. |
| Independent probe (this repository) | `Partitions/*` | Every compressed member is preceded by a fixed 40-byte descriptor. Verified fields: `+4` decoded size of the previous member, `+10` stored span of the previous member, `+20` record count of this member, `+24` stored span of this member (gzip bytes + 16), `+28` total record-body bytes, `+32` format tag (101/102/103). `+14` is `0x0E4E` throughout, `+8` is `0x0E47` except in a partition's first descriptor, and `+18`/`+36` are zero. A partition stream starts with four bytes followed by the first descriptor. | High for the sizes and the chain; medium for the unnamed words | Yes: `rivet member-framing` on three private Revit 2023 project models. all 34,077 members satisfy `+24 == gzip bytes + 16`; every inter-member gap is exactly 40 or 228 bytes; all 40-byte gaps also satisfy both back-pointers. Synthetic fixtures cover the parser. |
| Independent probe (this repository) | `Partitions/*` decoded members | A decoded member is a sequence of length-prefixed records with no separators: format tag 101 uses a 12-byte header whose `u32` at `+4` is the body length; tags 102 and 103 use a 16-byte header whose `u32` at `+8` is the body length. A record body may continue into the next member of the same partition, so the walk carries the outstanding byte count forward. | High | Yes: all 34,077 members in three private Revit 2023 project models walk without an error, and for every one of them the recovered record count equals the descriptor's `+20` and the body total equals `+28`. 7.59M records recovered. 2,377 members end inside a record and exactly as many resume with the matching carry. |
| Independent probe (this repository) | Member record headers | A record header is `[id:u32]` then, at `+4` (narrow) or `+8` (wide), the body length, then a trailing word read as `[schema class index:u16][unknown:u16]`. A wide header additionally carries an undecoded `u32` at `+4`. Each element is written as three records, one per descriptor format tag: tag 101 always carries class `ElementHeader`, tag 102 carries the element's own class, tag 103 carries `GElement` or `SerializedDummy`. | High for the identifier and class index; the remaining words stay unnamed | Yes: across three private Revit 2023 project models the lead `u32` covers 99.998%, 99.858%, and 100.000% of the `Global/ElemTable` candidate IDs, and the class index resolves against the decoded schema for 99.76-99.79% of all 7.59M records. Record leads ascend within 99.6% of members. |
| Independent probe (this repository) | `ElementHeader` record body | The body opens with the identifier block the schema declares: a `u16` (observed zero), then six `ElementId` slots of four bytes each. `+2` is the Revit category code and `+6` is a reference to the owning family element; `-1` means unset. Alignment beyond `+10` is not established. | High for `+2` and `+6`; the later slots stay unverified | Yes: over 60,000 header bodies, a per-byte scan puts the only category-range peak at `+2` (19.1%, neighbouring offsets 0%) and the only `Global/ElemTable` peak at `+6` (25.0%). Across the three models 17.9-21.6% of headers carry a category and 79.4-90.6% a family reference. |
| Independent probe (this repository) | `Element` base-class body | A body opens with a block of object pointers: a null pointer is `00 00`, a reference is `ff ff ff ff` followed by a `u16` schema class index. The block is followed by one four-byte word (observed `01 00 00 00`) and then `m_id`, after which the layout is the fixed tail the schema declares: seven `ElementId` slots (`m_assocLevelId`, `m_famId`, `m_unplacedOwnerId`, `m_ownerDBViewId`, `m_createdPhaseId`, `m_demolishedPhaseId`, `m_designOptionId`) and three `Bool` flags. | High | Yes: walking the block lands exactly four bytes before `m_id` for 97.4-100% of 8,000 records across ten classes, and the walk reaches `m_id` without any search for 90.6-92.1% of the 2.5M element bodies in three models; the rest fall back to a bounded identifier search. On 4,000 `FamilyInstance` bodies `m_assocLevelId` holds 50 distinct values of which 97.4% are `Global/ElemTable` IDs, `m_demolishedPhaseId` is always unset, and the flags are always boolean. |
| Independent probe (this repository) | Strings inside record bodies | Strings are `[count:u32][UTF-16LE code units]`, the same encoding already verified in `BasicFileInfo` and `Global/PartitionTable`. Where a class keeps its string is class-dependent: some hold it at a fixed offset after the `Element` tail, others behind variable-length fields. | High for the encoding; the position is calibrated per class, not derived from the schema | Yes: readable names come out of `Level`, `Family`, `FamilySymbol`, `CategoryElem`, `ParamElemFamily`, `FontElem` and `RbsPipeCurve` records in the project corpus; the strings themselves are model content and stay out of this repository. 106 of 271 classes place their first readable string at one offset in at least 90% of records. Bodies also contain unit identifiers such as `autodesk.unit.unit:meters-1.0.0`. |
| Independent probe (this repository) | Parameter values | `Element` declares four typed parameter sets (`m_pParamValueSetDouble/Int/AString/ElementId`) and the corresponding `ParamValueDouble/Int/AString/ElementId` classes give their entry layout: `[value:f64][paramId]`, `[paramId][value:i32]`, `[paramId][string]`. Each set is `[count:u32]` followed by its entries, and the sets are stored one after another in that order. A negative identifier is a built-in parameter code; a positive one is the identifier of a parameter element, whose name is recoverable. | High for the entry layout, which comes from the schema; the run's position inside a body is found by scanning | Yes: 235,704 / 418,659 / 808,834 values recovered across three models. Verified case: one element carries a built-in text parameter and a second text parameter whose identifier resolves to a `ParamElemExternal` in the same model, so the parameter's own name is recoverable from the export. |
| Independent probe (this repository) | `Partitions/*` | A schema-resolved `[class index:u16][zero:u16]` marker for `GElement` does not delimit records. Bounded envelopes around each candidate place most of them inside ascending `u32` runs, no candidate distance dominates, and the post-marker `u32` matches `ElemTable` IDs only intermittently. | Medium (refutation) | Yes: `rivet marker-envelopes` on three private Revit 2023 project models; the capture path itself is covered by synthetic fixtures. |

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
- Record bodies are decoded only for the `ElementHeader` identifier block.
  Parameters, names, and geometry are still opaque, as is the rest of the
  `ElementHeader` body beyond `+26` (six `f64` that look like a bounding box
  follow at `+48`, unverified).
- The pointer block is walked but not interpreted: the number of fields it
  yields varies within a single class, so it is not yet a one-to-one map onto
  the schema's property list. The four-byte word between the block and `m_id`
  is unexplained.
- Level coverage is a property of the models, not of the reader: 89% of
  `FamilyInstance` in the plumbing model carry a level against ~40% in the
  other two, while element bodies are read for essentially every record.
- A name is accepted only if it decodes as UTF-16, is at least two characters,
  contains only letters/digits/name punctuation, and keeps all its non-ASCII
  characters inside one 256-code-point block. That rejects chance decodings of
  arbitrary bytes, but it is a filter, not a layout rule: names taken by
  scanning are marked `"name_source":"scan"` in the export.
- Where the parameter run starts inside a body is not derivable yet, so it is
  located by scanning. Chance matches are held off by the narrow built-in code
  window rather than by a minimum set count: 99.6% of recovered codes cluster
  in `-1_200_000..-1_152`, and after the window was narrowed to
  `-2_000_000..-1_000` the out-of-cluster tail fell to 0.00% of values.
- Parameters are recovered for 22.9-27.1% of elements: `Level`,
  `RbsPipeCurve` 100%, `MaterialElem` 97%, `FamilyInstance` 94%, while
  `CategoryElem`, `FontElem` and most style objects carry none at all, which is
  what the numbers should look like for objects that have no parameters.
- Built-in parameter codes are reported as numbers: the `BuiltInParameter`
  mapping is Autodesk's and is not hardcoded here. Names resolve only for
  parameters defined by an element in the same model.
- Category codes are reported as numbers. The public `BuiltInCategory` mapping
  is not hardcoded; category names most likely live in the model's own
  `CategoryElem` records, which are not decoded yet.
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
