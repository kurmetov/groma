# Architecture

Rivet is deliberately split at format boundaries so that uncertain
reverse-engineering work cannot leak into the canonical BIM model.

```text
.rvt bytes
  -> rvt-container  (CFB, streams, compression/framing)
  -> rvt-schema     (generic type and field declarations)
  -> rvt-model      (serialized objects, including unknown bytes)
  -> bim-core       (format-independent BIM entities)
  -> exporters      (future JSON, IFC, databases)
```

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
catalog. The object and BIM crates currently define only stable,
loss-preserving data shapes. `rvt-model` recognizes only corpus-backed
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
values rather than named fields, and nothing else in a body is interpreted.

## Export roadmap

JSON is the first exporter and IFC is a required target, not an optional one.
The JSON exporter currently lives in the CLI and emits only fields the readers
actually recovered, omitting anything unknown rather than emitting a default;
it will move behind `bim-core` once the model carries names and parameters.
The dependency direction is fixed: exporters read `bim-core` only. Element
identity, category, family/type/instance hierarchy, parameters with units,
levels, spaces, and host-child relations must therefore be reconstructed in
`bim-core` in a form that satisfies both outputs, because an IFC exporter that
reaches back into RVT structures would reintroduce the coupling this split
exists to prevent. Geometry stays out of both exporters until the object graph
and semantics are reliable.

## Invariants

1. No public API mutates an RVT file.
2. Every bounded allocation has an explicit limit.
3. Unknown bytes remain accessible.
4. Schema and object semantics require corpus-backed tests before promotion.
5. `bim-core` never depends on an RVT crate.
