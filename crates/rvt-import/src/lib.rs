#![forbid(unsafe_code)]
//! Semantic reconstruction: a Revit file's records read into
//! [`bim_core::BimModel`].
//!
//! This is the stage between RVT serialization decoding and the exporters.
//! It used to live inside the `openrvt` binary,
//! which is why adding a second source format meant editing the CLI; IFC
//! reading was already a crate of its own and this is now its peer.
//!
//! The three questions it answers, in order:
//!
//! 1. [`recover_elements`] walks the container's partition members and
//!    resolves each record into an [`ExportedElement`] - the loss-preserving
//!    Revit-shaped intermediate.
//! 2. [`metadata_model`] selects which of those stand in the model as built,
//!    types them, and normalises each into a [`bim_core::BimElement`].
//! 3. [`geometry_statistics`] and [`tally_class_geometry`] measure what the
//!    walk resolved and what it refused, which is what the corpus gate reads.

use bim_convert::element_type_for_source;
use bim_core::{
    BimBoundingBox, BimBrep, BimBrepArc, BimBrepCurve, BimBrepEdge, BimBrepFace, BimBrepProfile,
    BimBrepRuling, BimBrepSurface, BimCategory, BimColor, BimDocumentIdentity, BimElement,
    BimElementId, BimElementType, BimExternalId, BimGeometry, BimLevel, BimLineSegment,
    BimMaterial, BimMaterialLayer, BimMaterialLayerSet, BimModel, BimNumber, BimPlacement,
    BimPoint3, BimProjectIdentity, BimProperty, BimPropertyValue, BimSiteLocation, BimSource,
    BimSweptDisk, BimUnit,
};
use revit_catalog::Catalog;
use rvt_container::{
    BasicFileInfo, DEFAULT_DECODE_LIMIT, PartitionReadOptions, REVIT_STORED_PAGE_BYTES,
    RvtContainer, decode_known_framing, strip_revit_page_checksums,
};
use rvt_model::{
    ELEMENT_TAIL_BYTES, ElemTable, ElementFields, ElementHeaderFields,
    FamilyInstancePlacementFields, FittingCenterLineFields, GElementBounds, GElementGraphFields,
    GInstanceTransformFields, GeoSiteFields, LevelFields, MemberWalk, ParameterSetClassIndexes,
    ParameterSets, ParameterSpec, ParameterValue, PipeLineGeometryFields, RecordHeader,
    RecordLayout, RecordString, RvtPoint3,
};
use rvt_schema::Schema;
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    io::{self},
    path::Path,
};

/// Share of a class's records that must place their first readable string at
/// the same offset before that offset is treated as the class's name field.
pub const NAME_OFFSET_AGREEMENT: u64 = 90;
/// Schema class whose record body starts with the element's identifier block.
pub const ELEMENT_HEADER_CLASS: &str = "ElementHeader";
/// Descriptor format tag whose records carry the element's own class; the
/// other tags carry a header record and a serialized/geometry record.
pub const ELEMENT_CLASS_FORMAT_TAG: u32 = 102;

/// Paths of the top-level `Partitions/*` streams, in container order.
pub fn partition_paths(container: &RvtContainer) -> Vec<String> {
    container
        .streams()
        .iter()
        .filter(|stream| {
            stream
                .path()
                .strip_prefix("Partitions/")
                .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        })
        .map(|stream| stream.path().to_owned())
        .collect()
}

/// # Errors
///
/// Fails where neither the raw stream nor the checksum-stripped one decodes
/// as a schema; the error names both attempts.
pub fn decode_schema_stream(
    raw: &[u8],
) -> Result<(rvt_container::DecodedStream, Schema, bool), Box<dyn Error>> {
    match decode_schema_attempt(raw) {
        Ok((decoded, schema)) => Ok((decoded, schema, false)),
        Err(raw_error) if raw.len() >= REVIT_STORED_PAGE_BYTES => {
            let stripped = strip_revit_page_checksums(raw);
            match decode_schema_attempt(&stripped) {
                Ok((decoded, schema)) => Ok((decoded, schema, true)),
                Err(stripped_error) => Err(schema_retry_error(&raw_error, &stripped_error).into()),
            }
        }
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error).into()),
    }
}

fn decode_schema_attempt(stored: &[u8]) -> Result<(rvt_container::DecodedStream, Schema), String> {
    let decoded =
        decode_known_framing(stored, DEFAULT_DECODE_LIMIT).map_err(|error| error.to_string())?;
    let schema = Schema::parse(&decoded.payload).map_err(|error| error.to_string())?;
    Ok((decoded, schema))
}

/// # Errors
///
/// Fails where neither the raw stream nor the checksum-stripped one decodes
/// as an element table.
pub fn decode_elem_table_stream(
    raw: &[u8],
) -> Result<(rvt_container::DecodedStream, ElemTable, bool), Box<dyn Error>> {
    let has_complete_pages = raw.len() >= REVIT_STORED_PAGE_BYTES;
    let prepared = if has_complete_pages {
        strip_revit_page_checksums(raw)
    } else {
        raw.to_vec()
    };
    let (decoded, table) = decode_elem_table_attempt(&prepared)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((decoded, table, has_complete_pages))
}

fn decode_elem_table_attempt(
    stored: &[u8],
) -> Result<(rvt_container::DecodedStream, ElemTable), String> {
    let decoded =
        decode_known_framing(stored, DEFAULT_DECODE_LIMIT).map_err(|error| error.to_string())?;
    let table = ElemTable::parse(&decoded.payload).map_err(|error| error.to_string())?;
    Ok((decoded, table))
}

/// Candidate element identifiers from `Global/ElemTable`, or `None` when the
/// container has no such stream.
///
/// # Errors
///
/// Fails where the stream is present but does not decode.
pub fn elem_table_ids(container: &RvtContainer) -> Result<Option<BTreeSet<u32>>, Box<dyn Error>> {
    if container.stream("Global/ElemTable").is_none() {
        return Ok(None);
    }
    let raw = container.read_stream_with_limit("Global/ElemTable", DEFAULT_DECODE_LIMIT as u64)?;
    let (_, table, _) = decode_elem_table_stream(&raw)?;
    Ok(Some(
        table
            .records
            .iter()
            .flat_map(|record| [record.id, record.original_id])
            .filter(|id| *id != 0 && *id != u32::MAX)
            .collect(),
    ))
}

/// One `Global/*` stream's decoded payload, or `None` where the container has
/// no such stream.
///
/// # Errors
///
/// Fails where the stream is present but its framing does not decode.
pub fn decode_global_stream(
    container: &RvtContainer,
    name: &str,
) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    if container.stream(name).is_none() {
        return Ok(None);
    }
    let raw = container.read_stream_with_limit(name, DEFAULT_DECODE_LIMIT as u64)?;
    let prepared = if raw.len() >= REVIT_STORED_PAGE_BYTES {
        strip_revit_page_checksums(&raw)
    } else {
        raw
    };
    Ok(Some(
        decode_known_framing(&prepared, DEFAULT_DECODE_LIMIT)?.payload,
    ))
}

/// Every element's own Revit `UniqueId`, in network byte order, keyed by
/// element id.
///
/// This is the identity Revit's own IFC export writes as an element's
/// `GlobalId`, so recovering it is what lets a converted model be joined to
/// Revit's by GUID rather than only by the element id in `Tag`. It is built
/// from two streams: `Global/ElemTable` gives each element the episode it was
/// created in, and `Global/History` gives that episode's GUID, which the
/// element's id is exclusive-ORed into. See
/// [`rvt_model::EpisodeTable::element_unique_id`].
///
/// An element whose creation episode the history does not carry is left out
/// rather than given a guessed identity - 673 of AR S1's 11 738 products.
/// A container missing either stream yields an empty map, which is not an
/// error: the exporter falls back to its own deterministic identity.
///
/// # Errors
///
/// Fails where a stream is present but its framing does not decode.
pub fn authored_unique_ids(
    container: &RvtContainer,
) -> Result<BTreeMap<u32, [u8; 16]>, Box<dyn Error>> {
    let (Some(history), Some(table)) = (
        decode_global_stream(container, "Global/History")?,
        decode_global_stream(container, "Global/ElemTable")?,
    ) else {
        return Ok(BTreeMap::new());
    };
    let (Some(episodes), Ok(table)) = (
        rvt_model::EpisodeTable::parse(&history),
        ElemTable::parse(&table),
    ) else {
        return Ok(BTreeMap::new());
    };
    Ok(table
        .records
        .iter()
        .filter_map(|record| {
            Some((
                record.id,
                episodes.element_unique_id(record.creation_episode, record.id)?,
            ))
        })
        .collect())
}

/// Visit every member that carries a usable descriptor, in partition order,
/// threading the continuation carry between them.
///
/// A record whose body runs past the end of its own member is completed from
/// the members that follow before the visitor sees it, so a body is either
/// whole or absent and no caller has to know that members are a transport
/// unit. The framing already says exactly how many bytes are owing
/// (`MemberWalk::trailing_deficit`) and exactly where they are - the head of
/// the next member, which is what its own walk skips as `leading_carry` - so
/// this is the framing already measured, applied rather than reported.
///
/// # Errors
///
/// Fails where a partition cannot be read or a member cannot be inflated
/// inside `max_member_bytes`.
pub fn for_each_member(
    container: &RvtContainer,
    partition_paths: &[String],
    max_member_bytes: u64,
    mut visit: impl FnMut(&str, &rvt_container::PartitionMember, u32, RecordLayout, &MemberWalk, &[u8]),
) -> Result<(), Box<dyn Error>> {
    for_each_member_batch(
        container,
        partition_paths,
        max_member_bytes,
        |path, batch| {
            for member in batch {
                visit(
                    path,
                    member.member,
                    member.format_tag,
                    member.layout,
                    &member.walk,
                    &member.payload,
                );
            }
        },
    )
}

/// Read every member of every named partition, mapping each on its own thread
/// and folding the results in member order.
///
/// This is [`for_each_member`] for a pass whose per-member work depends only on
/// that member. The mapping runs in parallel and the folding does not, so the
/// result is the same whatever the machine: `map` sees one member and cannot
/// see the pass so far, and `fold` sees the mapped values in the order the
/// members are stored.
///
/// Resolving a member is still sequential, and has to be: a record spilling out
/// of one member changes where the next member's walk starts, and completing
/// one reads the members that follow it.
///
/// # Errors
///
/// Fails where a partition cannot be inventoried or a member cannot be
/// inflated.
pub fn map_members<T: Send>(
    container: &RvtContainer,
    partition_paths: &[String],
    max_member_bytes: u64,
    map: impl Fn(&ResolvedMember<'_>) -> T + Sync,
    mut fold: impl FnMut(&str, T),
) -> Result<(), Box<dyn Error>> {
    map_members_with(
        container,
        partition_paths,
        max_member_bytes,
        map,
        |path, _, value| {
            fold(path, value);
        },
    )
}

/// [`map_members`], with the member itself handed to the fold beside what the
/// mapping made of it.
///
/// A pass that reads a member's records into values and then decides what to
/// keep of them needs both: the values, which were made on another thread, and
/// the records they were made from.
///
/// # Errors
///
/// Fails where a partition cannot be inventoried or a member cannot be
/// inflated.
pub fn map_members_with<T: Send>(
    container: &RvtContainer,
    partition_paths: &[String],
    max_member_bytes: u64,
    map: impl Fn(&ResolvedMember<'_>) -> T + Sync,
    mut fold: impl FnMut(&str, &ResolvedMember<'_>, T),
) -> Result<(), Box<dyn Error>> {
    for_each_member_batch(
        container,
        partition_paths,
        max_member_bytes,
        |path, batch| {
            for (member, value) in batch.iter().zip(parallel_map(batch, &map)) {
                fold(path, member, value);
            }
        },
    )
}

/// One member of a partition, inflated and framed, ready to be read.
pub struct ResolvedMember<'a> {
    pub member: &'a rvt_container::PartitionMember,
    pub format_tag: u32,
    pub layout: RecordLayout,
    pub walk: MemberWalk,
    pub payload: Vec<u8>,
}

/// Read every member of every named partition, a batch at a time.
///
/// Batching is what lets the expensive halves of the read - inflating a member
/// and walking its record array - happen on several threads while the order
/// the caller sees stays the order the members are stored in. Resolving the
/// batch is sequential for the reason [`map_members`] gives.
fn for_each_member_batch(
    container: &RvtContainer,
    partition_paths: &[String],
    max_member_bytes: u64,
    mut visit_batch: impl FnMut(&str, &[ResolvedMember<'_>]),
) -> Result<(), Box<dyn Error>> {
    for partition_path in partition_paths {
        // The whole stored stream, once, so that inflating a member borrows
        // nothing but bytes. Everything below - the inventory, the lookahead
        // and the parallel prepare - reads it instead of reopening the CFB
        // stream, which resolves the sector chain from its start on every
        // buffer refill. A partition too large to hold is read the old way
        // rather than refused.
        let stored = container
            .read_partition_bytes(partition_path, PARTITION_IN_MEMORY_LIMIT)
            .ok();
        let report = match stored.as_deref() {
            Some(bytes) => container.inspect_partition_bytes(
                partition_path,
                bytes,
                PartitionReadOptions::default(),
            )?,
            None => container.inspect_partition(partition_path, PartitionReadOptions::default())?,
        };
        let mut carry = 0_u64;
        // Members decoded ahead of the walk to complete a spilled record. Each
        // is decoded once: the lookahead leaves it here and the walk takes it
        // when it arrives.
        let mut ahead: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        let mut at = 0_usize;
        while at < report.members.len() {
            let end = (at + MEMBER_PREPARE_BATCH).min(report.members.len());
            let mut prepared = match stored.as_deref() {
                Some(bytes) => prepare_members(bytes, &report.members[at..end], max_member_bytes),
                None => (at..end).map(|_| None).collect(),
            };
            let mut batch: Vec<ResolvedMember<'_>> = Vec::with_capacity(end - at);
            for (offset, member) in report.members[at..end].iter().enumerate() {
                let index = at + offset;
                let Some(descriptor) = member.descriptor else {
                    ahead.remove(&index);
                    carry = 0;
                    continue;
                };
                let Some(layout) = RecordLayout::from_format_tag(descriptor.format_tag) else {
                    ahead.remove(&index);
                    carry = 0;
                    continue;
                };
                let ready = prepared.get_mut(offset).and_then(Option::take);
                let (mut payload, ready_walk) = match (ahead.remove(&index), ready) {
                    // The lookahead already inflated this member to finish an
                    // earlier record; its bytes and the prepared ones are the
                    // same bytes, and taking the ones already in hand keeps the
                    // promise that a member is inflated once for the walk.
                    (Some(payload), _) => (payload, None),
                    (None, Some(ready)) => (ready.payload, ready.walk),
                    (None, None) => (
                        container.decode_partition_member(
                            partition_path,
                            member.logical_offset,
                            max_member_bytes,
                        )?,
                        None,
                    ),
                };
                let leading_carry = usize::try_from(carry).unwrap_or(usize::MAX);
                // The prepared walk was taken with no carry, which is what all
                // but a few members have. Where a record did spill into this
                // one the walk starts elsewhere and is retaken here.
                let walk = match ready_walk {
                    Some(walk) if leading_carry == 0 => Some(walk),
                    _ => MemberWalk::parse(&payload, layout, leading_carry).ok(),
                };
                let Some(walk) = walk else {
                    carry = 0;
                    continue;
                };
                if walk.trailing_deficit > 0 {
                    complete_spilled_record(
                        container,
                        partition_path,
                        stored.as_deref(),
                        &report.members,
                        index,
                        walk.trailing_deficit,
                        max_member_bytes,
                        &mut payload,
                        &mut ahead,
                    )?;
                }
                carry = walk.trailing_deficit;
                batch.push(ResolvedMember {
                    member,
                    format_tag: descriptor.format_tag,
                    layout,
                    walk,
                    payload,
                });
            }
            visit_batch(partition_path, &batch);
            at = end;
        }
    }
    Ok(())
}

/// Largest partition stream held in memory to inflate its members from.
///
/// Above this the members are inflated one at a time from the file, as they
/// always were: a conversion of a file this project has never seen should get
/// slower, not fail.
const PARTITION_IN_MEMORY_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
/// Members resolved, and mapped, in one batch.
///
/// The batch bounds what is held at once - the members themselves, and
/// whatever a mapping pass makes of them, which for the record read is a
/// decode per record with its geometry in it. Small enough that a batch of a
/// geometry-heavy partition stays tens of megabytes, large enough that every
/// thread has several members of any partition to work on.
const MEMBER_PREPARE_BATCH: usize = 192;

/// One member inflated ahead of the walk, with the walk it takes when no
/// record spilled into it.
struct PreparedMember {
    payload: Vec<u8>,
    /// The record array, walked with no leading carry. `None` where that walk
    /// failed, which the caller reads as the failure it always was.
    walk: Option<MemberWalk>,
}

/// Threads to spread a batch over: what the machine reports, and never more
/// than there is work for.
fn batch_threads(work: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(work.max(1))
}

/// Apply `map` to every item, on as many threads as the machine has, and hand
/// the results back in the order the items came in.
///
/// A panic inside `map` is carried out to this thread rather than swallowed,
/// so a failure reads the same way it would have read in a sequential loop.
fn parallel_map<I: Sync, T: Send>(items: &[I], map: &(impl Fn(&I) -> T + Sync)) -> Vec<T> {
    let per_thread = items.len().div_ceil(batch_threads(items.len()));
    if per_thread == 0 {
        return Vec::new();
    }
    std::thread::scope(|scope| {
        let workers = items
            .chunks(per_thread)
            .map(|chunk| scope.spawn(move || chunk.iter().map(map).collect::<Vec<T>>()))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// Inflate a run of members and walk each one's record array, on as many
/// threads as the machine has.
///
/// Members are independent: each is its own gzip member at its own offset, and
/// its record array is read from its own payload. The one thing that is not
/// independent is a record spilling from one member into the next, which
/// changes where the receiving member's walk starts - so the walk here is
/// taken with no carry, and [`for_each_member_batch`] retakes it for the few
/// members that need one. Nothing is decided here: the results are handed back
/// in member order and a failure is left as a failure for the caller to read.
fn prepare_members(
    stored: &[u8],
    members: &[rvt_container::PartitionMember],
    max_member_bytes: u64,
) -> Vec<Option<PreparedMember>> {
    parallel_map(members, &|member: &rvt_container::PartitionMember| {
        let descriptor = member.descriptor?;
        let layout = RecordLayout::from_format_tag(descriptor.format_tag)?;
        let payload = rvt_container::decode_partition_member_bytes(
            stored,
            member.logical_offset,
            max_member_bytes,
        )
        .ok()?;
        let walk = MemberWalk::parse(&payload, layout, 0).ok();
        Some(PreparedMember { payload, walk })
    })
}

/// Append the bytes a record left owing to `payload`, taken from the members
/// that follow it.
///
/// The tail sits at the head of the next member, and a record long enough
/// spans several, so bytes are taken in member order until the deficit is
/// covered. Nothing is appended unless all of it is found: a body completed in
/// part is not the record, and the visitor reads a short body as no body,
/// which is what happened to every spilled record before this existed.
///
/// Each member decoded here is left in `ahead` for the walk to take when it
/// reaches it, so a member is never decoded twice.
#[allow(clippy::too_many_arguments)]
fn complete_spilled_record(
    container: &RvtContainer,
    partition_path: &str,
    stored: Option<&[u8]>,
    members: &[rvt_container::PartitionMember],
    at: usize,
    deficit: u64,
    max_member_bytes: u64,
    payload: &mut Vec<u8>,
    ahead: &mut BTreeMap<usize, Vec<u8>>,
) -> Result<(), Box<dyn Error>> {
    let Ok(owing) = usize::try_from(deficit) else {
        return Ok(());
    };
    let mut following = Vec::new();
    let mut have = 0_usize;
    let mut next = at + 1;
    while have < owing {
        // A member with no descriptor is where the framing gives up on the
        // continuation - `member-framing` counts it as a dropped carry - so
        // the record ends here unfinished rather than being stitched across
        // a boundary the framing does not vouch for.
        let Some(member) = members
            .get(next)
            .filter(|member| member.descriptor.is_some())
        else {
            return Ok(());
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = ahead.entry(next) {
            slot.insert(match stored {
                Some(stored) => rvt_container::decode_partition_member_bytes(
                    stored,
                    member.logical_offset,
                    max_member_bytes,
                )?,
                None => container.decode_partition_member(
                    partition_path,
                    member.logical_offset,
                    max_member_bytes,
                )?,
            });
        }
        let Some(decoded) = ahead.get(&next) else {
            return Ok(());
        };
        have += decoded.len();
        following.push(next);
        next += 1;
    }
    if let Some(tail) = spilled_tail(
        owing,
        following
            .iter()
            .filter_map(|at| ahead.get(at).map(Vec::as_slice)),
    ) {
        payload.extend_from_slice(&tail);
    }
    Ok(())
}

/// The `owing` bytes a record is short, read off the head of the members that
/// follow it, or `None` when they do not hold that many between them.
///
/// A member hands the tail of a record to the next member's head, which is
/// exactly the run the receiving member's own walk skips as `leading_carry`,
/// so the bytes are consecutive across as many members as the record needs.
fn spilled_tail<'a>(owing: usize, following: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    let mut tail = Vec::with_capacity(owing);
    for payload in following {
        let taken = (owing - tail.len()).min(payload.len());
        tail.extend_from_slice(payload.get(..taken)?);
        if tail.len() == owing {
            return Some(tail);
        }
    }
    None
}

/// Properties naming the type an element is an instance of, tried in order.
///
/// There is no one such property: `Element` declares none, and a class names
/// its type under whatever its own chain calls it. `m_masterSymbolId` is a
/// `FamilyInstance` property and covers loadable families; a system family
/// carries its own - `RbsCurve.m_idType` for a pipe or duct. Each is read by
/// [`rvt_model::record_declared_id`] out of the record's own header, so a class
/// that declares none of them simply has no type reference and a class that
/// declares one cannot pick up another's.
///
/// Measured, not guessed, on SMALL. All 10 011 `FamilyInstance` records that
/// carry a value name an element that is a `FamilySymbol` - 10 011 of 10 011,
/// where a misread field would land on one class or another at random - and
/// they name only 226 distinct ones, the type-to-instance ratio a real type
/// reference has. Two independent checks agree with it: on the 70 instances
/// whose symbol was separately verified by matching the instance's bounds
/// against the symbol's transformed box, the declared property names that same
/// symbol in 70 of 70; and every instance shares a family with the type it
/// names, 250 of 250 where both carry one.
///
/// Reading it replaces inferring the type from geometry, which covered only the
/// 198 instances whose bounds could be matched. The bounds-verified symbol
/// stays as the fallback, and where both are present they agree.
///
/// It is *not* the same thing as `GInstance`'s symbol, which is the symbol
/// whose geometry a record instantiates: across all 7 055 records carrying one
/// the two agree in only 7.4% of cases, because a record's geometry is usually
/// instantiated from a nested symbol rather than from the instance's own type.
/// Only on the bounds-verified subset, where the `GInstance` symbol is proven
/// to be this instance's, do they agree everywhere.
///
/// `RbsCurve.m_idType` was measured the same way; see the working notes for its
/// numbers.
///
/// A compound host object names its type under an `...AttributesId` property of
/// its own chain, and the schema declares exactly five: `VWall`, `Floor`,
/// `RoofBase`, `Ceiling` and `HostInfill`. All five are candidates here, each
/// read by the same `record_declared_id`, so a class picks up only the one its
/// own chain declares.
///
/// `VWall.m_WallAttributesId` is the wall's, measured the same way on AR S1 and
/// S2 - the first architecture files the reader has seen - and then against
/// Revit's own IFC export of those same models, which is an answer this project
/// did not produce. Its 13 208 / 13 851 values on `SWall` name 111 / 116
/// distinct elements and every one of them is a `WallType` descendant:
/// `BasicWallType` 13 153 / 13 801, `WallAttributes` 40 / 40,
/// `NewCurtainWallType` 15 / 10, three classes of one chain out of the file's
/// 4 418. Joined to the reference export by element id, all 7 617 / 7 739 walls
/// Revit exports have a type here, and it is the type Revit names for 7 615 /
/// 7 739 of them. `openrvt layers` is the instrument and
/// `scripts/compare_wall_layers.py` is the join.
const TYPE_ELEMENT_ID_PROPERTIES: &[&str] = DECLARED_ID_PROPERTIES
    .split_at(TYPE_ELEMENT_ID_PROPERTY_COUNT)
    .0;

/// How many of [`DECLARED_ID_PROPERTIES`] name a type. They are its leading
/// run, in the order a class picks from.
const TYPE_ELEMENT_ID_PROPERTY_COUNT: usize = 7;

/// `FamilySymbol.m_familyId`, the family a loadable type belongs to.
const FAMILY_ID_PROPERTY: &str = "m_familyId";
/// `FamilyBase.m_categoryId`, the category that family is of.
const CATEGORY_ID_PROPERTY: &str = "m_categoryId";
/// `InsertableInst.m_hostId`, the element an insert is cut into.
const HOST_ID_PROPERTY: &str = "m_hostId";
/// `DesignOption.m_DesignOptionSetId`, the set an option belongs to.
const DESIGN_OPTION_SET_ID_PROPERTY: &str = "m_DesignOptionSetId";
/// `DesignOptionSet.m_mainDesignOption`, the one option of a set that is part
/// of the model. Every other option of the set is an alternative to it.
const MAIN_DESIGN_OPTION_PROPERTY: &str = "m_mainDesignOption";

/// Every declared identifier property read out of a record's own header, in
/// one walk. The type candidates come first so their order is
/// [`TYPE_ELEMENT_ID_PROPERTIES`]'s and a class picks the first it declares.
const DECLARED_ID_PROPERTIES: &[&str] = &[
    "m_masterSymbolId",
    "m_idType",
    "m_WallAttributesId",
    "m_floorAttributesId",
    "m_roofAttributesId",
    "m_ceilingAttributesId",
    "m_AttributesId",
    FAMILY_ID_PROPERTY,
    CATEGORY_ID_PROPERTY,
    DESIGN_OPTION_SET_ID_PROPERTY,
    MAIN_DESIGN_OPTION_PROPERTY,
    HOST_ID_PROPERTY,
];

/// The value read for one of [`DECLARED_ID_PROPERTIES`].
#[must_use]
fn declared_id(values: &[Option<i32>], property: &str) -> Option<i32> {
    let at = DECLARED_ID_PROPERTIES
        .iter()
        .position(|candidate| *candidate == property)?;
    *values.get(at)?
}

/// Fold this record's own reading of [`TYPE_ELEMENT_ID_PROPERTIES`] into
/// `entry`, unconditionally - unlike every other declared id, which keeps
/// whichever record was visited first.
///
/// A duplicate record of one element in an earlier partition can carry a
/// stale type reference from before the element's family type was last
/// changed: AR S1 has real pairs where the earlier partition's
/// `m_masterSymbolId` names a door 100 mm narrower than the later
/// partition's, and only the later one matches `s1_revit.ifc`. So the record
/// from the latest partition processed always wins here, not the first one
/// seen - the opposite of `declared_id`'s callers just below this.
fn fold_type_element_id(entry: &mut ExportedElement, declared: &[Option<i32>]) {
    if let Some((property, id)) = TYPE_ELEMENT_ID_PROPERTIES
        .iter()
        .zip(declared)
        .find_map(|(property, id)| Some((*property, (*id)?)))
    {
        entry.type_element_id = Some(id);
        entry.type_element_property = Some(property);
    }
}

/// One element as it is emitted to JSON.
// A flat record of what the file said about one element, so each flag is an
// independent reading and there is no state here for them to be folded into.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct ExportedElement {
    pub class_index: Option<u16>,
    pub category: Option<i32>,
    /// Where [`ExportedElement::category`] came from: `"declared"` when the
    /// element's own record carried it, `"symbol"` when it was taken from the
    /// symbol its bounds verified it against. See [`attach_symbol_bounds`].
    pub category_source: Option<&'static str>,
    pub header_family_id: Option<i32>,
    pub level_id: Option<i32>,
    pub family_id: Option<i32>,
    pub owner_view_id: Option<i32>,
    pub created_phase_id: Option<i32>,
    pub design_option_id: Option<i32>,
    /// `Element.m_unplacedOwnerId`: the element that owns this one while it is
    /// not placed in the model in its own right. On the corpus it names a
    /// group definition and nothing else; see `stands_in_the_model`.
    pub unplaced_owner_id: Option<i32>,
    /// `InsertableInst.m_hostId`: the element this insert is cut into.
    pub host_id: Option<i32>,
    /// `DesignOption.m_DesignOptionSetId` when this element is an option.
    pub design_option_set_id: Option<i32>,
    /// `DesignOptionSet.m_mainDesignOption` when this element is an option set.
    pub main_design_option_id: Option<i32>,
    /// `Plane.m_origin[2]` for a `Level`, in Revit internal feet.
    pub elevation_feet: Option<f64>,
    /// A `GeoSite`'s own latitude, longitude and elevation. See
    /// [`rvt_model::GeoSiteFields`] for why a project can carry more than one.
    pub geo_site: Option<rvt_model::GeoSiteFields>,
    /// Which named location the project is placed by, from the single record
    /// that declares it. See [`rvt_model::ActiveGeoLocationFields`].
    pub active_geo_location: Option<rvt_model::ActiveGeoLocationFields>,
    /// A `GeoLocation`'s own frame, relative to the project's coordinates.
    pub geo_location_placement: Option<GInstanceTransformFields>,
    /// A `MaterialElem`'s shading colour. See [`rvt_model::MaterialColorFields`].
    pub material_color: Option<rvt_model::MaterialColorFields>,
    /// First readable string in the body, with how it was located.
    pub name: Option<(String, &'static str)>,
    pub parameters: Vec<rvt_model::Parameter>,
    /// The element this one is an instance of, from its own declarations.
    pub type_element_id: Option<i32>,
    /// Which of [`TYPE_ELEMENT_ID_PROPERTIES`] carried it, so a report can
    /// score each candidate property on its own.
    pub type_element_property: Option<&'static str>,
    /// `FamilySymbol.m_familyId`: the family a type belongs to.
    pub family_element_id: Option<i32>,
    /// `FamilyBase.m_categoryId`: the category a family is of. A loadable
    /// family's category lives here and nowhere else - neither the instance
    /// nor its type declares one - so this is the only route to it.
    pub declared_category_id: Option<i32>,
    /// The layer table this element carries when it is a compound host
    /// object's type. See [`rvt_model::CompoundStructure`].
    pub compound_structures: Vec<rvt_model::CompoundStructure>,
    /// Parameters read from the record of this element's type. See
    /// `inherit_symbol_parameters`.
    pub type_parameters: Vec<rvt_model::Parameter>,
    /// Forge spec carried by this element when it defines a parameter.
    pub parameter_spec: Option<String>,
    pub pipe_line_candidate: Option<PipeLineGeometryFields>,
    pub fitting_center_line_candidate: Option<FittingCenterLineFields>,
    pub fitting_axis_candidate: Option<FittingCenterLineFields>,
    pub family_instance_placement_candidates: Vec<FamilyInstancePlacementFields>,
    pub family_instance_placement: Option<FamilyInstancePlacementFields>,
    /// The placement this element's `GInstance` declares, from
    /// [`GInstanceTransformFields::from_instance_info`].
    pub ginstance_transform: Option<GInstanceTransformFields>,
    /// What the byte scan the declared reading replaced would have found.
    /// Measurement only - see [`GeometryStatistics`] - so the two readings can
    /// be compared on the corpus rather than one being asserted to cover the
    /// other.
    pub scanned_ginstance_transform: Option<GInstanceTransformFields>,
    /// How many `InstInfoBase` placements this element's records declare
    /// between them. More than one means the element is placed as several
    /// instances and no single transform describes it.
    pub declared_instance_placements: usize,
    /// The placements themselves, from the last record that declared any -
    /// the same record whose box became `placement_bounds`, so the two can be
    /// asked about each other. See `report_nested_placements`.
    pub declared_placements: Vec<GInstanceTransformFields>,
    /// The box of the record that supplied [`ExportedElement::ginstance_transform`].
    ///
    /// `ginstance_transform` keeps the *first* record's single placement and
    /// `placement_bounds` keeps the *last* record's box, so on an id whose
    /// records declare a placement more than once between them those come
    /// from two different records - the same crossing the body-to-box pairing
    /// a few lines below is careful to avoid, and it was costing the symbol
    /// link every one of SMALL's 819 refusals. This is set in the same step
    /// as the transform, so the two are always one record's.
    pub instance_placement_bounds: Option<GElementBounds>,
    pub geometry_graph: Option<GElementGraphFields>,
    pub geometry_bounds: Option<GElementBounds>,
    pub placement_bounds: Option<GElementBounds>,
    pub verified_symbol_bounds: Option<VerifiedSymbolBounds>,
    /// A symbol whose body is already in the model's project coordinates and
    /// belongs to this element: see [`attach_placed_symbol_bodies`]. Held
    /// apart from `verified_symbol_bounds` because nothing here is placed -
    /// the body is where it already was - so the two say different things.
    pub placed_symbol_body: Option<u32>,
    /// The boundary representation decoded from this id's own `GElement`
    /// record, in its own local frame and Revit internal feet. Populated for
    /// any id that carries one - typically a `FamilySymbol` - and looked up
    /// by an instance through its verified symbol id, not copied per instance.
    pub brep: Option<rvt_model::SymbolBrep>,
    /// How many of this id's `GElement` records yielded a body. More than one
    /// means the rest were passed over by [`keep_body`].
    pub brep_records: usize,
    /// Whether [`ExportedElement::brep`] reproduces the box of the same record
    /// it was decoded from - see [`body_placement_box`] for which box that is.
    /// That box is the one the placement chain already trusts - a symbol link
    /// is accepted when the instance's box agrees with the symbol's carried
    /// through its transform - so a body reproducing it is in the same frame
    /// as the box: already placed, needing no symbol and no transform.
    pub brep_is_placed: bool,
    /// A cut-face pass found a different body on the same box rather than the
    /// kept body with faces removed. The kept body's topology may still call
    /// it closed, but only its own face loops may license a closed-shell claim
    /// after that contradiction; see [`place_body_less_its_cut_faces`].
    pub brep_requires_loop_closure: bool,
    /// What each box on the record the kept body came from says about it. See
    /// [`BodyBoxResiduals`]: measurement for a possible second placement tier.
    pub brep_box_residuals: BodyBoxResiduals,
    /// The box [`ExportedElement::brep`] was judged against, from the same
    /// record as that body. Kept so a near miss can be anatomised - a body
    /// that is the box's size but somewhere else is a transform we do not
    /// read, one smaller than its box is geometry we did not decode - without
    /// crossing one record's body with another record's box.
    pub brep_placement_box: Option<GElementBounds>,
    /// `m_moribund` from the `Element` tail: the element is marked deleted.
    pub moribund: bool,
    pub locked: bool,
    pub source: Option<(usize, usize, usize)>,
    pub record_count: usize,
}

impl ExportedElement {
    /// Whether the element's *own* record declared a category.
    ///
    /// That is what separates a type or definition from an instance, and it is
    /// the reading the export's selection rests on. A category reached through
    /// the element's family or through a bounds-verified symbol is not a
    /// declaration and must not be read as one - the whole point of those two
    /// is to give an instance the category it does not declare.
    #[must_use]
    pub fn declares_a_category(&self) -> bool {
        self.category_source == Some("declared")
    }

    /// The element this one is an instance of: its declared type reference,
    /// or - for a record whose declarations did not yield one - the symbol its
    /// bounds were verified against. See [`TYPE_ELEMENT_ID_PROPERTIES`].
    #[must_use]
    pub fn type_element_reference(&self) -> Option<u32> {
        self.type_element_id
            .and_then(|id| u32::try_from(id).ok())
            .or_else(|| {
                self.verified_symbol_bounds
                    .map(|symbol| symbol.symbol_element_id)
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VerifiedSymbolBounds {
    pub symbol_element_id: u32,
    pub bounds: GElementBounds,
    /// The same extent in the model's own project coordinates, decoded from
    /// the instance's record rather than computed from `bounds`: the link was
    /// accepted only because the two agree to 1e-8 ft through the instance's
    /// transform, so this is the placed box with no rotation hull taken.
    pub instance_bounds: GElementBounds,
}

/// Per-class string calibration plus the identifiers of parameter elements.
type ExportContext = (BTreeMap<u16, NameCalibration>, BTreeSet<i32>);

/// Where one class keeps its first readable string, and how consistently.
#[derive(Debug, Default)]
pub struct NameCalibration {
    /// Offsets relative to the end of the `Element` tail, and how often each
    /// was the first readable string.
    pub offsets: BTreeMap<usize, u64>,
    pub bodies: u64,
    pub samples: Vec<String>,
}

impl NameCalibration {
    /// The offset the class agrees on, if the agreement is strong enough.
    #[must_use]
    pub fn settled_offset(&self) -> Option<usize> {
        let (offset, count) = self.offsets.iter().max_by_key(|(_, count)| **count)?;
        (count * 100 >= self.bodies * NAME_OFFSET_AGREEMENT).then_some(*offset)
    }

    #[must_use]
    pub fn agreement(&self) -> u64 {
        self.offsets
            .values()
            .max()
            .map_or(0, |count| count * 100 / self.bodies.max(1))
    }
}

/// Collect, in one pass, where each class keeps its string and which elements
/// are parameter definitions.
///
/// # Errors
///
/// Fails where a partition member cannot be read or inflated.
pub fn calibrate_names(
    container: &RvtContainer,
    schema: Option<&Schema>,
    partition_paths: &[String],
    max_member_bytes: u64,
) -> Result<ExportContext, Box<dyn Error>> {
    let parameter_classes = parameter_class_indexes(schema);
    let mut parameter_ids = BTreeSet::new();
    let mut calibrations: BTreeMap<u16, NameCalibration> = BTreeMap::new();
    // One member's records say nothing about another's, so each member is read
    // on its own thread and the readings are added in member order. Counting
    // and set membership do not care about that order; the three samples a
    // class keeps do, and folding in order is what keeps them the same three.
    map_members(
        container,
        partition_paths,
        max_member_bytes,
        |member| read_member_calibration(member, &parameter_classes),
        |_, read| {
            parameter_ids.extend(read.parameter_ids);
            for (class_index, found) in read.calibrations {
                let entry = calibrations.entry(class_index).or_default();
                entry.bodies += found.bodies;
                for (offset, count) in found.offsets {
                    *entry.offsets.entry(offset).or_default() += count;
                }
                for sample in found.samples {
                    if entry.samples.len() < NAME_SAMPLES_KEPT {
                        entry.samples.push(sample);
                    }
                }
            }
        },
    )?;
    Ok((calibrations, parameter_ids))
}

/// Samples of the string a class writes, kept per class so a report can show
/// what the settled offset actually reads.
const NAME_SAMPLES_KEPT: usize = 3;

/// What one member contributes to the calibration.
struct MemberCalibration {
    calibrations: Vec<(u16, NameCalibration)>,
    parameter_ids: Vec<i32>,
}

/// Read one member's element records: where each class puts its first readable
/// string, and which records are a parameter's definition.
///
/// This reads one member and nothing else, which is what lets the pass run a
/// member per thread. Everything it returns is added to the pass's totals by
/// the fold in [`calibrate_names`].
fn read_member_calibration(
    member: &ResolvedMember<'_>,
    parameter_classes: &BTreeSet<u16>,
) -> MemberCalibration {
    let mut calibrations: BTreeMap<u16, NameCalibration> = BTreeMap::new();
    let mut parameter_ids = Vec::new();
    if member.format_tag != ELEMENT_CLASS_FORMAT_TAG {
        return MemberCalibration {
            calibrations: Vec::new(),
            parameter_ids,
        };
    }
    let payload = &member.payload;
    for record in &member.walk.records {
        let Some(header) = RecordHeader::parse(payload, record, member.layout) else {
            continue;
        };
        let body = payload
            .get(record.body_offset()..record.end())
            .unwrap_or_default();
        if parameter_classes.contains(&header.class_index) {
            if let Ok(id) = i32::try_from(header.id) {
                parameter_ids.push(id);
            }
        }
        let Some(fields) = ElementFields::parse(body, header.id) else {
            continue;
        };
        let tail_end = fields.id_offset + 4 + ELEMENT_TAIL_BYTES;
        let entry = calibrations.entry(header.class_index).or_default();
        entry.bodies += 1;
        if let Some(found) = RecordString::scan_from(body, tail_end) {
            *entry.offsets.entry(found.offset - tail_end).or_default() += 1;
            if entry.samples.len() < NAME_SAMPLES_KEPT {
                entry.samples.push(found.value);
            }
        }
    }
    MemberCalibration {
        calibrations: calibrations.into_iter().collect(),
        parameter_ids,
    }
}

/// Schema classes whose elements define parameters.
#[must_use]
fn parameter_class_indexes(schema: Option<&Schema>) -> BTreeSet<u16> {
    schema
        .map(|schema| {
            schema
                .classes
                .iter()
                .filter(|class| class.name.starts_with("Param"))
                .map(|class| class.index)
                .collect()
        })
        .unwrap_or_default()
}

/// What one `GFace` declares about itself, beside its geometry.
///
/// Every field here is read from the object's own declarations in the order
/// the schema lists them: `GNode.m_GInfo` contributes `m_tag`,
/// `m_controlCommand`, `m_categoryId` and the alternate `m_flags`, and `GFace`
/// itself contributes `m_cutType`, the alternate `m_faceFlags_v9` and
/// `m_renderStyleId`. Nothing is searched for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GFaceMarks {
    pub tag: i32,
    pub cut_type: i32,
    pub render_style_id: i32,
    pub info_flags: i64,
    pub face_flags: i64,
}

impl GFaceMarks {
    /// Read them off a `Face` object, or `None` when the walk did not deliver
    /// every field - a truncated object is not a face declaring zeroes.
    #[must_use]
    pub fn read(object: &rvt_model::SerialObject) -> Option<Self> {
        let [
            tag,
            _control_command,
            _category_id,
            cut_type,
            render_style_id,
        ] = object.integers[..].try_into().ok()?;
        let [info_flags, face_flags] = object.alternate_integers[..].try_into().ok()?;
        Some(Self {
            tag,
            cut_type,
            render_style_id,
            info_flags,
            face_flags,
        })
    }
}

impl std::fmt::Display for GFaceMarks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tag={} cutType={} renderStyle={} flags=0x{:x} faceFlags=0x{:x}",
            self.tag, self.cut_type, self.render_style_id, self.info_flags, self.face_flags
        )
    }
}

#[allow(clippy::too_many_lines)] // One streaming pass keeps large RVT payloads out of memory.
/// # Errors
///
/// Fails where the file is not a readable Revit container, or where a
/// partition member cannot be inflated inside `max_member_bytes`.
pub fn recover_elements(
    path: &Path,
    max_member_bytes: u64,
) -> Result<RecoveredElements, Box<dyn Error>> {
    let container = RvtContainer::open(path)?;
    let release = read_basic_file_info(&container)?.and_then(|info| info.revit_version);
    let catalog = release.and_then(Catalog::for_release);
    let schema = read_schema(&container)?;
    let header_class_index = schema_class_index(schema.as_ref(), ELEMENT_HEADER_CLASS);
    let level_class_index = schema_class_index(schema.as_ref(), "Level");
    let plane_class_index = schema_class_index(schema.as_ref(), "Plane");
    let geo_site_class_index = schema_class_index(schema.as_ref(), "GeoSite");
    // Where the project stands in shared coordinates: one record says which
    // named location is active, and the location itself carries the frame.
    let active_geo_location_class_index =
        schema_class_index(schema.as_ref(), "ActiveGeoLocationTrackingElement");
    let geo_location_class_index = schema_class_index(schema.as_ref(), "GeoLocation");
    let material_elem_class_index = schema_class_index(schema.as_ref(), "MaterialElem");
    let pipe_curve_class_index = schema_class_index(schema.as_ref(), "RbsPipeCurve");
    let family_instance_class_index = schema_class_index(schema.as_ref(), "FamilyInstance");
    let curve_driver_class_index = schema_class_index(schema.as_ref(), "RbsCurveDriver");
    let pipe_fitting_center_line_class_index =
        schema_class_index(schema.as_ref(), "PipeFittingCenterLine");
    let gline_class_index = schema_class_index(schema.as_ref(), "GLine");
    let ginstance_class_index = schema_class_index(schema.as_ref(), "GInstance");
    // `InstInfoBase` is what declares the placement itself - `m_Trf`,
    // `m_symbolId`, `m_GRepId` - and `InstanceInfo` is the subclass the
    // records actually write, so the reading is narrowed to the base class and
    // accepts anything descending from it.
    let inst_info_base_class_index = schema_class_index(schema.as_ref(), "InstInfoBase");
    let gnode_class_index = schema_class_index(schema.as_ref(), "GNode");
    let geometry_element_class_index = schema_class_index(schema.as_ref(), "GElement");
    let parameter_set_classes = parameter_set_class_indexes(schema.as_ref());
    // A loadable family keeps its parameters in `FamilyParams` rather than in
    // the four typed sets. See [`rvt_model::read_family_parameters`].
    let family_parameter_classes = schema
        .as_ref()
        .and_then(rvt_model::FamilyParameterClassIndexes::detect);
    let compound_structure_classes = schema
        .as_ref()
        .and_then(rvt_model::CompoundStructureClassIndexes::detect);
    let brep_body_class_indices = brep_body_classes(schema.as_ref());
    let brep_classes = brep_class_indexes(schema.as_ref());
    let partition_paths = partition_paths(&container);
    let (calibrations, parameter_ids) = calibrate_names(
        &container,
        schema.as_ref(),
        &partition_paths,
        max_member_bytes,
    )?;
    // Everything the read of a record needs, gathered once. Reading a record
    // is then a function of that record, which is what lets a member's records
    // be read on every core at once and folded here in the order they are
    // stored. See `RecordContext`.
    let context = RecordContext {
        schema: schema.as_ref(),
        catalog,
        calibrations: &calibrations,
        parameter_ids: &parameter_ids,
        header_class_index,
        level_class_index,
        plane_class_index,
        geo_site_class_index,
        active_geo_location_class_index,
        geo_location_class_index,
        material_elem_class_index,
        pipe_curve_class_index,
        family_instance_class_index,
        curve_driver_class_index,
        pipe_fitting_center_line_class_index,
        gline_class_index,
        ginstance_class_index,
        inst_info_base_class_index,
        gnode_class_index,
        geometry_element_class_index,
        parameter_set_classes,
        family_parameter_classes,
        compound_structure_classes,
        brep_body_class_indices,
        brep_classes,
    };
    let mut elements: BTreeMap<u32, ExportedElement> = BTreeMap::new();

    for (partition_index, partition_path) in partition_paths.iter().enumerate() {
        map_members_with(
            &container,
            std::slice::from_ref(partition_path),
            max_member_bytes,
            |member| decode_member_records(&context, member),
            |_, member, decoded| {
                for (record, decoded) in member.walk.records.iter().zip(decoded) {
                    let Some(decoded) = decoded else {
                        continue;
                    };
                    let header = decoded.header;
                    let entry = elements.entry(header.id).or_default();
                    entry.record_count += 1;

                    if let Some(geometry) = decoded.geometry {
                        let exact_bounds = geometry.exact_bounds;
                        let graph_bounds = geometry.graph_bounds;
                        let placement_bounds = geometry.placement_bounds;
                        if let Some(bounds) = exact_bounds {
                            entry.geometry_bounds = Some(bounds);
                        }
                        entry.geometry_graph = geometry.graph;
                        if let Some(bounds) = placement_bounds {
                            entry.placement_bounds = Some(bounds);
                            if context.ginstance_class_index.is_some() {
                                entry.scanned_ginstance_transform =
                                    geometry.scanned_ginstance_transform;
                            }
                        }
                        let declared = geometry.declared_placements;
                        entry.declared_instance_placements += declared.len();
                        if !declared.is_empty() {
                            entry.declared_placements.clone_from(&declared);
                        }
                        // One placement is the case every consumer here is
                        // written for: the element is that instance. A record
                        // declaring several is a real thing - a nested family
                        // writes one per sub-instance - and is counted rather
                        // than resolved, because picking one of them would be
                        // picking arbitrarily.
                        if let [only] = declared[..] {
                            // Not `get_or_insert`: the box has to come from
                            // the record that supplied the transform, so both
                            // are set in the one step or neither is.
                            if entry.ginstance_transform.is_none() {
                                entry.ginstance_transform = Some(only);
                                entry.instance_placement_bounds = placement_bounds;
                            }
                        }
                        if let Some(read) = geometry.body {
                            // Counted as well as kept: one id can carry more
                            // than one body-bearing record.
                            entry.brep_records += 1;
                            let PlacedBody {
                                brep,
                                placed,
                                requires_loop_closure,
                                placement_box,
                            } = read;
                            if keep_body(placed, entry) {
                                let body = record.body_in(&member.payload);
                                entry.brep_placement_box = placement_box;
                                entry.brep_box_residuals = BodyBoxResiduals {
                                    exact: exact_bounds.and_then(|bounds| {
                                        body_bounds_residual_feet(&brep, &bounds)
                                    }),
                                    graph: graph_bounds.and_then(|bounds| {
                                        body_bounds_residual_feet(&brep, &bounds)
                                    }),
                                    // Scanned only where there is no exact
                                    // block to compare against, which is both
                                    // the population that could gain by it and
                                    // the only one worth the cost of a whole-
                                    // body scan.
                                    near_duplicate: exact_bounds
                                        .is_none()
                                        .then(|| GElementBounds::parse_near_duplicate(body))
                                        .flatten()
                                        .and_then(|bounds| {
                                            body_bounds_residual_feet(&brep, &bounds)
                                        }),
                                    graph_from_exact: exact_bounds.zip(graph_bounds).map(
                                        |(exact, graph)| {
                                            exact
                                                .min
                                                .into_iter()
                                                .chain(exact.max)
                                                .zip(graph.min.into_iter().chain(graph.max))
                                                .map(|(left, right)| (left - right).abs())
                                                .fold(0.0_f64, f64::max)
                                        },
                                    ),
                                };
                                entry.brep_is_placed = placed;
                                entry.brep_requires_loop_closure = requires_loop_closure;
                                entry.brep = Some(brep);
                            }
                        }
                    }

                    if let Some(fields) = decoded.header_fields {
                        if entry.category.is_none() && fields.category.is_some() {
                            entry.category = fields.category;
                            entry.category_source = Some("declared");
                        }
                        entry.header_family_id = entry.header_family_id.or(fields.family_id);
                    }
                    if let Some(element) = decoded.element {
                        let ElementDecode {
                            declared_parameters,
                            family_parameters,
                            declared_name,
                            declared_ids: declared,
                            compound_structures,
                            fields,
                            elevation_feet,
                            geo_site,
                            active_geo_location,
                            geo_location_placement,
                            material_color,
                            pipe_line_candidate,
                            fitting_center_line_candidate,
                            parameter_spec,
                        } = *element;
                        entry.class_index = Some(header.class_index);
                        entry.source = Some((partition_index, member.member.index, record.offset));
                        if entry.parameters.is_empty() {
                            if let Some(found) = declared_parameters {
                                entry.parameters = found.parameters;
                            }
                            // A loadable family keeps its parameters in
                            // `FamilyParams` and a system family in the four
                            // typed sets, so a record may hold either or both.
                            // A value the typed sets already stated stands:
                            // this adds what they did not carry rather than
                            // restating what they did.
                            let stated = entry
                                .parameters
                                .iter()
                                .map(|parameter| parameter.id)
                                .collect::<BTreeSet<_>>();
                            entry.parameters.extend(
                                family_parameters
                                    .into_iter()
                                    .filter(|parameter| !stated.contains(&parameter.id)),
                            );
                        }
                        if entry.name.is_none() {
                            entry.name = declared_name.map(|name| (name, "declared"));
                        }
                        fold_type_element_id(entry, &declared);
                        if entry.family_element_id.is_none()
                            || entry.declared_category_id.is_none()
                            || entry.design_option_set_id.is_none()
                            || entry.main_design_option_id.is_none()
                            || entry.host_id.is_none()
                        {
                            entry.family_element_id = entry
                                .family_element_id
                                .or_else(|| declared_id(&declared, FAMILY_ID_PROPERTY));
                            entry.declared_category_id = entry
                                .declared_category_id
                                .or_else(|| declared_id(&declared, CATEGORY_ID_PROPERTY));
                            entry.design_option_set_id = entry
                                .design_option_set_id
                                .or_else(|| declared_id(&declared, DESIGN_OPTION_SET_ID_PROPERTY));
                            entry.main_design_option_id = entry
                                .main_design_option_id
                                .or_else(|| declared_id(&declared, MAIN_DESIGN_OPTION_PROPERTY));
                            entry.host_id = entry
                                .host_id
                                .or_else(|| declared_id(&declared, HOST_ID_PROPERTY));
                        }
                        if entry.compound_structures.is_empty() {
                            entry.compound_structures = compound_structures;
                        }
                        if let Some(read) = fields {
                            let ElementFieldsDecode {
                                fields,
                                scanned_name,
                                scanned_parameters,
                                family_instance_placement_candidates,
                            } = read;
                            if entry.name.is_none() {
                                entry.name = scanned_name;
                            }
                            if entry.parameters.is_empty() {
                                if let Some(found) = scanned_parameters {
                                    entry.parameters = found.parameters;
                                }
                            }
                            if Some(header.class_index) == context.family_instance_class_index {
                                entry.family_instance_placement_candidates =
                                    family_instance_placement_candidates;
                            }
                            entry.moribund |= fields.moribund;
                            entry.locked |= fields.locked;
                            entry.level_id = entry.level_id.or(fields.assoc_level_id);
                            entry.family_id = entry.family_id.or(fields.family_id);
                            entry.owner_view_id = entry.owner_view_id.or(fields.owner_view_id);
                            entry.created_phase_id =
                                entry.created_phase_id.or(fields.created_phase_id);
                            entry.design_option_id =
                                entry.design_option_id.or(fields.design_option_id);
                            entry.unplaced_owner_id =
                                entry.unplaced_owner_id.or(fields.unplaced_owner_id);
                        }
                        if let Some(elevation_feet) = elevation_feet {
                            entry.elevation_feet = Some(elevation_feet);
                        }
                        if let Some(geo_site) = geo_site {
                            entry.geo_site = Some(geo_site);
                        }
                        if let Some(active) = active_geo_location {
                            entry.active_geo_location = Some(active);
                        }
                        if let Some(placement) = geo_location_placement {
                            entry.geo_location_placement = Some(placement);
                        }
                        if let Some(material_color) = material_color {
                            entry.material_color = Some(material_color);
                        }
                        if Some(header.class_index) == context.pipe_curve_class_index
                            && context.curve_driver_class_index.is_some()
                        {
                            entry.pipe_line_candidate = pipe_line_candidate;
                        }
                        if Some(header.class_index) == context.pipe_fitting_center_line_class_index
                            && context.gline_class_index.is_some()
                        {
                            entry.fitting_center_line_candidate = fitting_center_line_candidate;
                        }
                        if i32::try_from(header.id)
                            .is_ok_and(|id| context.parameter_ids.contains(&id))
                        {
                            entry.parameter_spec = parameter_spec;
                        }
                    }
                }
            },
        )?;
    }

    attach_symbol_bounds(&mut elements);
    attach_placed_symbol_bodies(&mut elements);
    attach_fitting_axes(&mut elements, catalog);
    verify_family_instance_placements(&mut elements);
    inherit_symbol_names(&mut elements);
    inherit_symbol_parameters(&mut elements);
    inherit_family_categories(&mut elements, schema.as_ref());

    let (parameter_names, parameter_specs) = parameter_metadata(&elements);
    Ok(RecoveredElements {
        release,
        catalog,
        parameter_values_schema_bound: catalog.is_some() && parameter_set_classes.is_some(),
        schema,
        partition_paths,
        parameter_names,
        parameter_specs,
        elements,
        authored_unique_ids: authored_unique_ids(&container)?,
        document_identity: document_identity(&container)?,
    })
}

/// Everything the read of one record needs besides the record: the class
/// indexes the schema resolved once, the parameter identifiers the first pass
/// collected, and the release catalogue.
///
/// Gathered into one value so that reading a record is a function of that
/// record, which is what lets the records of a batch be read on every core at
/// once. Nothing here changes while the pass runs.
struct RecordContext<'a> {
    schema: Option<&'a Schema>,
    catalog: Option<Catalog>,
    calibrations: &'a BTreeMap<u16, NameCalibration>,
    parameter_ids: &'a BTreeSet<i32>,
    header_class_index: Option<u16>,
    level_class_index: Option<u16>,
    plane_class_index: Option<u16>,
    geo_site_class_index: Option<u16>,
    active_geo_location_class_index: Option<u16>,
    geo_location_class_index: Option<u16>,
    material_elem_class_index: Option<u16>,
    pipe_curve_class_index: Option<u16>,
    family_instance_class_index: Option<u16>,
    curve_driver_class_index: Option<u16>,
    pipe_fitting_center_line_class_index: Option<u16>,
    gline_class_index: Option<u16>,
    ginstance_class_index: Option<u16>,
    inst_info_base_class_index: Option<u16>,
    gnode_class_index: Option<u16>,
    geometry_element_class_index: Option<u16>,
    parameter_set_classes: Option<ParameterSetClassIndexes>,
    family_parameter_classes: Option<rvt_model::FamilyParameterClassIndexes>,
    compound_structure_classes: Option<rvt_model::CompoundStructureClassIndexes>,
    brep_body_class_indices: Vec<u16>,
    brep_classes: Option<rvt_model::BrepClassIndexes>,
}

impl RecordContext<'_> {
    /// Whether a stored parameter identifier is one the file defines.
    fn accept_parameter(&self, id: i32) -> bool {
        self.parameter_ids.contains(&id)
    }

    /// The same question where a release catalogue can vouch for the built-in
    /// identifiers, which are negative and are not defined by the file.
    fn accept_verified_parameter(&self, id: i32) -> bool {
        if id < 0 {
            self.catalog
                .is_some_and(|catalog| catalog.built_in_parameter(id).is_some())
        } else {
            self.parameter_ids.contains(&id)
        }
    }
}

/// One record read into the values the pass may keep from it.
///
/// This is the whole of what reading a record costs, and none of it depends on
/// the pass so far - which is the point: the fold in [`recover_elements`] still
/// decides what to keep, with the guards it always had, and no longer does the
/// reading itself. A value the guards turn out not to want was read anyway,
/// which is what a parallel pass trades for the wall time it returns.
struct RecordDecode {
    header: RecordHeader,
    /// Present for a `GElement` record, which is the one that carries a solid.
    geometry: Option<GeometryDecode>,
    /// Present for an `ElementHeader` record.
    header_fields: Option<ElementHeaderFields>,
    /// Present for a record in a member whose descriptor says it carries the
    /// element's own class.
    element: Option<Box<ElementDecode>>,
}

/// What a `GElement` record's body states about where it is and what it holds.
struct GeometryDecode {
    exact_bounds: Option<GElementBounds>,
    graph: Option<GElementGraphFields>,
    graph_bounds: Option<GElementBounds>,
    placement_bounds: Option<GElementBounds>,
    scanned_ginstance_transform: Option<GInstanceTransformFields>,
    /// The placements the record's own `InstInfoBase` objects declare.
    declared_placements: Vec<GInstanceTransformFields>,
    /// The solid the record assembles, already placed against its own box.
    body: Option<PlacedBody>,
}

/// A solid read out of one record, and whether its own box placed it.
struct PlacedBody {
    brep: rvt_model::SymbolBrep,
    placed: bool,
    requires_loop_closure: bool,
    placement_box: Option<GElementBounds>,
}

/// What an element-class record's body states about the element.
struct ElementDecode {
    declared_parameters: Option<ParameterSets>,
    /// What a loadable family stores on this record. See
    /// [`rvt_model::read_family_parameters`].
    family_parameters: Vec<rvt_model::Parameter>,
    declared_name: Option<String>,
    declared_ids: Vec<Option<i32>>,
    compound_structures: Vec<rvt_model::CompoundStructure>,
    fields: Option<ElementFieldsDecode>,
    elevation_feet: Option<f64>,
    geo_site: Option<rvt_model::GeoSiteFields>,
    /// Which named location the project is placed by, from the one record
    /// that declares it. See [`rvt_model::ActiveGeoLocationFields`].
    active_geo_location: Option<rvt_model::ActiveGeoLocationFields>,
    /// The transform a `GeoLocation` carries: the frame that location's
    /// coordinates stand in, relative to the project's own.
    geo_location_placement: Option<GInstanceTransformFields>,
    material_color: Option<rvt_model::MaterialColorFields>,
    pipe_line_candidate: Option<PipeLineGeometryFields>,
    fitting_center_line_candidate: Option<FittingCenterLineFields>,
    parameter_spec: Option<String>,
}

/// What the fixed `Element` tail locates, and the readings that start from it.
struct ElementFieldsDecode {
    fields: ElementFields,
    scanned_name: Option<(String, &'static str)>,
    scanned_parameters: Option<ParameterSets>,
    family_instance_placement_candidates: Vec<FamilyInstancePlacementFields>,
}

/// Read every record of one member.
///
/// The result is in record order and holds one entry per record the header
/// parse accepted, so the fold can walk the member's records and its decodes
/// together.
fn decode_member_records(
    context: &RecordContext<'_>,
    member: &ResolvedMember<'_>,
) -> Vec<Option<RecordDecode>> {
    member
        .walk
        .records
        .iter()
        .map(|record| {
            let header = RecordHeader::parse(&member.payload, record, member.layout)?;
            let body = record.body_in(&member.payload);
            Some(RecordDecode {
                header,
                geometry: (Some(header.class_index) == context.geometry_element_class_index)
                    .then(|| decode_geometry(context, header, body)),
                header_fields: (Some(header.class_index) == context.header_class_index)
                    .then(|| ElementHeaderFields::parse(body))
                    .flatten(),
                element: (Some(header.class_index) != context.header_class_index
                    && member.format_tag == ELEMENT_CLASS_FORMAT_TAG)
                    .then(|| Box::new(decode_element(context, header, body))),
            })
        })
        .collect()
}

/// Read a `GElement` record's body: its boxes, the node graph under it, the
/// placements it declares and the solid it assembles.
fn decode_geometry(
    context: &RecordContext<'_>,
    header: RecordHeader,
    body: &[u8],
) -> GeometryDecode {
    let exact_bounds = GElementBounds::parse(body);
    let graph = context.gnode_class_index.and_then(|gnode_class_index| {
        GElementGraphFields::parse(body, |class_index| {
            context
                .schema
                .is_some_and(|schema| schema_class_is_a(schema, class_index, gnode_class_index))
        })
    });
    // The declared box first. `GRep.m_bBox` is read at its own offset now, so
    // it is the record's box as the format states it; the duplicated-block
    // scan below it is a search for the same field, and a search can land on a
    // different pair of blocks entirely in a record whose two boxes differ.
    let placement_bounds = graph
        .as_ref()
        .map(|graph| graph.bounds)
        .or(exact_bounds)
        .or_else(|| GElementBounds::parse_near_duplicate(body));
    let graph_bounds = graph.as_ref().map(|graph| graph.bounds);
    let scanned_ginstance_transform = placement_bounds.and_then(|bounds| {
        context
            .ginstance_class_index
            .and_then(|ginstance_class_index| {
                GInstanceTransformFields::parse(body, ginstance_class_index, &bounds)
            })
    });
    let mut declared_placements = Vec::new();
    let mut body_read = None;
    if let (Some(schema), Some(classes)) = (context.schema, context.brep_classes.as_ref()) {
        let (_walk, objects) = rvt_model::walk_record_collecting(schema, header.class_index, body);
        if let Some(base) = context.inst_info_base_class_index {
            declared_placements = objects
                .iter()
                .filter(|object| schema_class_is_a(schema, object.class_index, base))
                .filter_map(GInstanceTransformFields::from_instance_info)
                .collect::<Vec<_>>();
        }
        let brep =
            rvt_model::assemble_symbol_brep(&objects, classes, &context.brep_body_class_indices);
        if !brep.is_empty() {
            // Paired with the bounds block of *this* record. Reading it off
            // the element instead would cross one record's body with another's
            // box: on AR S1, 13 208 wall ids carry 25 486 body-bearing records
            // between them.
            let placement_box = body_placement_box(exact_bounds, graph_bounds);
            let (brep, placed) = place_declared_body(brep, placement_box.as_ref());
            // Second pass, where the first left something other than a closed
            // solid: either nothing placed, or what placed does not bound a
            // volume by its own loops because the file joined a face to it
            // that the record's box does not bound. See
            // `place_body_less_its_cut_faces`; a body that places and closes
            // is never re-read, so nothing that is a solid today can change.
            let mut requires_loop_closure = false;
            let (brep, placed) = if placed && brep.bounds_a_volume() {
                (brep, placed)
            } else {
                match cut_face_trim(
                    &objects,
                    classes,
                    &context.brep_body_class_indices,
                    placement_box.as_ref(),
                    &brep,
                ) {
                    CutFaceTrim::Accepted(trimmed) => (*trimmed, true),
                    CutFaceTrim::DifferentBody => {
                        // The source's topology calls the kept body closed,
                        // while its own loops do not. A second body landing on
                        // the same box is the contradiction that makes the
                        // topological claim unsafe for this record alone.
                        requires_loop_closure = true;
                        (brep, placed)
                    }
                    CutFaceTrim::Refused => (brep, placed),
                }
            };
            body_read = Some(PlacedBody {
                brep,
                placed,
                requires_loop_closure,
                placement_box,
            });
        }
    }
    GeometryDecode {
        exact_bounds,
        graph,
        graph_bounds,
        placement_bounds,
        scanned_ginstance_transform,
        declared_placements,
        body: body_read,
    }
}

/// Read the parts of an element record that its fixed `Element` tail locates:
/// the fallback name, the fallback parameter scan, and the placement
/// candidates a family instance carries.
///
/// Both fallbacks are read only where the declarations did not answer, which
/// is the one case the fold in [`recover_elements`] can reach them in.
fn decode_element_fields(
    context: &RecordContext<'_>,
    header: RecordHeader,
    body: &[u8],
    fields: ElementFields,
    declared_parameters: Option<&ParameterSets>,
    declared_name: Option<&String>,
) -> ElementFieldsDecode {
    let tail_end = fields.id_offset + 4 + ELEMENT_TAIL_BYTES;
    ElementFieldsDecode {
        fields,
        // Read only where the declarations did not answer, which is the
        // one case the fold can reach it in.
        scanned_name: declared_name
            .is_none()
            .then(|| {
                read_name(
                    body,
                    tail_end,
                    context
                        .calibrations
                        .get(&header.class_index)
                        .and_then(NameCalibration::settled_offset),
                )
            })
            .flatten(),
        // The scan stays as the fallback for a record the walk cannot
        // reach the sets in - which is a record whose declared read
        // yielded no parameter, not merely one that yielded no set.
        scanned_parameters: declared_parameters
            .is_none_or(|found| found.parameters.is_empty())
            .then(|| {
                if let (Some(classes), Some(_)) = (context.parameter_set_classes, context.catalog) {
                    ParameterSets::scan_schema_bound(
                        body,
                        tail_end,
                        fields.id_offset,
                        classes,
                        &|id| context.accept_verified_parameter(id),
                    )
                } else {
                    ParameterSets::scan(body, &|id| context.accept_parameter(id))
                }
            })
            .flatten(),
        family_instance_placement_candidates: if Some(header.class_index)
            == context.family_instance_class_index
        {
            FamilyInstancePlacementFields::candidates(body, tail_end)
        } else {
            Vec::new()
        },
    }
}

/// Read an element-class record's body: the parameters, name and identifiers
/// its declarations state, and the readings its fixed tail locates.
/// The frame a `GeoLocation` record carries, off the `InstInfoBase` in its own
/// node stream - the same declaration a family instance's placement is read
/// from, rather than a scan for twelve doubles. `None` for any other class.
fn read_geo_location_placement(
    context: &RecordContext<'_>,
    header: RecordHeader,
    body: &[u8],
) -> Option<GInstanceTransformFields> {
    (Some(header.class_index) == context.geo_location_class_index).then_some(())?;
    let schema = context.schema?;
    let base = context.inst_info_base_class_index?;
    let (_walk, objects) = rvt_model::walk_record_collecting(schema, header.class_index, body);
    objects
        .iter()
        .filter(|object| schema_class_is_a(schema, object.class_index, base))
        .find_map(GInstanceTransformFields::from_instance_info)
}

fn decode_element(context: &RecordContext<'_>, header: RecordHeader, body: &[u8]) -> ElementDecode {
    // The declarations name the four parameter sets outright, so they are read
    // from the walk rather than searched for, and without waiting on the
    // heuristic that locates the element's fixed tail.
    let (declared_parameters, family_parameters) = (|| {
        let classes = context.parameter_set_classes?;
        Some(rvt_model::read_record_parameters(
            context.schema?,
            header.class_index,
            body,
            classes,
            context.family_parameter_classes,
        ))
    })()
    .unwrap_or((None, Vec::new()));
    // The declarations name the property a record's name is the value of, so
    // it is read rather than scanned for. The scan stays as the fallback for a
    // record whose class declares no such property.
    let declared_name = context
        .schema
        .and_then(|schema| rvt_model::record_name(schema, header.class_index, body));
    // The type an element is an instance of is a declared property, so it is
    // read rather than inferred from geometry. See `TYPE_ELEMENT_ID_PROPERTIES`.
    let declared_ids = context.schema.map_or_else(Vec::new, |schema| {
        rvt_model::record_declared_ids(schema, header.class_index, body, DECLARED_ID_PROPERTIES)
    });
    // A compound host object's type owns its layer table, and `HostObjAttr` is
    // the class that declares it, so the read is bound to that chain rather
    // than to a list of type classes.
    let compound_structures = match (context.schema, context.compound_structure_classes) {
        (Some(schema), Some(classes))
            if rvt_model::descends_from(
                schema,
                header.class_index,
                rvt_model::HOST_OBJECT_ATTRIBUTES_CLASS_NAME,
            ) =>
        {
            rvt_model::CompoundStructure::from_record(schema, header.class_index, body, classes)
        }
        _ => Vec::new(),
    };
    let fields = ElementFields::parse(body, header.id).map(|fields| {
        decode_element_fields(
            context,
            header,
            body,
            fields,
            declared_parameters.as_ref(),
            declared_name.as_ref(),
        )
    });
    ElementDecode {
        declared_parameters,
        family_parameters,
        declared_name,
        declared_ids,
        compound_structures,
        fields,
        elevation_feet: (Some(header.class_index) == context.level_class_index)
            .then(|| {
                context
                    .plane_class_index
                    .and_then(|plane_index| LevelFields::parse(body, plane_index))
                    .map(|fields| fields.elevation_feet)
            })
            .flatten(),
        geo_site: (Some(header.class_index) == context.geo_site_class_index)
            .then(|| {
                context
                    .schema
                    .and_then(|schema| GeoSiteFields::parse(schema, header.class_index, body))
            })
            .flatten(),
        active_geo_location: (Some(header.class_index)
            == context.active_geo_location_class_index)
            .then(|| {
                context.schema.and_then(|schema| {
                    rvt_model::ActiveGeoLocationFields::parse(schema, header.class_index, body)
                })
            })
            .flatten(),
        // A `GeoLocation` is an `Instance`, so where it stands is the `m_Trf`
        // of the `InstInfoBase` in its own node stream - read exactly the way
        // a family instance's placement is, off the declaration rather than
        // by scanning for twelve doubles.
        geo_location_placement: read_geo_location_placement(context, header, body),
        material_color: (Some(header.class_index) == context.material_elem_class_index)
            .then(|| {
                context.schema.and_then(|schema| {
                    rvt_model::MaterialColorFields::parse(schema, header.class_index, body)
                })
            })
            .flatten(),
        pipe_line_candidate: (Some(header.class_index) == context.pipe_curve_class_index)
            .then(|| {
                context
                    .curve_driver_class_index
                    .and_then(|index| PipeLineGeometryFields::parse(body, index))
            })
            .flatten(),
        fitting_center_line_candidate: (Some(header.class_index)
            == context.pipe_fitting_center_line_class_index)
            .then(|| {
                context
                    .gline_class_index
                    .and_then(|index| FittingCenterLineFields::parse(body, index))
            })
            .flatten(),
        parameter_spec: i32::try_from(header.id)
            .is_ok_and(|id| context.parameter_ids.contains(&id))
            .then(|| ParameterSpec::scan(body).map(|spec| spec.type_id))
            .flatten(),
    }
}

/// Give a loadable family's elements the category their family declares.
///
/// A loadable family's category is on the family and nowhere else: neither the
/// instance nor its type declares one, which is why 187 131 elements carry a
/// category in this decode and not one of them is a product Revit exports. The
/// route is `FamilyInstance.m_masterSymbolId` -> `FamilySymbol.m_familyId` ->
/// `FamilyBase.m_categoryId`, all three declared identifier properties read out
/// of each record's own header.
///
/// The family end is gated on the class chain rather than on the property name.
/// Sixty-four classes declare an `m_categoryId` and most of them mean something
/// else by it - a schedule's filter, a style's owner - so only a record that is
/// a family is allowed to answer. It cannot reach anything else anyway, since
/// the only ids consulted are those a symbol names as its family, but the gate
/// keeps that a rule instead of a coincidence.
///
/// It only adds: an element that already has a category keeps it, so nothing
/// measured before this can move.
pub fn inherit_family_categories(
    elements: &mut BTreeMap<u32, ExportedElement>,
    schema: Option<&Schema>,
) {
    let family_category = elements
        .iter()
        .filter(|(_, element)| {
            element.class_index.is_some_and(|index| {
                schema.is_some_and(|schema| rvt_model::descends_from(schema, index, "FamilyBase"))
            })
        })
        .filter_map(|(id, element)| Some((*id, element.declared_category_id?)))
        .collect::<BTreeMap<_, _>>();
    let symbol_category = elements
        .iter()
        .filter_map(|(id, element)| {
            let family = u32::try_from(element.family_element_id?).ok()?;
            Some((*id, *family_category.get(&family)?))
        })
        .collect::<BTreeMap<_, _>>();
    let inherited = elements
        .iter()
        .filter(|(_, element)| element.category.is_none())
        .filter_map(|(id, element)| {
            let category = symbol_category
                .get(id)
                .or_else(|| symbol_category.get(&element.type_element_reference()?))?;
            Some((*id, *category))
        })
        .collect::<Vec<_>>();
    for (id, category) in inherited {
        if let Some(element) = elements.get_mut(&id) {
            element.category = Some(category);
            element.category_source = Some("family");
        }
    }

    // A system family has no `FamilyBase` record to reach: a pipe's type is an
    // `RbsPipeType`, and it declares the category itself. So an element that
    // still has none takes the one its own declared type declares - the same
    // hop as above with the family step absent, and the only route by which a
    // pipe, a duct or a cable tray is ever named as one.
    //
    // Not where the element's class already answers. `SWall` types a wall by
    // itself and no mapping row mentions `OST_Walls`, so handing a wall the
    // category its type declares turns it into a proxy: measured against
    // Revit's own export of AR S1, 1 638 walls, 79 slabs and 3 roofs lost
    // their entity that way and agreement fell 95.2% -> 80.0%. With this
    // clause the same run is 95.2% -> 95.2% on S1 and 95.8% -> **96.6%** on
    // S2, the gain being 89 railings that reach their family's category
    // through their type for the first time.
    let types_itself_by_class = |element: &ExportedElement| {
        let class_name = element.class_index.and_then(|index| {
            schema
                .and_then(|schema| schema.class_by_index(index))
                .map(|class| class.name.as_str())
        });
        element_type_for_source(class_name, None) != BimElementType::Unknown
    };
    let type_category = elements
        .iter()
        .filter(|(_, element)| element.declares_a_category())
        .filter_map(|(id, element)| Some((*id, element.category?)))
        .collect::<BTreeMap<_, _>>();
    let inherited = elements
        .iter()
        .filter(|(_, element)| element.category.is_none() && !types_itself_by_class(element))
        .filter_map(|(id, element)| {
            Some((*id, *type_category.get(&element.type_element_reference()?)?))
        })
        .collect::<Vec<_>>();
    for (id, category) in inherited {
        if let Some(element) = elements.get_mut(&id) {
            element.category = Some(category);
            element.category_source = Some("type");
        }
    }
}

/// The design options that are alternatives to the model rather than part of
/// it.
///
/// A design option set names the one of its options that is in the model,
/// `DesignOptionSet.m_mainDesignOption`; the rest are alternatives to it, and
/// what stands in them is not what the file describes as built. Both hops are
/// declared identifier properties read out of each record's own header, so an
/// option whose set cannot be reached is simply not called secondary - this
/// only ever subtracts what it can name.
#[must_use]
fn secondary_design_options(elements: &BTreeMap<u32, ExportedElement>) -> BTreeSet<i32> {
    let main_option = elements
        .iter()
        .filter_map(|(id, element)| Some((*id, element.main_design_option_id?)))
        .collect::<BTreeMap<_, _>>();
    elements
        .iter()
        .filter_map(|(id, element)| {
            let set = u32::try_from(element.design_option_set_id?).ok()?;
            let option = i32::try_from(*id).ok()?;
            (*main_option.get(&set)? != option).then_some(option)
        })
        .collect()
}

/// Give an instance the name of the symbol it was verified against.
///
/// An instance's own declarations carry no name property - in Revit its name is
/// its type's - so `FamilySymbol`'s declared `SymbolInfo.m_name` is the one to
/// use, reached through the instance's declared type reference (see
/// [`TYPE_ELEMENT_ID_PROPERTIES`]) or, failing that, the symbol its bounds were
/// verified against. It takes precedence over a scanned name, which for an
/// instance is whatever string the body happened to hold first, and stands
/// aside for a declared one.
fn inherit_symbol_names(elements: &mut BTreeMap<u32, ExportedElement>) {
    let symbol_names = elements
        .iter()
        .filter_map(|(id, element)| {
            let (name, source) = element.name.as_ref()?;
            (*source == "declared").then(|| (*id, name.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for element in elements.values_mut() {
        if element
            .name
            .as_ref()
            .is_some_and(|(_, source)| *source == "declared")
        {
            continue;
        }
        let Some(symbol) = element.type_element_reference() else {
            continue;
        };
        if let Some(name) = symbol_names.get(&symbol) {
            element.name = Some((name.clone(), "symbol"));
        }
    }
}

/// Give an instance the parameters stored on the symbol it was verified
/// against.
///
/// An element's own record carries only what was set on the instance - around
/// three values for a typical family instance in the corpus. The rest are
/// stored once on its type, and the symbol verified by bounds is the same link
/// `inherit_symbol_names` already trusts for the name.
///
/// They are kept in their own list rather than merged into the element's, so
/// that "this element carries this value" and "every element of this type
/// carries this value" stay distinguishable downstream. A parameter the
/// element already sets is not inherited: an instance value overrides its
/// type's, and no corpus body was found where both are written, so the rule
/// only guards against a duplicate rather than resolving a measured conflict.
fn inherit_symbol_parameters(elements: &mut BTreeMap<u32, ExportedElement>) {
    let symbol_parameters = elements
        .iter()
        .filter(|(_, element)| !element.parameters.is_empty())
        .map(|(id, element)| (*id, element.parameters.clone()))
        .collect::<BTreeMap<_, _>>();
    for element in elements.values_mut() {
        let Some(symbol) = element.type_element_reference() else {
            continue;
        };
        let Some(parameters) = symbol_parameters.get(&symbol) else {
            continue;
        };
        let own = element
            .parameters
            .iter()
            .map(|parameter| parameter.id)
            .collect::<BTreeSet<_>>();
        element.type_parameters = parameters
            .iter()
            .filter(|parameter| !own.contains(&parameter.id))
            .cloned()
            .collect();
    }
}

fn verify_family_instance_placements(elements: &mut BTreeMap<u32, ExportedElement>) {
    for element in elements.values_mut() {
        let Some(bounds) = element.placement_bounds else {
            continue;
        };
        let mut inside = element
            .family_instance_placement_candidates
            .iter()
            .copied()
            .filter(|candidate| bounds.contains_point(candidate.origin));
        let Some(candidate) = inside.next() else {
            continue;
        };
        if inside.next().is_none() {
            element.family_instance_placement = Some(candidate);
        }
    }
}

/// Link an element to a symbol whose body is *already placed*, where the
/// bounds cross-check has nothing to check.
///
/// [`attach_symbol_bounds`] verifies a placement by carrying the symbol's box
/// through the instance's transform and requiring the instance's own box back.
/// That is the right question for a family symbol drawn in its own local
/// frame. It is not the question here: these placements declare the identity
/// transform and name a symbol whose body reproduces its *own* record's box,
/// so the body is in the model's project coordinates already and there is no
/// transform to get wrong. Carrying an identity through and demanding the two
/// boxes match then refuses the link for a reason that has nothing to do with
/// placement - a stair run's box runs to the top of its storey while the
/// treads it is drawn from stop partway up, so the boxes differ in one
/// coordinate and agree in the other five.
///
/// What stands in for the cross-check is containment plus exclusivity: the
/// placed body lies inside the element's own declared box, and no other
/// element names that symbol. Neither is a placement claim, because none is
/// needed; together they say the body the file already put here is this
/// element's and nothing else's.
///
/// On AR S1 this links 35 elements, every one of which reaches the exporter
/// with no geometry without it: the 22 `StairsRun` and 12 `StairsLanding` of
/// the model's 11 stairs - whose treads and risers are a
/// `StairsTriserSymbol` of their own, 114 closed faces with a zero box
/// residual - and one family instance. Not one shares its symbol with another
/// element, and not one is refused by the containment test, so neither guard
/// is doing work it cannot be measured on; they are here because a body drawn
/// twice, or drawn on the wrong element, is worse than one not drawn.
fn attach_placed_symbol_bodies(elements: &mut BTreeMap<u32, ExportedElement>) {
    let mut named: BTreeMap<u32, usize> = BTreeMap::new();
    for element in elements.values() {
        if let Some(symbol) = element
            .ginstance_transform
            .and_then(|transform| transform.symbol_element_id)
        {
            *named.entry(symbol).or_default() += 1;
        }
    }
    let placed = elements
        .iter()
        .filter_map(|(id, element)| {
            element
                .brep_placement_box
                .filter(|_| element.brep_is_placed)
                .map(|bounds| (*id, bounds))
        })
        .collect::<BTreeMap<_, _>>();

    for element in elements.values_mut() {
        let Some(transform) = element.ginstance_transform else {
            continue;
        };
        if !declares_identity(&transform) {
            continue;
        }
        let Some(symbol_element_id) = transform.symbol_element_id else {
            continue;
        };
        if named.get(&symbol_element_id).copied() != Some(1) {
            continue;
        }
        let Some(symbol_box) = placed.get(&symbol_element_id) else {
            continue;
        };
        let Some(own_box) = element.placement_bounds else {
            continue;
        };
        if !own_box.contains_box(symbol_box) {
            continue;
        }
        element.placed_symbol_body = Some(symbol_element_id);
    }
}

/// Whether a placement is the identity as the file wrote it, not merely close
/// to one: a transform that moves the body at all is the cross-check's to verify.
fn declares_identity(transform: &GInstanceTransformFields) -> bool {
    let same = |ours: f64, theirs: f64| (ours - theirs).abs() <= f64::EPSILON;
    transform
        .origin
        .coordinates_feet
        .iter()
        .zip(IDENTITY_TRANSFORM.origin.coordinates_feet)
        .chain(
            transform
                .basis
                .iter()
                .flatten()
                .zip(IDENTITY_TRANSFORM.basis.into_iter().flatten()),
        )
        .all(|(ours, theirs)| same(*ours, theirs))
}

fn attach_symbol_bounds(elements: &mut BTreeMap<u32, ExportedElement>) {
    // Every element that carries a box, not only those of one class. What an
    // instance is placed from is whatever its `InstInfoBase.m_symbolId` names,
    // and on AR S1 that is a `FamilySymbol` for most of them but also a
    // `MasterImportSymbol`, a `SysMullionFamSym` and a `SysPanelFamSym`. The
    // link is verified by the boxes agreeing, which no unrelated element's box
    // does by chance to 1e-8 ft on all six coordinates, so the class of what
    // is named adds nothing to it.
    let symbols = elements
        .iter()
        .filter_map(|(id, element)| {
            let bounds = element.geometry_graph.as_ref()?.bounds;
            Some((*id, (element.category, bounds)))
        })
        .collect::<BTreeMap<_, _>>();

    for element in elements.values_mut() {
        let Some(transform) = element.ginstance_transform else {
            continue;
        };
        let Some(symbol_element_id) = transform.symbol_element_id else {
            continue;
        };
        let Some((symbol_category, symbol_bounds)) = symbols.get(&symbol_element_id).copied()
        else {
            continue;
        };
        // The box of the record that declared this transform, and only as a
        // fallback the id's last box. An id whose records declare a placement
        // more than once between them carries a box per record, and asking one
        // record's transform about another record's box is comparing two
        // records: on SMALL it refused 819 links, every one of which agrees
        // when the pair is kept together.
        let Some(instance_bounds) = element
            .instance_placement_bounds
            .or(element.placement_bounds)
        else {
            continue;
        };
        // The bounds cross-check is the verification: all six coordinates of
        // the instance's own independently decoded box must agree with the
        // symbol's box carried through the instance's rigid transform, to
        // within 1e-8 feet. Chance agreement is not a real possibility.
        if !instance_bounds.matches_transformed(&symbol_bounds, &transform) {
            continue;
        }
        // The categories are required not to *disagree*, rather than required
        // to be equal. Measured: among the links the cross-check accepts,
        // every one where both sides carry a category has the same category on
        // both - on BIG that is every accepted link without exception - so
        // demanding equality only ever rejected links where one side's
        // category was not recovered. That cost 2 491 / 1 983 / 199
        // geometrically verified links across the corpus and refused nothing
        // that was actually wrong. Kept as a disagreement guard because it is
        // free and would catch a mislinked symbol in a file unlike these; it
        // fires on nothing in this corpus.
        if let (Some(instance_category), Some(symbol_category)) =
            (element.category, symbol_category)
        {
            if instance_category != symbol_category {
                continue;
            }
        }
        element.verified_symbol_bounds = Some(VerifiedSymbolBounds {
            symbol_element_id,
            bounds: symbol_bounds,
            instance_bounds,
        });
        // An instance whose own record did not yield a category takes its
        // symbol's. In Revit a family instance's category *is* its family's,
        // and the corpus confirms that reading rather than assuming it: among
        // the links this cross-check accepts, every one where both sides carry
        // a category carries the same one, with no exception on any of the
        // three files. Without this, 6 576 of SMALL's 7 014 placed family
        // instances have no category and the exporter drops them, bodies and
        // all, because an element with no category is not a candidate.
        //
        // Marked as inherited rather than silently merged: the JSON reports
        // `category_source`, so a consumer can tell a declared category from
        // one taken from the symbol.
        if element.category.is_none() {
            if let Some(symbol_category) = symbol_category {
                element.category = Some(symbol_category);
                element.category_source = Some("symbol");
            }
        }
    }
}

fn attach_fitting_axes(elements: &mut BTreeMap<u32, ExportedElement>, catalog: Option<Catalog>) {
    let mut by_owner: BTreeMap<u32, Vec<FittingCenterLineFields>> = BTreeMap::new();
    for element in elements.values() {
        let Some(line) = element.fitting_center_line_candidate else {
            continue;
        };
        if category_name(element, catalog) == Some("OST_PipeFittingCenterLine") {
            by_owner
                .entry(line.owner_element_id)
                .or_default()
                .push(line);
        }
    }
    for (owner_id, lines) in by_owner {
        if lines.len() != 1 {
            continue;
        }
        let Some(owner) = elements.get_mut(&owner_id) else {
            continue;
        };
        if category_name(owner, catalog) == Some("OST_PipeFitting")
            && owner
                .geometry_bounds
                .is_some_and(|bounds| bounds.contains_line_segment(lines[0].start, lines[0].end))
        {
            owner.fitting_axis_candidate = lines.first().copied();
        }
    }
}

#[must_use]
pub fn category_name(element: &ExportedElement, catalog: Option<Catalog>) -> Option<&'static str> {
    catalog?
        .built_in_category(element.category?)
        .map(|category| category.enum_name)
}

pub struct RecoveredElements {
    pub release: Option<u16>,
    pub catalog: Option<Catalog>,
    pub parameter_values_schema_bound: bool,
    pub schema: Option<Schema>,
    pub partition_paths: Vec<String>,
    pub parameter_names: BTreeMap<i32, String>,
    pub parameter_specs: BTreeMap<i32, String>,
    pub elements: BTreeMap<u32, ExportedElement>,
    /// Each element's own Revit `UniqueId`, where the file carries the episode
    /// it was created in. See [`authored_unique_ids`].
    pub authored_unique_ids: BTreeMap<u32, [u8; 16]>,
    /// Which file, and which save of it, this is. See [`document_identity`].
    pub document_identity: Option<BimDocumentIdentity>,
}

#[must_use]
pub fn mapped_family_instance_placement_counts(
    model: &BimModel,
    recovered: &RecoveredElements,
) -> (usize, usize) {
    let mapped = model
        .elements
        .iter()
        .filter(|element| carries_family_symbol_geometry(element.element_type));
    let mut total = 0;
    let mut placed = 0;
    for element in mapped {
        total += 1;
        placed += usize::from(
            element
                .id
                .0
                .parse::<u32>()
                .ok()
                .and_then(|id| recovered.elements.get(&id))
                .is_some_and(|element| element.ginstance_transform.is_some()),
        );
    }
    (total, placed)
}

/// Source classes whose records are building elements of the model rather than
/// annotation, a type definition or a view artefact. `FamilyInstance` is here
/// because it carries the doors, windows, columns and railings; the class does
/// not say *which*, so it is typed from its category and falls back to a proxy.
#[must_use]
fn is_building_element_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "SWall"
            | "Floor"
            | "StairsLanding"
            | "StairsRun"
            | "StairsElement"
            | "ProfileRoof"
            | "FamilyInstance"
    )
}

/// The base class of a run of a building system: a pipe, a duct, a conduit, a
/// cable tray, a flexible run of any of them, and the insulation and lining
/// that follow one.
///
/// The list above is what the AR reference join established, and it is a list
/// of the classes *that file* builds a building out of. It says nothing about
/// a plumbing model, and applied to one it excluded the runs themselves: of
/// SMALL's 8 882 records descending from this class, 6 955 carry a decoded
/// body and not one reached the export, so the ВК file exported 6 628
/// `IfcPipeFitting` connecting nothing. Every clause that separates a model
/// element from a definition holds on them exactly as it does on a wall - all
/// 8 882 carry a phase, none is owned by a view or by a group definition, none
/// is moribund, and the 1 220 that declare their own category are the
/// definitions the same rule already drops.
const SYSTEM_RUN_CLASS: &str = "RbsCurve";

/// What the project's `ProjectInfo` element declares about it, read off the
/// five built-in parameters Revit's own IFC export reaches for the project,
/// building and site metadata it writes. Measured against AR S1: id 1365 is
/// the model's one `ProjectInfo` record, and every value below reproduces
/// what `s1_revit.ifc` states word for word - including the mapping, which
/// is not the obvious one: `PROJECT_NUMBER` is `IfcProject.Name`, and
/// `PROJECT_NAME` - the long descriptive sentence - is `IfcProject.LongName`.
fn project_identity(recovered: &RecoveredElements) -> Option<BimProjectIdentity> {
    const PROJECT_NUMBER: i32 = -1_006_316;
    const PROJECT_NAME: i32 = -1_006_317;
    const PROJECT_ADDRESS: i32 = -1_006_318;
    const PROJECT_STATUS: i32 = -1_006_320;
    const PROJECT_BUILDING_NAME: i32 = -1_019_006;

    let project_info_class = schema_class_index(recovered.schema.as_ref(), "ProjectInfo")?;
    let element = recovered
        .elements
        .values()
        .find(|element| !element.moribund && element.class_index == Some(project_info_class))?;
    let text = |code: i32| {
        element.parameters.iter().find_map(|parameter| {
            let ParameterValue::Text(text) = &parameter.value else {
                return None;
            };
            (parameter.id == code && !text.is_empty()).then(|| text.clone())
        })
    };
    Some(BimProjectIdentity {
        number: text(PROJECT_NUMBER),
        name: text(PROJECT_NAME),
        building_name: text(PROJECT_BUILDING_NAME),
        address: text(PROJECT_ADDRESS),
        phase: text(PROJECT_STATUS),
    })
}

/// The frame the project is shared in, read off the location the file itself
/// declares active.
///
/// Revit's *Manage > Coordinates* keeps one `GeoLocation` per named location
/// and each carries the transform between that location's coordinates and the
/// project's own, so which one is in force decides the answer.
/// `ActiveGeoLocationTrackingElement` states it outright - no agreement rule
/// like [`site_location`]'s is needed here, because nothing is being guessed
/// between records.
///
/// `None` where the file declares no active location, the location carries no
/// readable transform, or the transform is the identity - a project sitting at
/// its own origin is not placed anywhere, and saying so with an identity
/// placement would only add an entity that states nothing.
///
/// Verified against Revit's own export of AR S1: the active location is
/// `Внутренний`, its frame is `(30.8708, 16.3119, -0.275)` metres turned
/// `-88.52°`, and inverting it gives `IfcSite`'s placement in `s1_revit.ifc`
/// to the last digit Revit wrote - `(-17102.79, 30439.81, 275)` millimetres
/// with `RefDirection (0.025794452427383818, -0.99966726775661263, 0)`.
// Exact comparison, on purpose: an identity frame here is not a frame that
// came out near identity, it is the one Revit writes for a project that was
// never placed - literal zeroes and ones, put there by the application rather
// than arrived at by arithmetic.
#[allow(clippy::float_cmp)]
fn site_placement(recovered: &RecoveredElements) -> Option<BimPlacement> {
    let active = recovered
        .elements
        .values()
        .filter(|element| !element.moribund)
        .find_map(|element| element.active_geo_location)?;
    let location = recovered.elements.get(&active.active_id)?;
    let placement = normalize_placement(location.geo_location_placement)?;
    let identity = placement.origin.coordinates == [0.0; 3]
        && placement.reference_direction == [1.0, 0.0, 0.0]
        && placement.axis == [0.0, 0.0, 1.0];
    (!identity).then_some(placement)
}

/// Where the project sits, read off every `GeoSite` record the file carries.
///
/// A project can keep more than one named location (`Manage > Location` does
/// not delete an alternate when another becomes active), and nothing this
/// reader has found yet says which `GeoSite` record is the active one - see
/// [`rvt_model::GeoSiteFields`]. Guessing which one to trust would be a wrong
/// site silently written into every export of a file that has more than one,
/// so this only ever states a location when every non-deleted `GeoSite`
/// record in the file agrees, bit for bit. Measured against AR S1: 18
/// records, one value.
fn site_location(recovered: &RecoveredElements) -> Option<BimSiteLocation> {
    const RADIANS_TO_DEGREES: f64 = 180.0 / std::f64::consts::PI;
    let mut distinct = BTreeSet::new();
    let mut agreed = None;
    for element in recovered.elements.values() {
        if element.moribund {
            continue;
        }
        let Some(fields) = element.geo_site else {
            continue;
        };
        distinct.insert((
            fields.latitude_radians.to_bits(),
            fields.longitude_radians.to_bits(),
            fields.elevation_feet.to_bits(),
        ));
        agreed = Some(fields);
    }
    if distinct.len() != 1 {
        return None;
    }
    let fields = agreed?;
    Some(BimSiteLocation {
        latitude_degrees: fields.latitude_radians * RADIANS_TO_DEGREES,
        longitude_degrees: fields.longitude_radians * RADIANS_TO_DEGREES,
        elevation: revit_catalog::internal_feet_to_metres(fields.elevation_feet).map(|value| {
            BimNumber {
                value,
                unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
            }
        }),
    })
}

/// The storeys of *this* model, and the map that folds every recovered `Level`
/// onto the one that represents it.
fn building_storeys(
    recovered: &RecoveredElements,
    is_model_element: &dyn Fn(&ExportedElement) -> bool,
) -> (Vec<BimLevel>, BTreeMap<BimElementId, BimElementId>) {
    let is_level = |element: &ExportedElement| {
        element.class_index.is_some_and(|index| {
            recovered
                .schema
                .as_ref()
                .and_then(|schema| schema.class_by_index(index))
                .is_some_and(|class| class.name == "Level")
        })
    };
    // The record walk recovers every `Level` the file mentions, including
    // those a linked model or another section contributes, and they are not
    // distinguishable by any field on the level itself: AR S1 yields 163 of
    // them for 15 real storeys, the same name repeated at up to six
    // elevations a file's history has drifted through. Asking which levels
    // the exported elements actually reference settles the *right* elevation
    // for a storey that split into architecturally distinct copies - AR S1's
    // "02 Этаж" keeps 3.3 m and 4.2 m among its records, and only the
    // occupied one, 3.3 m, is where Revit's own export puts it too.
    let occupied_levels = recovered
        .elements
        .values()
        .filter(|element| is_model_element(element))
        .filter_map(|element| element.level_id)
        .collect::<BTreeSet<_>>();
    // Family documents contribute their own reference levels to the project
    // database. In the corpus those carry a family reference; top-level
    // project storeys do not.
    let is_project_level = |element: &ExportedElement| {
        !element.moribund
            && is_level(element)
            && element.family_id.is_none()
            && element.header_family_id.is_none()
    };
    let level_elevation = |element: &ExportedElement| {
        element.elevation_feet.and_then(|value| {
            Some(BimNumber {
                value: revit_catalog::internal_feet_to_metres(value)?,
                unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
            })
        })
    };

    let occupied = recovered
        .elements
        .iter()
        .filter(|(id, element)| {
            is_project_level(element)
                && i32::try_from(**id).is_ok_and(|id| occupied_levels.contains(&id))
        })
        .map(|(id, element)| BimLevel {
            id: BimElementId(id.to_string()),
            name: element.name.as_ref().map(|(name, _)| name.clone()),
            elevation: level_elevation(element),
        })
        .collect::<Vec<_>>();

    // Two `Level` records with the same name at the same elevation are one
    // storey, however many times the file repeats them: AR S1 keeps 55 records
    // for 15 distinct (name, elevation) pairs, one of them eleven times over.
    // The first record of each pair is the storey and the rest are folded into
    // it, so an element standing on any of them still lands somewhere.
    let mut canonical_level = BTreeMap::new();
    let mut seen_storeys: BTreeMap<(Option<&str>, Option<u64>), BimElementId> = BTreeMap::new();
    for level in &occupied {
        let key = (
            level.name.as_deref(),
            level.elevation.as_ref().map(|value| value.value.to_bits()),
        );
        let canonical = seen_storeys.entry(key).or_insert_with(|| level.id.clone());
        canonical_level.insert(level.id.clone(), canonical.clone());
    }
    let mut levels = occupied
        .iter()
        .filter(|level| canonical_level.get(&level.id) == Some(&level.id))
        .cloned()
        .collect::<Vec<_>>();

    include_unoccupied_storeys(
        recovered,
        &is_project_level,
        &level_elevation,
        &mut levels,
        &mut canonical_level,
    );

    (levels, canonical_level)
}

/// A storey nothing exported stands on - a roof no AR element sits at, a
/// level kept for a discipline this file does not carry - is still a real
/// storey of the project, and Revit's own export writes it exactly as it
/// does an occupied one, empty of contents. Measured against it on AR S1:
/// "01 `ПромЭтаж`", "25 Кровля 5" and "25 Кровля 6" are exactly the three
/// storeys occupancy leaves out in [`building_storeys`], each a single
/// record with no elevation to disambiguate - unlike the architecturally
/// split names occupancy already resolved, nothing here is ambiguous. Where a
/// name really is split several ways with none of them occupied, the split
/// this file's own history agrees on most - simple frequency, the same
/// signal that would have picked the wrong elevation for "02 Этаж" had it run
/// first there - is the best of nothing better available.
fn include_unoccupied_storeys(
    recovered: &RecoveredElements,
    is_project_level: &dyn Fn(&ExportedElement) -> bool,
    level_elevation: &dyn Fn(&ExportedElement) -> Option<BimNumber>,
    levels: &mut Vec<BimLevel>,
    canonical_level: &mut BTreeMap<BimElementId, BimElementId>,
) {
    let occupied_names: BTreeSet<Option<&str>> =
        levels.iter().map(|level| level.name.as_deref()).collect();
    let mut unoccupied_elevations: BTreeMap<&str, BTreeMap<Option<u64>, (u32, BimElementId)>> =
        BTreeMap::new();
    for (id, element) in &recovered.elements {
        if !is_project_level(element) || canonical_level.contains_key(&BimElementId(id.to_string()))
        {
            continue;
        }
        let Some((name, _)) = &element.name else {
            continue;
        };
        if occupied_names.contains(&Some(name.as_str())) {
            continue;
        }
        let elevation_bits = level_elevation(element).map(|value| value.value.to_bits());
        let bucket = unoccupied_elevations
            .entry(name.as_str())
            .or_default()
            .entry(elevation_bits)
            .or_insert((0, BimElementId(id.to_string())));
        bucket.0 += 1;
    }
    for (name, candidates) in unoccupied_elevations {
        let Some((&elevation_bits, (_, canonical))) =
            candidates.iter().max_by_key(|(_, (count, _))| *count)
        else {
            continue;
        };
        levels.push(BimLevel {
            id: canonical.clone(),
            name: Some(name.to_owned()),
            elevation: elevation_bits.map(|bits| BimNumber {
                value: f64::from_bits(bits),
                unit: Some(BimUnit::new("autodesk.unit.unit:meters-1.0.0", "Meters")),
            }),
        });
        for (id, element) in &recovered.elements {
            if is_project_level(element) && element.name.as_ref().is_some_and(|(n, _)| n == name) {
                canonical_level.insert(BimElementId(id.to_string()), canonical.clone());
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // The selection's clauses, each with its measurement.
#[must_use]
pub fn metadata_model(
    recovered: &RecoveredElements,
    include_unplaced: bool,
    limit: Option<usize>,
) -> (BimModel, PropertyCounts) {
    let class_name = |element: &ExportedElement| {
        element.class_index.and_then(|index| {
            recovered
                .schema
                .as_ref()
                .and_then(|schema| schema.class_by_index(index))
                .map(|class| class.name.as_str())
        })
    };
    let secondary_options = secondary_design_options(&recovered.elements);
    // Whether the element stands in the model the file describes as built, as
    // against a copy of it kept somewhere else in the same database.
    //
    // Two places keep such copies, and an element in either declares which:
    //
    // - A group definition. `Element.m_unplacedOwnerId` names it, and it names
    //   nothing else: of the 36 391 elements of AR S1 and the 26 259 of AR S2
    //   that carry one, every single one names an `ElementGroupType`, 235 and
    //   212 distinct definitions. Revit places a group by copying its
    //   definition's members into the model; those copies are the elements it
    //   exports, and they carry no owner. Joined to Revit's own export the
    //   split is exact - of the 11 291 / 10 918 products we and Revit both
    //   emit not one names a group definition, while 4 215 / 3 925 of the
    //   products we emit and Revit does not do. It is the single largest thing
    //   we were over-exporting.
    // - A secondary design option. See [`secondary_design_options`]: 17
    //   products on each file, and again none on either join.
    let stands_in_the_model = |element: &ExportedElement| {
        element.unplaced_owner_id.is_none()
            && !element
                .design_option_id
                .is_some_and(|option| secondary_options.contains(&option))
    };
    // A model element of a building class, as against a type definition, an
    // annotation or a view artefact of the same class. Each clause is
    // independently meaningful and together they keep every product Revit
    // exports - 100% recall on `SWall`, `Floor` and `FamilyInstance` alike.
    // A class the model is built out of: one of the building classes the
    // reference join established, or a run of a building system - see
    // [`SYSTEM_RUN_CLASS`], which is read as a class chain rather than a name
    // so that a duct, a conduit and a cable tray are admitted by the same
    // reading that admits a pipe.
    let is_building_element = |element: &ExportedElement| {
        class_name(element).is_some_and(is_building_element_class)
            || element
                .class_index
                .zip(recovered.schema.as_ref())
                .is_some_and(|(class_index, schema)| {
                    rvt_model::descends_from(schema, class_index, SYSTEM_RUN_CLASS)
                })
    };
    let is_model_element = |element: &ExportedElement| {
        stands_in_the_model(element)
            && is_building_element(element)
            // Owned by a view, so annotation or a detail item, not the model.
            && element.owner_view_id.is_none()
            // A model element is placed in a phase.
            && element.created_phase_id.is_some()
            // A record that declares its own category is a type or definition,
            // not an instance; the instances declare none. A category the
            // element inherited from its family is not a declaration.
            && !element.declares_a_category()
    };
    // A room is a place, not a building element: it fails every clause of
    // `is_model_element` - no building class, no phase - and carries no
    // category, so nothing would ever admit it. It is admitted on the terms
    // its own class establishes, with the same two clauses that separate an
    // instance from a definition: no declared category and no owning view.
    let is_space = |element: &ExportedElement| {
        element_type_for_source(class_name(element), None).is_spatial()
            && !element.declares_a_category()
            && element.owner_view_id.is_none()
    };
    let is_candidate = |element: &ExportedElement| {
        // An element carrying verified geometry is a candidate whether or not
        // its level was recovered. The default export otherwise requires a
        // level so that everything lands in a storey, and on BIG that is what
        // was hiding the model: of its 1 031 placed instances 461 have a level
        // and 658 have a category but only 88 have both, so the 78 that got
        // through were an unrepresentative slice whose transforms happened to
        // be near the origin - the file looked like one pile at (0,0,0) while
        // the decoded transforms actually spread over 108 x 141 x 33 m.
        //
        // Such an element is contained in the building rather than a storey,
        // which is what `--include-unplaced` already does for everything; this
        // extends it only to elements whose body and placement are verified,
        // so the default export gains geometry without gaining 272 000 rows.
        //
        // A third way in: the element is a *model* element of a building class,
        // which is how the real products reach the export at all. Joining our
        // decode to the IFC Revit itself exported from AR S1 on the Revit
        // element id showed the category test above selects almost exactly
        // against the truth - of Revit's 11 518 products we decode 11 332
        // (98.4%) but exported only 382 (3.3%), because a real instance keeps
        // its category on its type and declares none of its own. Not one of
        // the 11 332 carries a declared category, while 31-44% of the records
        // of the same classes that Revit does *not* export do.
        //
        // Each clause of `is_model_element` is independently meaningful and the
        // three together keep every one of the products Revit emits - 100%
        // recall on `SWall`, `Floor` and `FamilyInstance` alike - while
        // dropping the records of those classes that are not model elements:
        // precision rises from 57.8% to 69.3% on `SWall`, 44.5% to 58.8% on
        // `Floor` and 28.2% to 56.9% on `FamilyInstance`. What remained
        // over-selected after that read as not separable by any field this
        // decode recovers; half of it turned out to be separable by one.
        //
        // The first way in - a declared category - was the leak. It admits the
        // very records the third way in is careful to exclude: a type or a
        // definition declares its category, an instance does not. Joining AR
        // S1 again bears that out without a single exception - of the 11 291
        // products we and Revit both export not one declares a category, while
        // 3 222 of the records we export and Revit does not do. So the reading
        // holds wherever it is applied, and it is applied here to every way in
        // rather than to one of them.
        let is_model_element = is_model_element(element);
        let verified_geometry = element.verified_symbol_bounds.is_some();
        let is_space = is_space(element);
        !element.moribund
            && stands_in_the_model(element)
            && class_name(element) != Some("Level")
            && !element.declares_a_category()
            // A record a view owns is drawn on a sheet, not built: a legend
            // component, a detail item, an annotation. `is_model_element`
            // has always said so, but the two ways in that bypass it - a
            // category, or verified geometry - did not, and a legend has
            // both a body and a symbol whose bounds verify. On AR S1 that
            // let four `LegendComponent` records through, and because a
            // legend is drawn beside the sheet rather than in the building
            // they sat 1.5 km out, stretching the model's extent from 27 m
            // to 1 525 m along y. Applied here it holds for every way in,
            // as the category clause below already does.
            && element.owner_view_id.is_none()
            && (element.category.is_some() || verified_geometry || is_model_element || is_space)
            && (include_unplaced
                || element.level_id.is_some()
                || verified_geometry
                || is_model_element
                || is_space)
    };

    let (levels, canonical_level) = building_storeys(recovered, &is_model_element);

    // Selected before any is normalized, because an element's geometry depends
    // on which others are products: a nested family leaves out the members
    // that are drawn by products of their own.
    let products = recovered
        .elements
        .iter()
        .filter(|(_, element)| is_candidate(element))
        .map(|(id, _)| *id)
        .take(limit.unwrap_or(usize::MAX))
        .collect::<BTreeSet<_>>();
    let is_product = |id: u32| products.contains(&id);

    let mut counts = PropertyCounts::default();
    let elements = products
        .iter()
        .filter_map(|id| Some((id, recovered.elements.get(id)?)))
        .map(|(id, element)| {
            let mut normalized = normalize_element(
                *id,
                element,
                &recovered.elements,
                &is_product,
                recovered.schema.as_ref(),
                &recovered.parameter_names,
                &recovered.parameter_specs,
                recovered.catalog,
            );
            normalized.level_id = normalized
                .level_id
                .as_ref()
                .and_then(|level_id| canonical_level.get(level_id).cloned());
            normalized.authored_uuid = recovered.authored_unique_ids.get(id).copied();
            let mut properties = trusted_source_properties(&normalized);
            // A parameter nothing names is not written into the model. See
            // `placeholder_parameter_name`: the reader recovered a value and
            // its code, and neither the file nor Autodesk's table says what
            // the code means, so a property sheet can only repeat the code
            // back at its reader. It stays in the JSON export, which writes
            // the intermediate.
            for set in [&mut normalized.properties, &mut normalized.type_properties] {
                let before = set.len();
                set.retain(|property| !names_nothing(property));
                counts.unnamed += before - set.len();
            }
            // The type's values come from the same reader as the element's own
            // and carry the same risk of an unverified parameter code, so they
            // are held to the same catalogue check rather than to none.
            if recovered.parameter_values_schema_bound {
                counts.included += normalized.properties.len();
                properties.append(&mut normalized.properties);
                counts.included_from_type += normalized.type_properties.len();
            } else {
                counts.unverified += normalized.properties.len();
                counts.unverified += normalized.type_properties.len();
                normalized.type_properties.clear();
            }
            normalized.properties = properties;
            normalized
        })
        .collect::<Vec<_>>();

    let space_boundaries = bim_core::compute_space_boundaries(&elements);
    (
        BimModel {
            source: Some(BimSource {
                application: "Autodesk Revit".to_owned(),
                release: recovered.release.map(|release| release.to_string()),
                // A file whose release `BasicFileInfo` does not state is no
                // better off than one from a release with no tables: either
                // way nothing named the built-in codes this model carries.
                release_catalogued: Some(recovered.catalog.is_some()),
            }),
            project: project_identity(recovered),
            document_identity: recovered.document_identity.clone(),
            site: site_location(recovered),
            site_placement: site_placement(recovered),
            documents: Vec::new(),
            elements,
            levels,
            relations: Vec::new(),
            space_boundaries,
        },
        counts,
    )
}

/// How many of the parameters the reader recovered stand in the model, and
/// why the rest do not.
///
/// Each number is a different reading: `included` and `included_from_type` are
/// what a reader of the model gets, `unverified` is what this Revit release's
/// catalogue could not speak for, and `unnamed` is what nothing names. They
/// are reported rather than summed, since a count of "left out" alone says
/// nothing about which of the two reasons applies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PropertyCounts {
    pub included: usize,
    pub included_from_type: usize,
    pub unverified: usize,
    pub unnamed: usize,
}

/// Class name used for a record whose class index the schema did not resolve.
pub const UNRESOLVED_CLASS: &str = "(class not resolved)";

/// How close a body's own extent must come to its record's bounds block to
/// count as the same box. A micro-foot is 0.3 micrometres: far below anything
/// a modelled dimension carries, and far above double-precision noise.
pub const BODY_BOUNDS_TOLERANCE_FEET: f64 = 1e-6;

/// How far a body's own extent sits from a bounds block: the largest of the
/// six coordinate differences, in Revit internal feet.
///
/// `None` when the body has no extent, or when the box holds no volume - a box
/// flat on an axis describes a region or a sketch rather than a solid, and AR
/// S1 carries 9 873 `FilledRegion` records that would otherwise agree with one
/// on a single face.
#[must_use]
pub fn body_bounds_residual_feet(
    brep: &rvt_model::SymbolBrep,
    bounds: &GElementBounds,
) -> Option<f64> {
    let (min, max) = body_extent_feet(brep)?;
    bounds.is_volumetric().then(|| {
        min.into_iter()
            .chain(max)
            .zip(bounds.min.into_iter().chain(bounds.max))
            .map(|(ours, theirs)| (ours - theirs).abs())
            .fold(0.0_f64, f64::max)
    })
}

/// Which of a record's bodies is the element's, and whether it is placed.
///
/// A `GElement` record is not one body. It declares them - each is a `GBRep`
/// node naming its own faces - and a wall's record typically declares the
/// solid plus a free surface for each plane its compound structure separates
/// on. Read as one body those union together, and the union reaches outside
/// the box the same record declares: on AR S1 that is 1 576 walls whose body
/// was refused for missing a box it does not describe.
///
/// The box says which body is the element's. Where exactly one declared body
/// reproduces it, that body is the record's geometry and the rest are the free
/// surfaces beside it. Where none does, the whole assembly stands as before -
/// a record whose solid is split across two nodes is not resolved by this and
/// is left exactly as it was.
#[must_use]
pub fn place_declared_body(
    brep: rvt_model::SymbolBrep,
    bounds: Option<&GElementBounds>,
) -> (rvt_model::SymbolBrep, bool) {
    let Some(bounds) = bounds else {
        return (brep, false);
    };
    let mut matching = (0..brep.bodies.len()).filter(|index| {
        brep.body(*index)
            .is_some_and(|body| body_is_placed_in(&body, bounds))
    });
    if let (Some(index), None) = (matching.next(), matching.next()) {
        if let Some(body) = brep.body(index) {
            return (body, true);
        }
    }
    let placed = body_is_placed_in(&brep, bounds);
    (brep, placed)
}

/// The bit of a face's own `GInfo.m_flags` that is set on the faces the
/// record's box bounds and clear on the rest.
///
/// What Revit calls it is not established - the flags word carries no
/// declaration beyond its name - so it is named here for what it separates,
/// and it was chosen by measurement rather than read off one record.
/// `openrvt face-mark-probe` scores every mark a `GFace` declares against the
/// one answer that is independent of them: whether the face reaches outside
/// the box its own record carries. On AR S1 this bit is clear on 9 828 faces
/// that do and set on all but 1 039 of the 355 361 that do not, and dropping
/// the faces it leaves clear lands 4 303 records' bodies exactly on their box,
/// 4 295 of them bounding a volume by their own loops.
///
/// The alternatives are refuted by the same probe, and that is where they are
/// recorded: `GFace.m_cutType`, which the notebook nominated, is zero on every
/// face of every record that would need it and recovers nothing;
/// `m_faceFlags_v9 & 0x2` recovers a similar count of records and **not one**
/// of them bounds a volume, so it cuts into the wall rather than around it;
/// and a null `m_renderStyleId` fires on 214 185 faces that are inside the box.
pub const FACE_INSIDE_THE_BOX_FLAG: i64 = 0x0008_0000;

/// What rebuilding a record without the faces its own mark sets apart found.
enum CutFaceTrim {
    /// The same placed body with some of its faces removed.
    Accepted(Box<rvt_model::SymbolBrep>),
    /// A closed body landed on the same box, but it is made from different
    /// faces. This is evidence against the first body's topological closure,
    /// not permission to replace it with the second body.
    DifferentBody,
    /// No single volume-bounding body landed on the record's box.
    Refused,
}

/// Place the body a record's box bounds, after dropping the faces that are not
/// in it.
///
/// A wall's record does not hold the wall alone, and the file joins what is
/// beside it into the same shell in two ways.
///
/// *Outside the box.* Where something is cut out of the wall, the cut geometry
/// is joined to it: element 4975869 of AR S1 declares one closed shell of 26
/// faces of which the wall is 22, the other four being the far caps of two
/// window voids, 0.12 m past one face of the wall and 0.25 m past the other.
/// `GBRep.m_pFaces` and `GEdge.m_pFace` cannot separate those - the file
/// declares them as one shell - so the wall had no body at all: nothing
/// reproduced the box, and the box alone is not offered as an extent.
///
/// *Inside it.* A face can also sit in the interior of the shell, where it
/// changes no extent and so costs the body nothing at placement: wall 5572231
/// is a 2 750 x 250 x 3 243 mm wall of 7 faces, and the seventh spans its whole
/// footprint 3 004 mm up, at the top of the layers its compound structure
/// separates on. The box is reproduced with it, and so it was exported - as a
/// closed solid whose volume is the wall's 2.206 m³ plus that face's own
/// contribution to the divergence integral, 8.441 m³ in total. Revit's own
/// number for it is 2.206 m³.
///
/// The faces themselves say which is which in both cases, in
/// [`FACE_INSIDE_THE_BOX_FLAG`]. Dropping the ones that lack it and assembling
/// the record again is what this does, and the result is accepted only when it
/// meets the test the first pass applies plus two more: exactly one body
/// reproduces the box, its faces bound a volume by their own loops
/// ([`rvt_model::SymbolBrep::bounds_a_volume`]) - so a trim that opens a shell
/// is refused rather than exported - and the body it places is `first_pass`
/// less some of its faces rather than a different body.
///
/// That last guard is what the interior case needs and the exterior one never
/// exercises. A record trimmed of a face beyond its box can place a *second*
/// body on that box: element 7281744 is a 10 mm panel whose shell reaches
/// y = 4.5276 ft, where its box ends, and whose two coincident layer sheets
/// reach it too - so dropping the face that carries the shell there leaves the
/// sheets to reproduce the box between them, and two coincident sheets pair
/// every edge, so they bound a volume as well. What they bound is not the
/// panel: 254 ft³, where the panel's whole box holds 0.25 ft³. Requiring a
/// subset refuses all 98 such records on AR S1 and
/// costs nothing anywhere: of the 4 407 trims the corpus accepts through the
/// unplaced door, every one is already a subset.
#[must_use]
pub fn place_body_less_its_cut_faces(
    objects: &[rvt_model::SerialObject],
    classes: &rvt_model::BrepClassIndexes,
    body_classes: &[u16],
    bounds: Option<&GElementBounds>,
    first_pass: &rvt_model::SymbolBrep,
) -> Option<rvt_model::SymbolBrep> {
    match cut_face_trim(objects, classes, body_classes, bounds, first_pass) {
        CutFaceTrim::Accepted(trimmed) => Some(*trimmed),
        CutFaceTrim::DifferentBody | CutFaceTrim::Refused => None,
    }
}

fn cut_face_trim(
    objects: &[rvt_model::SerialObject],
    classes: &rvt_model::BrepClassIndexes,
    body_classes: &[u16],
    bounds: Option<&GElementBounds>,
    first_pass: &rvt_model::SymbolBrep,
) -> CutFaceTrim {
    let Some(bounds) = bounds else {
        return CutFaceTrim::Refused;
    };
    let mut dropped = 0_usize;
    let kept = objects
        .iter()
        .filter(|object| {
            let cut = object.class_index == classes.face
                && GFaceMarks::read(object)
                    .is_some_and(|marks| marks.info_flags & FACE_INSIDE_THE_BOX_FLAG == 0);
            dropped += usize::from(cut);
            !cut
        })
        .cloned()
        .collect::<Vec<_>>();
    if dropped == 0 {
        return CutFaceTrim::Refused;
    }
    let trimmed = rvt_model::assemble_symbol_brep(&kept, classes, body_classes);
    let (trimmed, placed) = place_declared_body(trimmed, Some(bounds));
    if !(placed && trimmed.bounds_a_volume()) {
        return CutFaceTrim::Refused;
    }
    if is_trim_of(&trimmed, first_pass) {
        CutFaceTrim::Accepted(Box::new(trimmed))
    } else {
        CutFaceTrim::DifferentBody
    }
}

/// Whether one body is another read again with some of its faces dropped:
/// every face it carries is one the other carried, and it carries fewer.
fn is_trim_of(trimmed: &rvt_model::SymbolBrep, first_pass: &rvt_model::SymbolBrep) -> bool {
    !trimmed.face_ids.is_empty()
        && trimmed.face_ids.len() < first_pass.face_ids.len()
        && trimmed
            .face_ids
            .iter()
            .all(|face| first_pass.face_ids.contains(face))
}

/// Whether a body is already placed, by reproducing the bounds block carried
/// by the same `GElement` record.
#[must_use]
pub fn body_is_placed_in(brep: &rvt_model::SymbolBrep, bounds: &GElementBounds) -> bool {
    body_bounds_residual_feet(brep, bounds)
        .is_some_and(|residual| residual <= BODY_BOUNDS_TOLERANCE_FEET)
}

/// The box a body is judged against: the exact duplicated block its own record
/// carries, or - only where that record carries none - the box in the record's
/// `GElement` graph header.
///
/// The two are one box read two ways. Where a record carries both, they agree
/// on every record in the corpus that carries a body: 14 808 on AR S1, 2 678
/// on KJ S1, 9 840 on ВК and 3 720 on ЭОМ, with not one disagreement. So the
/// graph header's box is not a second opinion to fall back on - a record whose
/// exact block refuses a body is not re-asked, which is why this picks one box
/// rather than trying both - it is the same box on the records where the
/// whole-body scan cannot single one out. Reading it places 2 642 more bodies
/// on AR S1, 2 321 of them walls, and 806 / 160 / 26 on the other three.
///
/// The near duplicate that [`GElementBounds::parse_near_duplicate`] finds adds
/// nothing here: every body it would place, the graph header's box already
/// places, on all four files.
#[must_use]
fn body_placement_box(
    exact: Option<GElementBounds>,
    graph: Option<GElementBounds>,
) -> Option<GElementBounds> {
    graph.or(exact)
}

/// What every box on a body's own record says about where that body sits.
///
/// Measurement only: the export places a body on the exact duplicated block
/// alone, and this records what the record's other two boxes - the `GElement`
/// graph header's, and the numerically-equal near duplicate - would have said
/// about the same body. Each value is the residual from
/// [`body_bounds_residual_feet`], so `Some(0.0)` is exact agreement and `None`
/// means that box was absent or held no volume.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BodyBoxResiduals {
    pub exact: Option<f64>,
    pub graph: Option<f64>,
    /// Read only for a record carrying no exact block: it is a scan of the
    /// whole body, and that is the only population it could add anything to.
    pub near_duplicate: Option<f64>,
    /// How far the graph header's box sits from the exact block on records
    /// that carry both. If the two are the same box, the graph box inherits
    /// whatever standing the exact one has.
    pub graph_from_exact: Option<f64>,
}

impl BodyBoxResiduals {
    /// Whether a residual is close enough to call the two boxes the same.
    #[must_use]
    pub fn agrees(residual: Option<f64>) -> bool {
        residual.is_some_and(|residual| residual <= BODY_BOUNDS_TOLERANCE_FEET)
    }
}

/// Whether a newly decoded body should replace the one its id already holds.
///
/// A placed body wins over an unplaced one, and between two placed bodies the
/// later record wins - the same rule, for the same reason, as
/// [`fold_type_element_id`]: a duplicate record of one element in an earlier
/// partition is a state the model has moved on from, and everything else the
/// element carries is already read from the latest record. Keeping the larger
/// of the two instead, which this did before, mixed one record's body with
/// another's box: AR S1's wall 5 405 889 is 2.8 m tall in `Partitions/755`,
/// which is the height its own box and `s1_revit.ifc` both state, and 4.0 m
/// tall in `Partitions/541`, whose body was the one kept.
///
/// Among unplaced bodies the last still wins, which is what every id did
/// before the two could be told apart.
#[must_use]
fn keep_body(placed: bool, kept: &ExportedElement) -> bool {
    match (placed, kept.brep_is_placed) {
        (true | false, false) | (true, true) => true,
        (false, true) => false,
    }
}

/// What one class contributes to the decoded geometry, from both ends: the
/// bodies its records own, and the model elements of that class that reach a
/// body at all.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClassGeometry {
    /// Ids of this class whose own `GElement` record yielded a body.
    pub body_ids: usize,
    /// Body-bearing records on those ids. More than one per id means the
    /// recovery keeps the last and the rest are not reachable.
    pub body_records: usize,
    /// Bodies with no excluded face, which is what the exporter emits.
    pub complete_bodies: usize,
    pub faces: usize,
    /// Bodies whose owning record also carried an exact bounds block, and how
    /// many reproduce it. Agreement says the body is in the same frame as the
    /// bounds the placement chain already trusts.
    pub bodies_with_bounds: usize,
    pub bodies_matching_their_bounds: usize,
    /// The same question asked of the two other boxes the record carries, as
    /// the measurement behind a possible second placement tier. `only` counts
    /// bodies whose record carries no exact block at all: what that tier would
    /// actually add, rather than what it would re-confirm.
    pub bodies_with_graph_bounds: usize,
    pub bodies_matching_their_graph_bounds: usize,
    pub bodies_placed_only_by_graph_bounds: usize,
    pub bodies_placed_only_by_near_duplicate_bounds: usize,
    /// Placed by either of them: what a second tier reading both would add,
    /// with the overlap counted once.
    pub bodies_placed_only_by_another_box: usize,
    /// Bodies whose record carries both boxes and whose graph box is not the
    /// exact block. Where this is zero the graph box is the exact block seen
    /// from its declared offset.
    pub graph_bounds_differing_from_exact: usize,
    /// Bodies whose centre is more than a foot from the origin, i.e. already
    /// carrying a position rather than sitting in a symbol's local frame.
    pub bodies_away_from_the_origin: usize,
    /// Of `body_ids`, how many are named as a symbol by some instance's
    /// `GInstance` transform, and how many an instance's bounds check
    /// accepted. The gap between them is what the export's gates cost.
    pub named_by_an_instance: usize,
    pub verified_by_an_instance: usize,
    /// Model elements of this class, and how many reach a body - their own,
    /// or the one on the symbol their bounds verified.
    pub model_elements: usize,
    pub model_elements_with_their_own_body: usize,
    /// Of those, the ones whose body reproduces its record's bounds and is
    /// therefore emitted: the placed-body path's actual reach.
    pub model_elements_with_a_placed_body: usize,
    pub model_elements_with_a_verified_symbol_body: usize,
}

/// A body's axis-aligned extent, in Revit internal feet.
///
/// Exact for every surface this decoder produces. The endpoints of the edges
/// bound a planar face, and they bound a cylindrical one too: its generators
/// are straight lines between boundary points, so nothing on the patch lies
/// outside its boundary. What the endpoints alone miss is the bulge of an arc
/// between them, and that is added analytically rather than left as an
/// approximation - a curved body could otherwise never be told from one in
/// the wrong frame.
#[must_use]
pub fn body_extent_feet(brep: &rvt_model::SymbolBrep) -> Option<([f64; 3], [f64; 3])> {
    faces_extent_feet(&brep.faces)
}

/// The extent of a set of faces, in the frame they were read in. `None` when
/// any point is not finite, so a body carrying one unreadable coordinate never
/// reports a plausible box.
pub fn faces_extent_feet(faces: &[rvt_model::BrepFace]) -> Option<([f64; 3], [f64; 3])> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let include = |point: [f64; 3], min: &mut [f64; 3], max: &mut [f64; 3]| {
        for (axis, value) in point.into_iter().enumerate() {
            if !value.is_finite() {
                return false;
            }
            min[axis] = min[axis].min(value);
            max[axis] = max[axis].max(value);
        }
        true
    };
    for face in faces {
        for face_loop in &face.loops {
            for edge in face_loop {
                for point in [edge.start, edge.end] {
                    if !include(point, &mut min, &mut max) {
                        return None;
                    }
                }
                if let rvt_model::BrepCurve::Arc(arc) = &edge.curve {
                    for point in arc_extreme_points(arc) {
                        if !include(point, &mut min, &mut max) {
                            return None;
                        }
                    }
                }
            }
        }
    }
    min.into_iter().all(f64::is_finite).then_some((min, max))
}

/// The points where an arc reaches an axis extreme, for the axes whose extreme
/// its own angular range covers.
///
/// `point(a) = center + radius * (cos a * x_axis + sin a * y_axis)`, so along
/// one axis the arc traces `center + R cos(a - phase)` and reaches its extreme
/// at `phase` and `phase + pi`. Only an extreme the arc actually sweeps
/// through counts; elsewhere the endpoints already bound it.
#[must_use]
fn arc_extreme_points(arc: &rvt_model::BrepArc) -> Vec<[f64; 3]> {
    let y_axis = [
        arc.z_axis[1] * arc.x_axis[2] - arc.z_axis[2] * arc.x_axis[1],
        arc.z_axis[2] * arc.x_axis[0] - arc.z_axis[0] * arc.x_axis[2],
        arc.z_axis[0] * arc.x_axis[1] - arc.z_axis[1] * arc.x_axis[0],
    ];
    let point = |angle: f64| {
        let (sine, cosine) = angle.sin_cos();
        [0, 1, 2].map(|axis| {
            arc.center[axis] + arc.radius * (cosine * arc.x_axis[axis] + sine * y_axis[axis])
        })
    };
    // The file's own `u` values run in either direction; the arc covers what
    // lies between them either way.
    let (low, high) = (
        arc.start_angle.min(arc.end_angle),
        arc.start_angle.max(arc.end_angle),
    );
    let mut points = Vec::new();
    for (x, y) in arc.x_axis.into_iter().zip(y_axis) {
        let phase = y.atan2(x);
        for extreme in [phase, phase + std::f64::consts::PI] {
            // The first turn of this extreme at or after the arc's start.
            let swept =
                extreme + std::f64::consts::TAU * ((low - extreme) / std::f64::consts::TAU).ceil();
            if swept <= high {
                points.push(point(swept));
            }
        }
    }
    points
}

/// Tally the decoded bodies by the class of the element that owns them.
///
/// The question this answers is which half of the geometry gap a class is in:
/// a class with bodies that no instance names holds geometry the export never
/// asks for, while a class with no bodies at all holds none to ask for and
/// would have to have its shape constructed from its parameters.
pub fn tally_class_geometry<'a>(
    elements: &BTreeMap<u32, ExportedElement>,
    class_name: impl Fn(&ExportedElement) -> Option<&'a str>,
) -> BTreeMap<&'a str, ClassGeometry> {
    let mut named = BTreeSet::new();
    let mut verified = BTreeSet::new();
    for element in elements.values() {
        if let Some(symbol) = element
            .ginstance_transform
            .and_then(|transform| transform.symbol_element_id)
        {
            named.insert(symbol);
        }
        if let Some(symbol) = element.verified_symbol_bounds {
            verified.insert(symbol.symbol_element_id);
        }
    }

    let mut rows: BTreeMap<&str, ClassGeometry> = BTreeMap::new();
    for (id, element) in elements {
        let name = class_name(element).unwrap_or(UNRESOLVED_CLASS);
        let is_model_element = class_name(element).is_some_and(is_building_element_class)
            && element.owner_view_id.is_none()
            && element.created_phase_id.is_some()
            && !element.declares_a_category();
        let row = rows.entry(name).or_default();
        if let Some(brep) = &element.brep {
            row.body_ids += 1;
            row.body_records += element.brep_records;
            row.complete_bodies += usize::from(brep.excluded_faces.is_empty());
            row.faces += brep.faces.len();
            row.named_by_an_instance += usize::from(named.contains(id));
            row.verified_by_an_instance += usize::from(verified.contains(id));
            if let Some((min, max)) = body_extent_feet(brep) {
                let centre_is_placed = min
                    .into_iter()
                    .zip(max)
                    .any(|(low, high)| (low + high).abs() / 2.0 > 1.0);
                row.bodies_away_from_the_origin += usize::from(centre_is_placed);
                row.bodies_with_bounds += usize::from(element.geometry_bounds.is_some());
                // The recovery paired this body with the bounds of its own
                // record; re-deriving it here from the element would cross
                // one record's body with another's box.
                let residuals = element.brep_box_residuals;
                // The exact block alone, so this column keeps meaning what it
                // did before the graph box became a second tier;
                // `model_elements_with_a_placed_body` below is the one that
                // counts both.
                row.bodies_matching_their_bounds +=
                    usize::from(BodyBoxResiduals::agrees(residuals.exact));
                row.bodies_with_graph_bounds += usize::from(residuals.graph.is_some());
                row.bodies_matching_their_graph_bounds +=
                    usize::from(BodyBoxResiduals::agrees(residuals.graph));
                if residuals.exact.is_none() {
                    row.bodies_placed_only_by_graph_bounds +=
                        usize::from(BodyBoxResiduals::agrees(residuals.graph));
                    row.bodies_placed_only_by_near_duplicate_bounds +=
                        usize::from(BodyBoxResiduals::agrees(residuals.near_duplicate));
                    row.bodies_placed_only_by_another_box += usize::from(
                        BodyBoxResiduals::agrees(residuals.graph)
                            || BodyBoxResiduals::agrees(residuals.near_duplicate),
                    );
                }
                row.graph_bounds_differing_from_exact += usize::from(
                    residuals.graph_from_exact.is_some()
                        && !BodyBoxResiduals::agrees(residuals.graph_from_exact),
                );
            }
        }
        if is_model_element {
            row.model_elements += 1;
            row.model_elements_with_their_own_body += usize::from(element.brep.is_some());
            row.model_elements_with_a_placed_body += usize::from(element.brep_is_placed);
            row.model_elements_with_a_verified_symbol_body += usize::from(
                element
                    .verified_symbol_bounds
                    .and_then(|symbol| elements.get(&symbol.symbol_element_id))
                    .is_some_and(|symbol| symbol.brep.is_some()),
            );
        }
    }
    rows
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GeometryStatistics {
    pub pipe_candidates: usize,
    pub pipe_candidates_with_bounds: usize,
    pub verified_pipe_lines: usize,
    pub fitting_center_line_candidates: usize,
    pub verified_fitting_axes: usize,
    pub family_instances_with_placement_candidates: usize,
    pub verified_family_instance_placements: usize,
    pub verified_ginstance_transforms: usize,
    pub verified_symbol_bounds: usize,
    /// Instances whose link to a symbol is verified and whose symbol's body
    /// is flat: every face lies in one plane, so the family draws a plan
    /// symbol and states no solid. Counted because the alternative is a
    /// reader concluding the conversion lost the geometry. Revit's own export
    /// of AR S1 writes nothing for these either.
    pub instances_whose_symbol_body_is_flat: usize,
    /// Kept bodies whose topological closure was contradicted by a cut-face
    /// rebuild selecting a different set of faces on the same box.
    pub bodies_requiring_loop_closure: usize,
    /// Kept bodies containing exact, same-winding duplicate faces whose
    /// removal leaves an independently closed boundary.
    pub bodies_with_redundant_duplicate_faces: usize,
    /// What an element declaring several placements says about itself. A
    /// nested family writes one `InstInfoBase` per sub-instance, and no single
    /// transform describes it, so the export reads none of them. These count
    /// what a reading of all of them would have to stand on: the box on the
    /// element's own record against the hull of its sub-instances' symbol
    /// boxes, each carried through its own transform - the same cross-check
    /// that verifies the single-placement case, asked of the whole set.
    pub elements_with_several_placements_naming_symbols: usize,
    pub elements_with_several_placements_whose_symbols_are_known: usize,
    pub elements_with_several_placements_matching_their_box: usize,
    /// Of those, how many sub-instances they place between them, and how many
    /// of the symbols carry a decoded body: what emitting them would draw.
    pub placements_of_elements_with_several: usize,
    pub placements_whose_symbol_has_a_body: usize,
    /// Funnel from "an instance names a symbol" to "that body is placed in the
    /// world", so a shortfall can be attributed to the gate that caused it
    /// rather than guessed at. Each field counts the instances that passed
    /// every gate up to and including its own.
    pub instances_naming_a_symbol: usize,
    pub instances_whose_symbol_has_a_body: usize,
    pub instances_with_a_symbol_body_and_own_bounds: usize,
    pub instances_whose_category_matches_the_symbol: usize,
    pub instances_whose_bounds_match_the_symbol: usize,
    /// The bounds cross-check on its own, with the category-equality gate not
    /// applied, so the two gates can be told apart.
    pub instances_whose_bounds_match_ignoring_category: usize,
    /// The same check run on each pairing of the two boxes each record
    /// declares - `GRep.m_bBox` and `GRep.m_tightbBox` - so which box the
    /// chain is actually about is read rather than assumed.
    pub bounds_match_by_box_pair: [usize; 4],
    /// The instances the box cross-check refuses, and what refuses them. The
    /// check is the only verification the symbol link has, so a refusal is
    /// either a link that is not real or a box that is not the one to
    /// compare, and those are opposite conclusions. Each field counts the
    /// same refusals from a different side: whether the element declares more
    /// than one placement (so its box is the hull of several and no single
    /// symbol reproduces it), which box contains which, and how far apart the
    /// two are.
    pub instances_failing_the_box_cross_check: usize,
    pub failing_with_several_placements: usize,
    /// The instance's box holds the transformed symbol box: it draws that and
    /// something more.
    pub failing_whose_box_holds_the_symbols: usize,
    /// The other way round: the symbol's box carried over reaches outside the
    /// instance's, so what is placed is less than the symbol declares.
    pub failing_whose_box_is_inside_the_symbols: usize,
    /// Neither contains the other - the two boxes are somewhere else.
    pub failing_with_neither_box_inside: usize,
    /// Of the refusals, how many would agree against the id's last box - the
    /// pairing this gate used to use. It is kept the other way round now, so
    /// a file where the old crossing answered something this one does not is
    /// visible rather than silent.
    pub failing_that_agree_with_the_id_s_last_box: usize,
    /// The largest of the six coordinate differences, in feet, bucketed by
    /// [`BOX_GAP_BUCKETS`]. A gap of millimetres is a box read slightly wrong;
    /// a gap of feet is a different object.
    pub box_gap_buckets: [usize; BOX_GAP_BUCKETS.len()],
    /// Of the instances whose symbol has a body, how the category gate fails.
    pub instances_whose_symbol_has_no_category: usize,
    pub instances_whose_category_differs_from_the_symbol: usize,
    /// The declared `InstInfoBase` placement against the byte scan it
    /// replaced, so the two readings are compared rather than one being
    /// assumed to cover the other.
    pub elements_declaring_a_placement: usize,
    pub elements_declaring_several_placements: usize,
    pub placements_read_both_ways: usize,
    pub placements_the_two_readings_agree_on: usize,
    pub placements_only_the_scan_found: usize,
    /// Where the nested-family path stops, by the refusal it names. The hull
    /// check is the verification; everything after it is an element the file
    /// says is that set of sub-instances and whose bodies did not come out.
    pub nested_refusals: BTreeMap<&'static str, usize>,
    /// For an element held back by a member that does not close, what each of
    /// its members' bodies actually is. This is the backlog those elements are
    /// waiting on, stated per member rather than per element.
    pub nested_member_bodies: BTreeMap<&'static str, usize>,
    /// For an element held back by a member with no body at all, what that
    /// member is instead. A sub-instance whose own symbol carries no faces is
    /// not necessarily geometry this file lacks: it may be one more hop away,
    /// which is a reading to add rather than a backlog to wait on.
    pub nested_members_without_a_body: BTreeMap<&'static str, usize>,
    /// The node classes such a member's graph does carry. A graph with a box
    /// and no faces still declares something, and what it declares says
    /// whether a body is missing or was never written: on AR S1 it is `GLine`
    /// and `GArc`, so those members draw curves and no reading will give them
    /// a solid.
    pub nested_bodiless_member_nodes: BTreeMap<String, usize>,
    /// What the elements a missing body holds back are, by class. This is the
    /// row that says whether a file's shortfall is nested families at all: on
    /// AR S1 it is 3 555 `ElementGroup` against 183 `FamilyInstance`, and a
    /// group is not a product here in the first place.
    pub nested_refused_classes: BTreeMap<String, usize>,
}

/// How far apart the instance's box and the symbol's carried over may be, as
/// `(name, largest gap in it)` in Revit internal feet. The first bucket is the
/// tolerance the cross-check itself applies, so nothing a refusal produces can
/// land in it; the rest separate a box read slightly wrong from a box that
/// belongs to something else.
pub const BOX_GAP_BUCKETS: [(&str, f64); 7] = [
    ("<=1e-8", 1.0e-8),
    ("<=1e-6", 1.0e-6),
    ("<=1mm", 0.003_281),
    ("<=1cm", 0.032_81),
    ("<=10cm", 0.328_1),
    ("<=1m", 3.281),
    (">1m", f64::INFINITY),
];

/// Record one refusal of the box cross-check from every side that could
/// explain it. Nothing here changes a verdict: it is the reading that says
/// whether the 819 refusals on SMALL are links that are not real or boxes that
/// are not the ones to compare.
fn tally_box_cross_check_refusal(
    statistics: &mut GeometryStatistics,
    element: &ExportedElement,
    instance_bounds: &GElementBounds,
    symbol_bounds: &GElementBounds,
    transform: &GInstanceTransformFields,
) {
    // The tolerance `GElementBounds::matches_transformed` applies, repeated
    // here because containment has to be asked with the same slack the
    // equality was.
    const TOLERANCE_FEET: f64 = 1.0e-8;
    statistics.instances_failing_the_box_cross_check += 1;
    statistics.failing_with_several_placements +=
        usize::from(element.declared_instance_placements > 1);
    let (low, high) = GElementBounds::transformed(symbol_bounds, transform);
    let holds = (0..3).all(|axis| {
        instance_bounds.min[axis] <= low[axis] + TOLERANCE_FEET
            && instance_bounds.max[axis] >= high[axis] - TOLERANCE_FEET
    });
    let inside = (0..3).all(|axis| {
        instance_bounds.min[axis] >= low[axis] - TOLERANCE_FEET
            && instance_bounds.max[axis] <= high[axis] + TOLERANCE_FEET
    });
    match (holds, inside) {
        // Equal on every axis is what `matches_transformed` accepts, so it
        // cannot reach here; if it ever did it would be a tolerance mismatch
        // between the two and belongs with "holds".
        (true, _) => statistics.failing_whose_box_holds_the_symbols += 1,
        (false, true) => statistics.failing_whose_box_is_inside_the_symbols += 1,
        (false, false) => statistics.failing_with_neither_box_inside += 1,
    }
    let gap = low
        .into_iter()
        .chain(high)
        .zip(instance_bounds.min.into_iter().chain(instance_bounds.max))
        .map(|(expected, actual)| (expected - actual).abs())
        .fold(0.0_f64, f64::max);
    let bucket = BOX_GAP_BUCKETS
        .iter()
        .position(|(_, largest)| gap <= *largest)
        .unwrap_or(BOX_GAP_BUCKETS.len() - 1);
    statistics.box_gap_buckets[bucket] += 1;
    // The same question against the id's last box, which is what this gate
    // compared against before the pair was kept together.
    if let (Some(transform), Some(last_bounds)) =
        (element.ginstance_transform, element.placement_bounds)
    {
        if last_bounds.matches_transformed(symbol_bounds, &transform) {
            statistics.failing_that_agree_with_the_id_s_last_box += 1;
        }
    }
}

/// Whether every face of a body lies in one plane: a family that draws a plan
/// symbol rather than stating a solid. Read from the body's own extent, which
/// is the one measurement that does not depend on how the faces are wound.
#[must_use]
fn body_is_flat(brep: &rvt_model::SymbolBrep) -> bool {
    body_extent_feet(brep).is_some_and(|(min, max)| {
        min.into_iter()
            .zip(max)
            .any(|(low, high)| (high - low).abs() <= BODY_BOUNDS_TOLERANCE_FEET)
    })
}

#[allow(clippy::too_many_lines)] // One pass over the elements, tallying each funnel.
#[must_use]
pub fn geometry_statistics(
    elements: &BTreeMap<u32, ExportedElement>,
    schema: Option<&Schema>,
) -> GeometryStatistics {
    let mut statistics = GeometryStatistics::default();
    let class_name = |class_index: Option<u16>| {
        class_index
            .and_then(|index| schema.and_then(|schema| schema.class_by_index(index)))
            .map_or_else(
                || {
                    class_index
                        .map_or_else(|| "no class".to_owned(), |index| format!("class {index}"))
                },
                |class| class.name.clone(),
            )
    };
    for element in elements.values() {
        statistics.bodies_requiring_loop_closure += usize::from(element.brep_requires_loop_closure);
        statistics.bodies_with_redundant_duplicate_faces += usize::from(
            element
                .brep
                .as_ref()
                .is_some_and(|brep| !brep.redundant_duplicate_face_indexes().is_empty()),
        );
        if let Some(line) = element.pipe_line_candidate {
            statistics.pipe_candidates += 1;
            statistics.pipe_candidates_with_bounds +=
                usize::from(element.geometry_bounds.is_some());
            statistics.verified_pipe_lines += usize::from(
                element
                    .geometry_bounds
                    .is_some_and(|bounds| line.matches_bounds(&bounds)),
            );
        }
        statistics.fitting_center_line_candidates +=
            usize::from(element.fitting_center_line_candidate.is_some());
        statistics.verified_fitting_axes += usize::from(element.fitting_axis_candidate.is_some());
        statistics.family_instances_with_placement_candidates +=
            usize::from(!element.family_instance_placement_candidates.is_empty());
        if let Some(symbol_id) = element
            .ginstance_transform
            .and_then(|transform| transform.symbol_element_id)
        {
            statistics.instances_naming_a_symbol += 1;
            let symbol = elements.get(&symbol_id);
            if symbol.is_some_and(|symbol| symbol.brep.is_some()) {
                statistics.instances_whose_symbol_has_a_body += 1;
                if element
                    .instance_placement_bounds
                    .or(element.placement_bounds)
                    .is_some()
                {
                    statistics.instances_with_a_symbol_body_and_own_bounds += 1;
                }
                if element.category.is_some()
                    && element.category == symbol.and_then(|symbol| symbol.category)
                {
                    statistics.instances_whose_category_matches_the_symbol += 1;
                }
                if element.verified_symbol_bounds.is_some() {
                    statistics.instances_whose_bounds_match_the_symbol += 1;
                    statistics.instances_whose_symbol_body_is_flat += usize::from(
                        symbol.is_some_and(|symbol| symbol.brep.as_ref().is_some_and(body_is_flat)),
                    );
                }
                let symbol_category = symbol.and_then(|symbol| symbol.category);
                if symbol_category.is_none() {
                    statistics.instances_whose_symbol_has_no_category += 1;
                } else if element.category != symbol_category {
                    statistics.instances_whose_category_differs_from_the_symbol += 1;
                }
                if let (Some(transform), Some(instance_bounds), Some(symbol_bounds)) = (
                    element.ginstance_transform,
                    element
                        .instance_placement_bounds
                        .or(element.placement_bounds),
                    symbol
                        .and_then(|symbol| symbol.geometry_graph.as_ref())
                        .map(|graph| graph.bounds),
                ) {
                    if instance_bounds.matches_transformed(&symbol_bounds, &transform) {
                        statistics.instances_whose_bounds_match_ignoring_category += 1;
                    } else {
                        tally_box_cross_check_refusal(
                            &mut statistics,
                            element,
                            &instance_bounds,
                            &symbol_bounds,
                            &transform,
                        );
                    }
                }
                if let (Some(transform), Some(instance), Some(symbol)) = (
                    element.ginstance_transform,
                    element.geometry_graph.as_ref(),
                    symbol.and_then(|symbol| symbol.geometry_graph.as_ref()),
                ) {
                    for (index, (instance_box, symbol_box)) in [
                        (&instance.bounds, &symbol.bounds),
                        (&instance.bounds, &symbol.tight_bounds),
                        (&instance.tight_bounds, &symbol.bounds),
                        (&instance.tight_bounds, &symbol.tight_bounds),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        statistics.bounds_match_by_box_pair[index] +=
                            usize::from(instance_box.matches_transformed(symbol_box, &transform));
                    }
                }
            }
        }
        statistics.elements_declaring_a_placement +=
            usize::from(element.declared_instance_placements > 0);
        statistics.elements_declaring_several_placements +=
            usize::from(element.declared_instance_placements > 1);
        if element.declared_placements.len() > 1 {
            let symbols = element
                .declared_placements
                .iter()
                .map(|placement| {
                    let symbol = elements.get(&placement.symbol_element_id?)?;
                    Some((*placement, symbol.geometry_graph.as_ref()?.bounds, symbol))
                })
                .collect::<Option<Vec<_>>>();
            if element
                .declared_placements
                .iter()
                .all(|placement| placement.symbol_element_id.is_some())
            {
                statistics.elements_with_several_placements_naming_symbols += 1;
                statistics.placements_of_elements_with_several += element.declared_placements.len();
            }
            if let (Some(symbols), Some(bounds)) = (symbols, element.placement_bounds) {
                statistics.elements_with_several_placements_whose_symbols_are_known += 1;
                statistics.placements_whose_symbol_has_a_body += symbols
                    .iter()
                    .filter(|(_, _, symbol)| symbol.brep.is_some())
                    .count();
                let mut min = [f64::INFINITY; 3];
                let mut max = [f64::NEG_INFINITY; 3];
                for (placement, symbol_bounds, _) in &symbols {
                    let (low, high) = GElementBounds::transformed(symbol_bounds, placement);
                    for axis in 0..3 {
                        min[axis] = min[axis].min(low[axis]);
                        max[axis] = max[axis].max(high[axis]);
                    }
                }
                statistics.elements_with_several_placements_matching_their_box += usize::from(
                    min.into_iter()
                        .chain(max)
                        .zip(bounds.min.into_iter().chain(bounds.max))
                        .all(|(ours, theirs)| {
                            ours.is_finite() && (ours - theirs).abs() <= BODY_BOUNDS_TOLERANCE_FEET
                        }),
                );
            }
        }
        match (
            element.ginstance_transform,
            element.scanned_ginstance_transform,
        ) {
            (Some(declared), Some(scanned)) => {
                statistics.placements_read_both_ways += 1;
                statistics.placements_the_two_readings_agree_on += usize::from(
                    declared.basis == scanned.basis
                        && declared.origin == scanned.origin
                        && declared.symbol_element_id == scanned.symbol_element_id,
                );
            }
            (None, Some(_)) => statistics.placements_only_the_scan_found += 1,
            (Some(_) | None, None) => {}
        }
        statistics.verified_family_instance_placements +=
            usize::from(element.family_instance_placement.is_some());
        statistics.verified_ginstance_transforms +=
            usize::from(element.ginstance_transform.is_some());
        statistics.verified_symbol_bounds += usize::from(element.verified_symbol_bounds.is_some());
        let refusal = match nested_assembly(element, elements, &|_| false) {
            Ok(_) => "the assembly is written",
            Err(NestedRefusal::NotSeveral) => continue,
            Err(NestedRefusal::NoElementBounds) => "the element declares no box",
            Err(NestedRefusal::SymbolMissing) => "a placement names no recovered symbol",
            Err(NestedRefusal::SymbolBoundsMissing) => "a named symbol carries no box",
            Err(NestedRefusal::HullDisagreed) => {
                "the hull of the symbol boxes is not the element's"
            }
            Err(NestedRefusal::MemberHasNoBody) => "a member's symbol carries no decoded body",
            Err(NestedRefusal::MemberNotConverted) => "a member's body would not convert",
            Err(NestedRefusal::MemberIncomplete) => "a member's body does not close",
            Err(NestedRefusal::EveryMemberDrawnAlone) => "every member is a product of its own",
            Err(NestedRefusal::OwnBodyUnusable) => {
                "the hull needs the record's own body and it does not close"
            }
        };
        *statistics.nested_refusals.entry(refusal).or_default() += 1;
        if refusal == "a member's symbol carries no decoded body" {
            *statistics
                .nested_refused_classes
                .entry(class_name(element.class_index))
                .or_default() += 1;
        }
        if refusal == "a member's symbol carries no decoded body" {
            for placement in &element.declared_placements {
                let member = placement
                    .symbol_element_id
                    .and_then(|symbol| elements.get(&symbol));
                let verdict = match member {
                    None => "the placement names no recovered element",
                    Some(member) if member.brep.is_some() => "carries a body",
                    Some(member) if member.brep_records > 0 => "decoded a body that was not kept",
                    Some(member) if member.declared_placements.len() > 1 => {
                        "declares several placements of its own"
                    }
                    Some(member) if member.ginstance_transform.is_some() => {
                        "names one symbol of its own"
                    }
                    Some(member) if member.geometry_graph.is_some() => {
                        for node in member
                            .geometry_graph
                            .iter()
                            .flat_map(|graph| graph.top_level_nodes.iter())
                        {
                            *statistics
                                .nested_bodiless_member_nodes
                                .entry(class_name(Some(node.class_index)))
                                .or_default() += 1;
                        }
                        "carries a graph with no face-bearing record"
                    }
                    Some(_) => "carries no geometry record at all",
                };
                *statistics
                    .nested_members_without_a_body
                    .entry(verdict)
                    .or_default() += 1;
            }
        }
        if refusal != "a member's body does not close" {
            continue;
        }
        for placement in &element.declared_placements {
            let member = placement
                .symbol_element_id
                .and_then(|symbol| elements.get(&symbol));
            let verdict = match member.and_then(|member| member.brep.as_ref()) {
                None => "no decoded body",
                Some(brep) if brep.is_closed() => "closes: every face read and every shell closed",
                Some(brep) if brep.bounds_a_volume() => "bounds a volume by its own loops",
                Some(brep) if brep.excluded_faces.is_empty() => "every face read, no shell closed",
                Some(brep)
                    if brep
                        .excluded_faces
                        .iter()
                        .all(|exclusion| exclusion.reason == "face has no first loop") =>
                {
                    "excludes only faces no edge names"
                }
                Some(_) => "excludes a face of the shell",
            };
            *statistics.nested_member_bodies.entry(verdict).or_default() += 1;
        }
    }
    statistics
}

#[must_use]
fn trusted_source_properties(element: &BimElement) -> Vec<BimProperty> {
    let mut properties = vec![BimProperty {
        id: None,
        name: "Revit Element Id".to_owned(),
        specification: None,
        value: BimPropertyValue::Text(element.id.0.clone()),
    }];
    for (name, value) in [
        ("Revit Class", element.class_name.as_deref()),
        (
            "Revit Category",
            element
                .category
                .as_ref()
                .map(|category| category.name.as_str()),
        ),
    ] {
        if let Some(value) = value {
            properties.push(BimProperty {
                id: None,
                name: name.to_owned(),
                specification: None,
                value: BimPropertyValue::Text(value.to_owned()),
            });
        }
    }
    properties
}

#[must_use]
pub fn schema_class_index(schema: Option<&Schema>, name: &str) -> Option<u16> {
    schema
        .and_then(|schema| schema.class_by_name(name))
        .map(|class| class.index)
}

/// The boundary-representation class indices, resolved by name once. The
/// module that uses them never looks a class up itself.
#[must_use]
pub fn brep_class_indexes(schema: Option<&Schema>) -> Option<rvt_model::BrepClassIndexes> {
    Some(rvt_model::BrepClassIndexes {
        face: schema_class_index(schema, "Face")?,
        edge_loop: schema_class_index(schema, "EdgeLoop")?,
        // The one class derived from `EdgeLoop`, and optional because a file
        // whose schema does not declare it must still read every other loop.
        edge_loop_with_chain_envelopes: schema_class_index(schema, "EdgeLoopWithChainEnvelopes"),
        edge: schema_class_index(schema, "Edge")?,
        plane: schema_class_index(schema, "Plane")?,
        cyl_surf: schema_class_index(schema, "CylSurf")?,
        cone_surf: schema_class_index(schema, "ConeSurf")?,
        surf_rev: schema_class_index(schema, "SurfRev")?,
        ruled_surf: schema_class_index(schema, "RuledSurf")?,
        g_line: schema_class_index(schema, "GLine")?,
        g_arc: schema_class_index(schema, "GArc")?,
    })
}

/// Every class index that is a `GBRep`, which is the node that owns faces.
/// The records write its subclass `Geometry`, so the answer is a set rather
/// than one index, and [`rvt_model::assemble_symbol_brep`] takes it as one.
#[must_use]
pub fn brep_body_classes(schema: Option<&Schema>) -> Vec<u16> {
    let Some(schema) = schema else {
        return Vec::new();
    };
    let Some(g_brep) = schema_class_index(Some(schema), "GBRep") else {
        return Vec::new();
    };
    schema
        .classes
        .iter()
        .filter(|class| schema_class_is_a(schema, class.index, g_brep))
        .map(|class| class.index)
        .collect()
}

#[must_use]
pub fn schema_class_is_a(schema: &Schema, class_index: u16, ancestor_index: u16) -> bool {
    let mut current = Some(class_index);
    let mut remaining = schema.classes.len();
    while let Some(index) = current {
        if index == ancestor_index {
            return true;
        }
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        current = schema
            .class_by_index(index)
            .and_then(|class| class.parent.index());
    }
    false
}

#[must_use]
pub fn parameter_set_class_indexes(schema: Option<&Schema>) -> Option<ParameterSetClassIndexes> {
    Some(ParameterSetClassIndexes {
        double: schema_class_index(schema, "ParamValueSetDouble")?,
        integer: schema_class_index(schema, "ParamValueSetInt")?,
        text: schema_class_index(schema, "ParamValueSetAString")?,
        reference: schema_class_index(schema, "ParamValueSetElementId")?,
    })
}

/// Parameter identifiers point at definition elements in this same export.
#[must_use]
fn parameter_metadata(
    elements: &BTreeMap<u32, ExportedElement>,
) -> (BTreeMap<i32, String>, BTreeMap<i32, String>) {
    let names = elements
        .iter()
        .filter_map(|(id, element)| {
            let name = element.name.as_ref()?;
            Some((i32::try_from(*id).ok()?, name.0.clone()))
        })
        .collect();
    let specs = elements
        .iter()
        .filter_map(|(id, element)| {
            Some((
                i32::try_from(*id).ok()?,
                element.parameter_spec.as_ref()?.clone(),
            ))
        })
        .collect();
    (names, specs)
}

/// Cross the format boundary once: raw Revit identifiers stay available as
/// external IDs, while numbers with a known spec become unit-bearing values.
///
/// `is_product` says which other ids the caller writes as products of their
/// own; see [`drawn_as_its_own_product`].
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn normalize_element(
    id: u32,
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
    is_product: &dyn Fn(u32) -> bool,
    schema: Option<&Schema>,
    parameter_names: &BTreeMap<i32, String>,
    parameter_specs: &BTreeMap<i32, String>,
    catalog: Option<Catalog>,
) -> BimElement {
    let class_name = element.class_index.and_then(|class_index| {
        schema
            .and_then(|schema| schema.class_by_index(class_index))
            .map(|class| class.name.clone())
    });
    let category = element.category.map(|code| BimCategory {
        id: Some(BimExternalId {
            system: "autodesk.revit.builtInCategory".to_owned(),
            value: code.to_string(),
        }),
        name: catalog
            .and_then(|catalog| catalog.built_in_category(code))
            .map_or_else(
                || code.to_string(),
                |category| category.enum_name.to_owned(),
            ),
    });
    let properties = element
        .parameters
        .iter()
        .map(|parameter| normalize_property(parameter, parameter_names, parameter_specs, catalog))
        .collect();
    let type_properties = element
        .type_parameters
        .iter()
        .map(|parameter| normalize_property(parameter, parameter_names, parameter_specs, catalog))
        .collect();
    let element_type = curtain_wall_type(element, elements, schema).unwrap_or_else(|| {
        element_type_for_source(
            class_name.as_deref(),
            category.as_ref().map(|category| category.name.as_str()),
        )
    });
    // A room is named by its number and called something else, and Revit's own
    // export writes the split that way round. Both are on the record, by their
    // built-in parameters rather than by a display name.
    let (name, long_name) = if element_type.is_spatial() {
        let (number, room_name) = room_identity(element, catalog);
        (
            number.or_else(|| element.name.as_ref().map(|(name, _)| name.clone())),
            room_name,
        )
    } else {
        (element.name.as_ref().map(|(name, _)| name.clone()), None)
    };

    let geometry = normalize_geometry(element, element_type, elements, is_product)
        .map(|geometry| resolve_face_materials(geometry, elements));
    if let Some(BimGeometry::Brep(brep)) = &geometry {
        if let Some(extent) = brep_extent_metres(brep) {
            if extent < MIN_PLAUSIBLE_BREP_EXTENT_METRES {
                let name = element.name.as_ref().map_or("<unnamed>", |(name, _)| name);
                eprintln!(
                    "warning: element {id} ({name}) has a boundary representation only {:.3} mm across - likely degenerate source geometry, not a decode error (both the Face/Edge reconstruction and the record's own declared bounding box agree on this size)",
                    extent * 1000.0
                );
            }
        }
    }

    BimElement {
        id: BimElementId(id.to_string()),
        // A reader states one file; `bim_core::federate` names the document
        // when several are assembled.
        document: None,
        // Read from two `Global/*` streams rather than from the element's own
        // record, so it is attached where those are in scope - see
        // `authored_unique_ids`.
        authored_uuid: None,
        element_type,
        class_name,
        name,
        long_name,
        category,
        level_id: element.level_id.map(|id| BimElementId(id.to_string())),
        // The declared type reference, or the symbol verified by bounds; the
        // family reference the header carries is neither.
        type_id: element
            .type_element_reference()
            .map(|id| BimElementId(id.to_string())),
        // The type's own recovered name, read off the type record the same
        // reference reaches; a number is what identifies it, not what it is
        // called.
        type_name: element
            .type_element_reference()
            .and_then(|id| elements.get(&id))
            .and_then(|type_element| type_element.name.as_ref())
            .map(|(name, _)| name.clone()),
        // The wall, floor or roof this element is cut into - a door or a
        // window declares one, almost nothing else does.
        host_id: element
            .host_id
            .and_then(|id| u32::try_from(id).ok())
            .map(|id| BimElementId(id.to_string())),
        placement: normalize_placement(element.ginstance_transform),
        geometry,
        properties,
        type_properties,
        material_layers: normalize_material_layers(element, elements),
    }
}

/// The three wall-type classes that make a wall a curtain wall. All three are
/// siblings under `WallType`, so nothing in the class chain separates them
/// from an ordinary wall type and they are named outright.
const CURTAIN_WALL_TYPE_CLASSES: &[&str] =
    &["CurtainWallType", "NewCurtainWallType", "NRCurtainWallType"];

/// `CurtainWall` for a wall whose type is one of [`CURTAIN_WALL_TYPE_CLASSES`].
///
/// The wall's own class does not say so - a curtain wall is an `SWall` like any
/// other - but its type does, and the type is reached by the same declared
/// reference everything else uses. Measured against Revit's own export of AR
/// S1: 15 walls name a `NewCurtainWallType` and Revit writes an
/// `IfcCurtainWall` for exactly those 15, which were the only `IfcWall` we
/// emitted where it did not.
#[must_use]
fn curtain_wall_type(
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
    schema: Option<&Schema>,
) -> Option<BimElementType> {
    let type_id = element.type_element_reference()?;
    let class = elements
        .get(&type_id)?
        .class_index
        .and_then(|index| schema?.class_by_index(index))?;
    CURTAIN_WALL_TYPE_CLASSES
        .contains(&class.name.as_str())
        .then_some(BimElementType::CurtainWall)
}

/// What the element is made of, from the layer table its type carries.
///
/// The table lives on the type, so an element reaches it through the same
/// declared type reference the name and the parameters come through - and a
/// type carrying its own table keeps it. `source_type_id` records which record
/// it was read from, so an element wearing its type's build-up is never
/// mistaken for one that declared it.
///
/// Widths cross into metres here, at the format boundary, and a width the
/// conversion rejects drops its layer rather than being written unitless.
#[must_use]
fn normalize_material_layers(
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
) -> Option<BimMaterialLayerSet> {
    let (source_type_id, source, structure) = element.compound_structures.first().map_or_else(
        || {
            let id = element.type_element_reference()?;
            let source = elements.get(&id)?;
            let structure = source.compound_structures.first()?;
            Some((Some(BimElementId(id.to_string())), source, structure))
        },
        |structure| Some((None, element, structure)),
    )?;
    let count = structure.layers.len();
    let exterior = usize::try_from(structure.shell_layers_exterior).unwrap_or(count);
    let interior = usize::try_from(structure.shell_layers_interior).unwrap_or(count);
    let layers = structure
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| BimMaterialLayer {
            material: layer.material_id.map(|id| {
                let source = u32::try_from(id).ok().and_then(|id| elements.get(&id));
                BimMaterial {
                    id: Some(BimExternalId {
                        system: "autodesk.revit.elementId".to_owned(),
                        value: id.to_string(),
                    }),
                    name: source
                        .and_then(|material| material.name.as_ref())
                        .map(|(name, _)| name.clone()),
                    color: source
                        .and_then(|material| material.material_color)
                        .map(|fields| BimColor {
                            red: fields.red,
                            green: fields.green,
                            blue: fields.blue,
                        }),
                }
            }),
            thickness: BimNumber {
                value: revit_catalog::internal_feet_to_metres(layer.width_feet).unwrap_or(f64::NAN),
                unit: Some(metres_unit()),
            },
            is_core: index >= exterior && index < count.saturating_sub(interior),
            is_structural: structure.structural_layer_index == Some(index),
            source_function: Some(i64::from(layer.function)),
        })
        .filter(|layer| layer.thickness.value.is_finite())
        .collect();
    Some(BimMaterialLayerSet {
        source_type_id,
        // The build-up's name is the type's own - Revit does not name the
        // structure separately - so it is taken from the record the layers
        // were read from rather than from the element wearing them.
        name: source.name.as_ref().map(|(name, _)| name.clone()),
        layers,
    })
}

/// A room's number and its name, from the built-in parameters that carry
/// them. `ROOM_NUMBER` is on 553 of AR S1's 554 rooms and `ROOM_NAME` on all
/// 554, so the number can be missing where the name is not.
#[must_use]
fn room_identity(
    element: &ExportedElement,
    catalog: Option<Catalog>,
) -> (Option<String>, Option<String>) {
    let mut number = None;
    let mut name = None;
    for parameter in &element.parameters {
        let Some(built_in) = catalog.and_then(|catalog| catalog.built_in_parameter(parameter.id))
        else {
            continue;
        };
        let ParameterValue::Text(text) = &parameter.value else {
            continue;
        };
        match built_in.enum_name {
            "ROOM_NUMBER" => number = Some(text.clone()),
            "ROOM_NAME" => name = Some(text.clone()),
            _ => {}
        }
    }
    (number, name)
}

/// Below this, a `Brep` body is flagged as likely-degenerate source geometry
/// rather than a real physical part (see `normalize_element`'s warning). Not
/// a hard rule - chosen to be well under any real MEP fitting while still
/// catching sub-millimetre slivers like a family authored with a units bug.
const MIN_PLAUSIBLE_BREP_EXTENT_METRES: f64 = 0.005;

/// The largest axis-aligned extent across every vertex the body's edges
/// name, in metres. `None` for a body with no edges.
#[must_use]
fn brep_extent_metres(brep: &BimBrep) -> Option<f64> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut seen = false;
    for face in &brep.faces {
        for loop_edges in &face.loops {
            for edge in loop_edges {
                for point in [&edge.start, &edge.end] {
                    seen = true;
                    for axis in 0..3 {
                        min[axis] = min[axis].min(point.coordinates[axis]);
                        max[axis] = max[axis].max(point.coordinates[axis]);
                    }
                }
            }
        }
    }
    seen.then(|| (0..3).map(|axis| max[axis] - min[axis]).fold(0.0, f64::max))
}

fn normalize_placement(transform: Option<GInstanceTransformFields>) -> Option<BimPlacement> {
    let transform = transform?;
    let origin = transform
        .origin
        .coordinates_feet
        .map(revit_catalog::internal_feet_to_metres);
    let [Some(origin_x), Some(origin_y), Some(origin_z)] = origin else {
        return None;
    };
    Some(BimPlacement {
        origin: BimPoint3 {
            coordinates: [origin_x, origin_y, origin_z],
            unit: metres_unit(),
        },
        reference_direction: transform.basis[0],
        axis: transform.basis[2],
    })
}

/// Whether a mapped type is placed from a family symbol, and can therefore
/// carry a verified symbol extent. A pipe segment is a swept curve rather than
/// a placed symbol, and is handled before this.
///
/// An unclassified source is *not* excluded. Whether the exporter can name the
/// kind of building element something is, and whether its geometry was
/// verified, are independent questions: the symbol link is accepted only when
/// the instance's own independently decoded box agrees with the symbol's box
/// carried through its transform to within 1e-8 feet, which says nothing about
/// the category and does not need to. Excluding `Unknown` discarded the body of
/// every element whose category the mapping does not cover, which on this
/// corpus is most of them, and `IfcBuildingElementProxy` - what an unclassified
/// element is exported as - carries a shape representation perfectly well.
#[must_use]
fn carries_family_symbol_geometry(element_type: BimElementType) -> bool {
    !matches!(element_type, BimElementType::PipeSegment)
}

/// The bodies of an element that the file places as several instances.
///
/// A nested family writes one `InstInfoBase` per sub-instance, so no single
/// transform describes the element and the single-placement path above cannot
/// answer for it. What answers is the same cross-check one hop wider: the hull
/// of every sub-instance's symbol box, each carried through its own declared
/// transform, against the box on the element's own record. On AR S1 that hull
/// reproduces the box on 4 118 of the 4 280 elements whose symbols all decode,
/// and on AR S2 on 2 906 of 3 069, to 1e-6 ft on all six coordinates - the
/// standard the single instance is accepted on, asked of the whole set.
///
/// Every member has to be a closed body, for the reason the single path gives:
/// an incomplete shell is not something to ship. An element one of whose
/// sub-instances did not decode is therefore left to the paths below rather
/// than drawn in part, since a fifth of a nested family is not the element.
/// Why a nested family's set of placements did not become an assembly.
///
/// The path refuses in several places and they are not the same failure: a set
/// the hull check rejects is not the element, while one it accepts whose
/// member bodies did not decode is the element with the geometry missing. The
/// export ignores this, but nothing can be improved that is not first counted,
/// so the refusal is named rather than folded into a `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedRefusal {
    /// Fewer than two declared placements: not a nested family by this route.
    NotSeveral,
    /// The element declares no box to check a hull against.
    NoElementBounds,
    /// A placement names no symbol, or one this file did not recover.
    SymbolMissing,
    /// A named symbol carries no graph bounds, so its box cannot be placed.
    SymbolBoundsMissing,
    /// The hull of the placed symbol boxes is not the element's box, and the
    /// record carries no body of its own to account for the difference.
    HullDisagreed,
    /// The hull needs the record's own body and that body did not decode, or
    /// does not close, or is already the whole element on its own.
    OwnBodyUnusable,
    /// The hull agrees and a member's symbol carries no decoded body.
    MemberHasNoBody,
    /// A member's body would not convert to metres.
    MemberNotConverted,
    /// A member's body decoded but does not close: see `BimBrep::complete`.
    MemberIncomplete,
    /// The hull agrees and every member is drawn by a product of its own, so
    /// the element has nothing left to draw. See [`drawn_as_its_own_product`].
    EveryMemberDrawnAlone,
}

fn nested_assembly(
    element: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
    is_product: &dyn Fn(u32) -> bool,
) -> Result<Vec<BimBrep>, NestedRefusal> {
    let placements = &element.declared_placements;
    if placements.is_empty() {
        return Err(NestedRefusal::NotSeveral);
    }
    let bounds = element
        .placement_bounds
        .ok_or(NestedRefusal::NoElementBounds)?;
    let symbols = placements
        .iter()
        .map(|placement| {
            let symbol = elements.get(&placement.symbol_element_id?)?;
            Some((*placement, symbol))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(NestedRefusal::SymbolMissing)?;

    // The verification, before anything is built: the set has to be the
    // element, which is what reproducing the element's own box says.
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for (placement, symbol) in &symbols {
        let symbol_bounds = symbol
            .geometry_graph
            .as_ref()
            .ok_or(NestedRefusal::SymbolBoundsMissing)?
            .bounds;
        let (low, high) = GElementBounds::transformed(&symbol_bounds, placement);
        for axis in 0..3 {
            min[axis] = min[axis].min(low[axis]);
            max[axis] = max[axis].max(high[axis]);
        }
    }
    let agrees = |min: [f64; 3], max: [f64; 3]| {
        min.into_iter()
            .chain(max)
            .zip(bounds.min.into_iter().chain(bounds.max))
            .all(|(ours, theirs)| {
                ours.is_finite() && (ours - theirs).abs() <= BODY_BOUNDS_TOLERANCE_FEET
            })
    };

    // The record's own body, when the placements alone do not account for the
    // element's box. A window is the worked example: its record places the
    // sash and the frame as sub-instances *and* declares a body of its own,
    // and the box on the record bounds all of them together, so the symbols
    // alone can never reproduce it. Adding the record's own extent to the hull
    // is the same test asked of everything the record actually holds - and it
    // is a test, not an assumption: on AR S1 the widened hull agrees on all
    // six coordinates for the elements below and disagrees for the rest, which
    // a hull that merely grew would not do.
    //
    // Only where the symbols alone fall short. A record whose own body already
    // reproduces its box is the single-body case that `normalize_geometry`
    // answers before this, and drawing it here as well would draw it twice.
    let own_body = if agrees(min, max) {
        // The placements are the element. One of them is the single-instance
        // case, which the symbol path answers with the transform it verified.
        if placements.len() < 2 {
            return Err(NestedRefusal::NotSeveral);
        }
        None
    } else {
        let own = element
            .brep
            .as_ref()
            .filter(|brep| !brep.is_empty() && !element.brep_is_placed)
            .ok_or(NestedRefusal::HullDisagreed)?;
        let (low, high) = body_extent_feet(own).ok_or(NestedRefusal::OwnBodyUnusable)?;
        for axis in 0..3 {
            min[axis] = min[axis].min(low[axis]);
            max[axis] = max[axis].max(high[axis]);
        }
        if !agrees(min, max) {
            return Err(NestedRefusal::HullDisagreed);
        }
        let brep = normalize_brep(own, &IDENTITY_TRANSFORM, element.brep_requires_loop_closure)
            .ok_or(NestedRefusal::MemberNotConverted)?;
        if !brep.complete {
            return Err(NestedRefusal::OwnBodyUnusable);
        }
        Some(brep)
    };

    let mut parts = Vec::with_capacity(symbols.len() + usize::from(own_body.is_some()));
    parts.extend(own_body);
    let mut drawn_alone = 0_usize;
    for (placement, symbol) in &symbols {
        // A member that is an element of its own is drawn by its own product,
        // and drawing it here as well puts the same solid in the model twice:
        // a window's sill, each shaft of a composite column. It still counted
        // towards the hull above, because the element's box bounds it.
        if placement
            .symbol_element_id
            .is_some_and(|member| drawn_as_its_own_product(member, elements, is_product))
        {
            drawn_alone += 1;
            continue;
        }
        parts.push(member_part(placement, symbol, elements)?);
    }
    if parts.is_empty() {
        return Err(if drawn_alone > 0 {
            NestedRefusal::EveryMemberDrawnAlone
        } else {
            NestedRefusal::MemberHasNoBody
        });
    }
    Ok(parts)
}

/// One member's closed body, carried through the placement that names it.
///
/// A member that carries no body of its own is not necessarily empty: it can
/// be an instance, and then the body is one hop further, through the symbol
/// link its own box already verified. The two transforms compose, and the
/// composition is the route the member's box took into the hull - the
/// member's box is stated in the frame its own transform lands in, and the
/// hull carried that box through this same placement.
fn member_part(
    placement: &GInstanceTransformFields,
    symbol: &ExportedElement,
    elements: &BTreeMap<u32, ExportedElement>,
) -> Result<BimBrep, NestedRefusal> {
    let (source, placement) = if symbol.brep.is_some() {
        (symbol, *placement)
    } else {
        let inner = symbol
            .verified_symbol_bounds
            .as_ref()
            .and_then(|verified| elements.get(&verified.symbol_element_id))
            .filter(|inner| inner.brep.is_some())
            .ok_or(NestedRefusal::MemberHasNoBody)?;
        let transform = symbol
            .ginstance_transform
            .ok_or(NestedRefusal::MemberHasNoBody)?;
        (
            inner,
            GInstanceTransformFields::composed(placement, &transform),
        )
    };
    let brep =
        normalize_element_brep(source, &placement).ok_or(NestedRefusal::MemberNotConverted)?;
    if !brep.complete {
        return Err(NestedRefusal::MemberIncomplete);
    }
    Ok(brep)
}

/// Whether a nested family's member is drawn by a product of its own.
///
/// Revit places a shared nested family as an element in its own right, and
/// the export writes it as one: on AR S1 every member of an exported assembly
/// that is a `FamilyInstance` - 768 column shafts, 268 proxy parts, 185 window
/// sills - is also a product of its own, and not one that is a `FamilySymbol`
/// is. Which ids become products is the caller's selection, so it is asked
/// rather than guessed from the record. The member also has to carry a body
/// by itself: one that would fall back to a box, or to nothing, is still
/// drawn here, since leaving it out would lose a solid the host can draw.
///
/// Measured by what the two files draw, per solid: of AR S1's 509 hosts that
/// lose a part to this, 501 lose one whose world box the member's own product
/// reproduces to 0.1 mm. The other 8 are proxies whose anchors the host drew
/// 28 to 45 m across, because the writer stated a cone's axis in the wrong
/// frame - a fault that showed only where the cone sat far from its product's
/// origin, and that the member's own product therefore never had.
fn drawn_as_its_own_product(
    member: u32,
    elements: &BTreeMap<u32, ExportedElement>,
    is_product: &dyn Fn(u32) -> bool,
) -> bool {
    is_product(member)
        && elements.get(&member).is_some_and(|member| {
            matches!(
                normalize_geometry(member, BimElementType::Unknown, elements, &|_| false),
                Some(BimGeometry::Brep(_) | BimGeometry::Assembly(_))
            )
        })
}

/// Fill in the name and shading colour of every face material
/// [`normalize_brep`] left with only an identity, now that `elements` is in
/// scope to read them from - the same `MaterialElem` a compound layer's own
/// material names, so `normalize_material_layers` is not repeated here, only
/// its lookup.
fn resolve_face_materials(
    geometry: BimGeometry,
    elements: &BTreeMap<u32, ExportedElement>,
) -> BimGeometry {
    let source_of = |id: &str| id.parse::<u32>().ok().and_then(|id| elements.get(&id));
    let resolve_brep = |mut brep: BimBrep| {
        for face in &mut brep.faces {
            if let Some(material) = &mut face.material {
                if let Some(source) = material.id.as_ref().and_then(|id| source_of(&id.value)) {
                    material.name = source.name.as_ref().map(|(name, _)| name.clone());
                    material.color = source.material_color.map(|fields| BimColor {
                        red: fields.red,
                        green: fields.green,
                        blue: fields.blue,
                    });
                }
            }
        }
        brep
    };
    match geometry {
        BimGeometry::Brep(brep) => BimGeometry::Brep(resolve_brep(brep)),
        BimGeometry::Assembly(parts) => {
            BimGeometry::Assembly(parts.into_iter().map(resolve_brep).collect())
        }
        other @ (BimGeometry::AxisLine(_)
        | BimGeometry::BoundingBox(_)
        | BimGeometry::SweptDisk(_)) => other,
    }
}

fn normalize_geometry(
    element: &ExportedElement,
    element_type: BimElementType,
    elements: &BTreeMap<u32, ExportedElement>,
    is_product: &dyn Fn(u32) -> bool,
) -> Option<BimGeometry> {
    let metres = |value| revit_catalog::internal_feet_to_metres(value);
    let point = |coordinates: [f64; 3]| {
        Some(BimPoint3 {
            coordinates: [
                metres(coordinates[0])?,
                metres(coordinates[1])?,
                metres(coordinates[2])?,
            ],
            unit: metres_unit(),
        })
    };
    if let (Some(line), Some(bounds)) = (element.pipe_line_candidate, element.geometry_bounds) {
        if let Some(radius_feet) = line.swept_radius_feet(&bounds) {
            return Some(BimGeometry::SweptDisk(BimSweptDisk {
                directrix: BimLineSegment {
                    start: point(line.start.coordinates_feet)?,
                    end: point(line.end.coordinates_feet)?,
                },
                radius: BimNumber {
                    value: metres(radius_feet)?,
                    unit: Some(metres_unit()),
                },
            }));
        }
    }
    if let Some(line) = element.fitting_axis_candidate {
        return Some(BimGeometry::AxisLine(BimLineSegment {
            start: point(line.start.coordinates_feet)?,
            end: point(line.end.coordinates_feet)?,
        }));
    }
    // A body on the element's own record that reproduces that record's own
    // bounds needs no symbol and no transform: it is already in the source's
    // project coordinates, which is the frame every geometry here is carried
    // in and which the IFC layer expresses in the product's own frame. This is
    // the only path a system family has - a wall, a floor, a stair and a roof
    // are not placed from a symbol - and on AR S1 it is 12 482 of the 17 377
    // model elements against 577 reached through a symbol.
    // Except on a type definition. A record that declares its own category is
    // a type, not an instance - the clause `is_model_element` already uses -
    // and a `FamilySymbol` reproduces its record's box just as exactly while
    // that box is in the symbol's own local frame. On AR S1 that is 81 records
    // which would otherwise pile their bodies at the origin.
    if element.brep_is_placed && element.category_source != Some("declared") {
        if let Some(brep) = normalize_element_brep(element, &IDENTITY_TRANSFORM) {
            // Closed bodies only, for the reason the symbol path gives below:
            // IfcOpenShell refuses a large share of open shells.
            if brep.complete {
                return Some(BimGeometry::Brep(brep));
            }
        }
        // A body that reproduced its record's box but does not close is still
        // an element of that exact extent - the box is verified by the same
        // agreement that placed the body - so it carries the box, exactly as
        // an instance whose symbol body does not close carries its symbol's.
        if let Some(bounds) = element
            .brep_placement_box
            .filter(rvt_model::GElementBounds::is_volumetric)
        {
            return Some(BimGeometry::BoundingBox(BimBoundingBox {
                min: point(bounds.min)?,
                max: point(bounds.max)?,
            }));
        }
    }
    if !carries_family_symbol_geometry(element_type) {
        return None;
    }
    // Several declared placements, verified as a set. See `nested_assembly`:
    // this is the only path for an element no single transform describes.
    match nested_assembly(element, elements, is_product) {
        Ok(parts) => return Some(BimGeometry::Assembly(parts)),
        // The element is its members, and they are drawn. A box here would
        // draw them again.
        Err(NestedRefusal::EveryMemberDrawnAlone) => return None,
        Err(_) => {}
    }
    // A body the file already placed, reached by a declared placement the
    // cross-check cannot speak for. See `attach_placed_symbol_bodies`. Asked
    // before the verified symbol because an element that has one of these has
    // no verified symbol at all - the same cross-check refused it.
    if let Some(symbol) = element
        .placed_symbol_body
        .and_then(|symbol_element_id| elements.get(&symbol_element_id))
    {
        if let Some(geometry) = normalize_symbol_geometry(symbol, &IDENTITY_TRANSFORM) {
            return Some(geometry);
        }
    }
    let symbol = element.verified_symbol_bounds?;
    if let (Some(symbol_element), Some(transform)) = (
        elements.get(&symbol.symbol_element_id),
        element.ginstance_transform,
    ) {
        // Only a body whose every face resolved is emitted. An incomplete one
        // is schema-valid as an open `IfcShellBasedSurfaceModel`, and for a
        // handful of records it geometrizes, but at corpus scale it does not:
        // of SMALL's 1 771 incomplete shells IfcOpenShell builds 831 and fails
        // on 940, while all 695 complete bodies build. A body the reference
        // kernel refuses is not something to ship, so an incomplete one falls
        // back to the symbol's verified box. See `normalize_symbol_geometry`
        // for what "complete" allows beyond a single closed shell.
        if let Some(geometry) = normalize_symbol_geometry(symbol_element, &transform) {
            return Some(geometry);
        }
    }
    // A box flat on an axis is an extent, not a volume, and the IFC writer
    // will not make an `IfcBoundingBox` of one; saying so here keeps a product
    // from carrying a representation that silently holds nothing.
    //
    // The instance's box, not the symbol's: project coordinates are the frame
    // every geometry here is carried in, and the placed-body path above
    // states its own box in them. Handing the symbol's local box over instead
    // left the two boxes in two frames, which the writer could not tell apart
    // and read as one.
    symbol
        .instance_bounds
        .is_volumetric()
        .then(|| {
            Some(BimGeometry::BoundingBox(BimBoundingBox {
                min: point(symbol.instance_bounds.min)?,
                max: point(symbol.instance_bounds.max)?,
            }))
        })
        .flatten()
}

/// The rigid transform of a body that is already placed. Reusing
/// [`normalize_brep`] with it keeps one conversion from feet to metres and one
/// surface/curve mapping for both paths, rather than a second copy that could
/// drift from it.
const IDENTITY_TRANSFORM: GInstanceTransformFields = GInstanceTransformFields {
    offset: 0,
    basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    origin: RvtPoint3 {
        coordinates_feet: [0.0, 0.0, 0.0],
    },
    symbol_element_id: None,
};

/// Convert a body that already carries its own placement, in Revit internal
/// feet, to the metres the exporter works in.
#[must_use]
pub fn normalize_placed_brep(placed: &rvt_model::SymbolBrep) -> Option<BimBrep> {
    normalize_brep(placed, &IDENTITY_TRANSFORM, false)
}

fn normalize_element_brep(
    element: &ExportedElement,
    transform: &GInstanceTransformFields,
) -> Option<BimBrep> {
    normalize_brep(
        element.brep.as_ref()?,
        transform,
        element.brep_requires_loop_closure,
    )
}

/// A symbol's body where the whole record reads as one closed shell - or,
/// failing that, as the several closed shells it declares side by side.
///
/// A record is not always one body: a door's frame and its panel are two
/// closed solids under one `FamilySymbol`, and `SymbolBrep::bounds_a_volume`
/// and `SymbolBrep::is_closed` both judge the record as a whole, so a
/// perfectly closed frame sitting beside a perfectly closed panel - or beside
/// an open construction surface the loop-closure gate below already screens
/// out - reads as neither. `SymbolBrep::body` already exists to hand back one
/// declared body on its own; this is the first place anything asks for every
/// one of them; see the geometry gaps this closed on AR S1's doors, measured
/// against `s1_revit.ifc`.
///
/// Each candidate body is put through exactly the same
/// [`normalize_brep`]/`complete` test the whole-record reading uses - a body
/// this cannot verify closes on its own is left out rather than guessed
/// into the assembly, same as a lone symbol whose only body does not close.
/// Two or more surviving bodies become a
/// [`BimGeometry::Assembly`]; fewer than that is not an assembly reading and
/// falls through to whatever a caller tries next.
fn normalize_symbol_geometry(
    symbol: &ExportedElement,
    transform: &GInstanceTransformFields,
) -> Option<BimGeometry> {
    if let Some(brep) = normalize_element_brep(symbol, transform) {
        if brep.complete {
            return Some(BimGeometry::Brep(brep));
        }
    }
    let local = symbol.brep.as_ref()?;
    let parts: Vec<BimBrep> = (0..local.bodies.len())
        .filter_map(|index| local.body(index))
        .filter_map(|body| normalize_brep(&body, transform, symbol.brep_requires_loop_closure))
        .filter(|brep| brep.complete)
        .collect();
    (parts.len() >= 2).then_some(BimGeometry::Assembly(parts))
}

fn normalize_brep_points(
    points: &[[f64; 3]],
    world_point: &impl Fn([f64; 3]) -> Option<BimPoint3>,
) -> Option<Vec<BimPoint3>> {
    points.iter().copied().map(world_point).collect()
}

/// The profile a revolved surface turns, in that surface's own frame: still a
/// length, so still metres, but the frame's axes already carry the instance's
/// rotation and it must not be applied twice.
/// Place one side of a ruled surface in world coordinates and metres.
///
/// A line profile's parameter is a length, so it converts with the origin; an
/// arc's is an angle and does not.
fn world_ruling(
    ruling: rvt_model::BrepRuling,
    world_point: &impl Fn([f64; 3]) -> Option<BimPoint3>,
    world_direction: &impl Fn([f64; 3]) -> [f64; 3],
) -> Option<BimBrepRuling> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    Some(match ruling {
        rvt_model::BrepRuling::Point(point) => BimBrepRuling::Point(world_point(point)?),
        rvt_model::BrepRuling::Curve {
            profile: rvt_model::BrepProfile::Line { origin, direction },
            start,
            end,
        } => BimBrepRuling::Curve {
            profile: Box::new(BimBrepProfile::Line {
                origin: world_point(origin)?,
                direction: world_direction(direction),
            }),
            start: metres(start)?,
            end: metres(end)?,
        },
        rvt_model::BrepRuling::Curve {
            profile:
                rvt_model::BrepProfile::Arc {
                    center,
                    x_axis,
                    y_axis,
                    radius,
                },
            start,
            end,
        } => BimBrepRuling::Curve {
            profile: Box::new(BimBrepProfile::Arc {
                center: world_point(center)?,
                x_axis: world_direction(x_axis),
                y_axis: world_direction(y_axis),
                radius: BimNumber {
                    value: metres(radius)?,
                    unit: Some(metres_unit()),
                },
            }),
            start,
            end,
        },
    })
}

#[must_use]
fn normalize_brep_profile(profile: rvt_model::BrepProfile) -> Option<BimBrepProfile> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    let frame_point = |point: [f64; 3]| -> Option<BimPoint3> {
        Some(BimPoint3 {
            coordinates: [metres(point[0])?, metres(point[1])?, metres(point[2])?],
            unit: metres_unit(),
        })
    };
    Some(match profile {
        rvt_model::BrepProfile::Line { origin, direction } => BimBrepProfile::Line {
            origin: frame_point(origin)?,
            direction,
        },
        rvt_model::BrepProfile::Arc {
            center,
            x_axis,
            y_axis,
            radius,
        } => BimBrepProfile::Arc {
            center: frame_point(center)?,
            x_axis,
            y_axis,
            radius: BimNumber {
                value: metres(radius)?,
                unit: Some(metres_unit()),
            },
        },
    })
}

/// Place a symbol-local `SymbolBrep` (Revit internal feet) into world
/// coordinates via the instance's own `GInstance` transform, and convert to
/// metres. Reuses the same rigid transform
/// [`GElementBounds::matches_transformed`] already applies to bounding-box
/// corners: `world = origin + sum_i local[i] * basis[i]` for a point, and
/// the same sum without `origin` for a direction.
/// One face's surface, placed in world coordinates and metres.
fn world_brep_surface(
    surface: rvt_model::BrepSurface,
    world_point: &impl Fn([f64; 3]) -> Option<BimPoint3>,
    world_direction: &impl Fn([f64; 3]) -> [f64; 3],
) -> Option<BimBrepSurface> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    Some(match surface {
        rvt_model::BrepSurface::Plane {
            origin,
            x_axis,
            y_axis,
        } => BimBrepSurface::Plane {
            origin: world_point(origin)?,
            x_axis: world_direction(x_axis),
            y_axis: world_direction(y_axis),
        },
        rvt_model::BrepSurface::Cylinder {
            center,
            x_axis,
            y_axis,
            z_axis,
            radius,
        } => BimBrepSurface::Cylinder {
            center: world_point(center)?,
            x_axis: world_direction(x_axis),
            y_axis: world_direction(y_axis),
            z_axis: world_direction(z_axis),
            radius: BimNumber {
                value: metres(radius)?,
                unit: Some(metres_unit()),
            },
        },
        // The frame goes to world coordinates; the profile stays in the
        // frame's own, which is the only place its numbers mean anything.
        rvt_model::BrepSurface::Revolution {
            center,
            x_axis,
            y_axis,
            z_axis,
            profile,
        } => BimBrepSurface::Revolution {
            center: world_point(center)?,
            x_axis: world_direction(x_axis),
            y_axis: world_direction(y_axis),
            z_axis: world_direction(z_axis),
            profile: Box::new(normalize_brep_profile(profile)?),
        },
        // `RuledSurf` states both profiles in the body's own coordinates
        // rather than in a frame of the surface's, so unlike a
        // revolution's profile they are placed in world coordinates like
        // any other point.
        rvt_model::BrepSurface::Ruled { first, second } => BimBrepSurface::Ruled {
            first: world_ruling(first, &world_point, &world_direction)?,
            second: world_ruling(second, &world_point, &world_direction)?,
        },
    })
}

/// How much extent a body needs along every axis, in metres, before it
/// counts as enclosing a volume rather than collapsing flat.
///
/// Well above the noise a rigid transform leaves in a coordinate (see
/// `FINGERPRINT_GRID` in `ifc-export` for the same order-of-magnitude
/// argument) and well below any real product's thickness - a door leaf is
/// tens of millimetres, not tenths.
const MIN_VOLUME_EXTENT_METRES: f64 = 1.0e-4;

/// Whether `faces` reaches real extent along all three axes, rather than
/// collapsing flat along one of them.
///
/// A topological closure test - every edge two-sided, the face on the other
/// side one this body itself owns - cannot see this: it reads no coordinate.
/// A flat plan symbol's front and back faces, drawn at the same elevation
/// and sharing every edge, close under that test exactly as a real solid
/// does. This reads every point the faces carry instead.
fn spans_a_volume(faces: &[BimBrepFace]) -> bool {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for face in faces {
        for loop_edges in &face.loops {
            for edge in loop_edges {
                for point in [&edge.start, &edge.end] {
                    for axis in 0..3 {
                        let value = point.coordinates[axis];
                        min[axis] = min[axis].min(value);
                        max[axis] = max[axis].max(value);
                    }
                }
            }
        }
    }
    (0..3).all(|axis| max[axis] - min[axis] > MIN_VOLUME_EXTENT_METRES)
}

#[must_use]
fn normalize_brep(
    local: &rvt_model::SymbolBrep,
    transform: &GInstanceTransformFields,
    requires_loop_closure: bool,
) -> Option<BimBrep> {
    let metres = |value: f64| revit_catalog::internal_feet_to_metres(value);
    let world_point = |local_point: [f64; 3]| -> Option<BimPoint3> {
        let mut world = transform.origin.coordinates_feet;
        for (axis_local, value) in local_point.into_iter().enumerate() {
            for (axis_world, component) in world.iter_mut().enumerate() {
                *component += transform.basis[axis_local][axis_world] * value;
            }
        }
        Some(BimPoint3 {
            coordinates: [metres(world[0])?, metres(world[1])?, metres(world[2])?],
            unit: metres_unit(),
        })
    };
    let world_direction = |local_direction: [f64; 3]| -> [f64; 3] {
        let mut world = [0.0; 3];
        for (axis_local, value) in local_direction.into_iter().enumerate() {
            for (axis_world, component) in world.iter_mut().enumerate() {
                *component += transform.basis[axis_local][axis_world] * value;
            }
        }
        world
    };
    let mut redundant_faces = local.redundant_duplicate_face_indexes();
    if redundant_faces.is_empty() {
        redundant_faces = local.free_surface_face_indexes();
    }
    let mut faces = Vec::with_capacity(local.faces.len() - redundant_faces.len());
    for (face_index, face) in local.faces.iter().enumerate() {
        if redundant_faces.binary_search(&face_index).is_ok() {
            continue;
        }
        let surface = world_brep_surface(face.surface, &world_point, &world_direction)?;
        let mut loops = Vec::with_capacity(face.loops.len());
        for loop_edges in &face.loops {
            let mut edges = Vec::with_capacity(loop_edges.len());
            for edge in loop_edges {
                let curve = match &edge.curve {
                    rvt_model::BrepCurve::Line => BimBrepCurve::Line,
                    rvt_model::BrepCurve::Arc(arc) => BimBrepCurve::Arc(Box::new(BimBrepArc {
                        center: world_point(arc.center)?,
                        x_axis: world_direction(arc.x_axis),
                        z_axis: world_direction(arc.z_axis),
                        radius: BimNumber {
                            value: metres(arc.radius)?,
                            unit: Some(metres_unit()),
                        },
                        start_angle: arc.start_angle,
                        end_angle: arc.end_angle,
                    })),
                    rvt_model::BrepCurve::Polyline(points) => BimBrepCurve::Polyline(
                        normalize_brep_points(points, &world_point)?.into_boxed_slice(),
                    ),
                };
                edges.push(BimBrepEdge {
                    start: world_point(edge.start)?,
                    end: world_point(edge.end)?,
                    curve,
                });
            }
            loops.push(edges);
        }
        // `MaterialElem` is looked up once every element's own geometry has
        // been built, in `resolve_face_materials` - not here, where the
        // element table is not in scope. Carried as a bare identity meanwhile
        // so the lookup that comes after this has something to resolve.
        let material = face.material_id.map(|id| {
            Box::new(BimMaterial {
                id: Some(BimExternalId {
                    system: "autodesk.revit.elementId".to_owned(),
                    value: id.to_string(),
                }),
                name: None,
                color: None,
            })
        });
        faces.push(BimBrepFace {
            surface,
            loops,
            material,
        });
    }
    // Usually the source topology and the loops being written are two
    // independent ways to establish closure, and either is enough. A
    // cut-face pass can make them contradict each other, however: element
    // 7281744's kept shell is topologically closed while its own loops
    // leave an edge unmatched, and rebuilding the record lands a different
    // body on the same box. Only for that measured population does the
    // face-loop reading become mandatory. The different rebuilt body is
    // not substituted; it is evidence against this one's closed claim.
    // Exact, same-winding duplicate faces are omitted only when the
    // remainder closes independently. In that case the loops actually
    // written provide the closed-shell claim; the duplicated interior
    // sheet is not allowed to add a spurious divergence volume. Surfaces
    // the file declares free are omitted on the same terms, and inside the
    // solid only: see `SymbolBrep::free_surface_face_indexes`.
    let topologically_closed = !redundant_faces.is_empty()
        || local.bounds_a_volume()
        || (!requires_loop_closure && local.is_closed());
    // Neither test above reads a coordinate: `GEdge.m_pFace` bookkeeping can
    // report a body "closed" - every edge two-sided, on the other side a face
    // this body itself owns - for a flat plan symbol whose front and back
    // faces coincide at the same elevation, or for a single unbounded face
    // the decode isolated into a body of its own. Neither encloses anything.
    // Measured on AR S1: before this gate, splitting a symbol's several
    // bodies (see `normalize_symbol_geometry`) turned every 2D furniture and
    // plumbing-fixture symbol Revit's own IFC export carries none of into a
    // "closed" body IfcOpenShell then failed or accepted as a zero-height
    // shell - 89 more `IfcFurnishingElement` and all 339
    // `IfcSanitaryTerminal` this file has none of in `s1_revit.ifc`. Requires
    // a real 3D extent, not a genuine volume measurement - `ifc-export`'s own
    // `quantities` module measures that later, from planar-and-straight
    // bodies only, which this gate does not require.
    Some(BimBrep {
        complete: topologically_closed && spans_a_volume(&faces),
        faces,
    })
}

#[must_use]
fn metres_unit() -> BimUnit {
    // Shared rather than built: the per-point closures in `normalize_brep`
    // call this for every coordinate they convert, and a model runs to
    // hundreds of millions of them.
    BimUnit::metres()
}

/// The name a property carries when nothing names its parameter.
///
/// A parameter is named by its own `ParameterElement` where the file declares
/// one, and otherwise by Autodesk's published `BuiltInParameter` table. A
/// built-in code the table does not list has neither, and its own code stands
/// in. Two codes reach every wall in AR S1 that way - -1 001 111 and
/// -1 001 101, 10 966 elements each - and Revit's own IFC export writes
/// neither: they are internal bookkeeping, not parameters a reader can use.
///
/// [`metadata_model`] drops what still carries this name, so the canonical
/// model holds only parameters something names. The JSON export keeps them,
/// since it writes the decoded intermediate rather than the model. Both sides
/// ask here, so the rule is stated once.
#[must_use]
pub fn placeholder_parameter_name(id: i32) -> String {
    format!("param_{id}")
}

/// Whether nothing in the file or in the catalogue names this property's
/// parameter. See [`placeholder_parameter_name`].
#[must_use]
pub fn names_nothing(property: &BimProperty) -> bool {
    property.id.as_ref().is_some_and(|id| {
        id.value
            .parse::<i32>()
            .is_ok_and(|code| property.name == placeholder_parameter_name(code))
    })
}

#[must_use]
fn normalize_property(
    parameter: &rvt_model::Parameter,
    parameter_names: &BTreeMap<i32, String>,
    parameter_specs: &BTreeMap<i32, String>,
    catalog: Option<Catalog>,
) -> BimProperty {
    let built_in = catalog.and_then(|catalog| catalog.built_in_parameter(parameter.id));
    let name = parameter_names
        .get(&parameter.id)
        .cloned()
        .or_else(|| built_in.map(|parameter| parameter.display_name.to_owned()))
        .unwrap_or_else(|| placeholder_parameter_name(parameter.id));
    // A user parameter declares its spec on its own `ParameterElement`; a
    // built-in one declares nothing anywhere in the file and takes it from
    // Revit's definition, which is what the catalog holds. Without the
    // fallback every built-in double left as a raw internal number: on AR S1
    // that is `Unconnected Height` reaching the IFC as 13.12 rather than as a
    // 4 m length.
    let specification = parameter_specs.get(&parameter.id).cloned().or_else(|| {
        catalog
            .and_then(|catalog| catalog.built_in_parameter_specification(parameter.id))
            .map(str::to_owned)
    });
    let value = match &parameter.value {
        ParameterValue::Double(number) => {
            let normalized = specification
                .as_deref()
                .and_then(|type_id| catalog.and_then(|catalog| catalog.specification(type_id)))
                .and_then(|specification| {
                    Some((specification, specification.from_internal(*number)?))
                });
            let (value, unit) = normalized.map_or((*number, None), |(specification, value)| {
                (
                    value,
                    Some(BimUnit::new(
                        specification.storage_unit,
                        specification.storage_unit_name,
                    )),
                )
            });
            BimPropertyValue::Number(BimNumber { value, unit })
        }
        ParameterValue::Integer(number) => BimPropertyValue::Integer(i64::from(*number)),
        ParameterValue::Text(text) => BimPropertyValue::Text(text.clone()),
        ParameterValue::Reference(reference) => {
            BimPropertyValue::Reference(BimElementId(reference.to_string()))
        }
    };
    BimProperty {
        id: Some(BimExternalId {
            system: if parameter.is_built_in() {
                "autodesk.revit.builtInParameter"
            } else {
                "autodesk.revit.parameterElementId"
            }
            .to_owned(),
            value: parameter.id.to_string(),
        }),
        name,
        specification,
        value,
    }
}

/// Read the element's name, preferring the offset the class agrees on and
/// falling back to the first readable string after the `Element` tail.
#[must_use]
pub fn read_name(
    body: &[u8],
    tail_end: usize,
    settled_offset: Option<usize>,
) -> Option<(String, &'static str)> {
    if let Some(offset) = settled_offset {
        if let Some(found) = RecordString::parse_at(body, tail_end + offset) {
            return Some((found.value, "offset"));
        }
    }
    RecordString::scan_from(body, tail_end).map(|found| (found.value, "scan"))
}

/// The file's class schema, or `None` where it declares no `Formats/Latest`.
///
/// # Errors
///
/// Fails where the stream is present but does not decode.
pub fn read_schema(container: &RvtContainer) -> Result<Option<Schema>, Box<dyn Error>> {
    if container.stream("Formats/Latest").is_none() {
        return Ok(None);
    }
    let raw = container.read_stream_with_limit("Formats/Latest", DEFAULT_DECODE_LIMIT as u64)?;
    Ok(Some(decode_schema_stream(&raw)?.1))
}

#[must_use]
fn schema_retry_error(raw: &str, stripped: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "schema parse failed before checksum-page cleanup ({raw}); retry also failed ({stripped})"
        ),
    )
}

/// Which file, and which save of it, this container is.
///
/// Two streams state the same identity and both are read, because they are
/// the two halves of one answer. `BasicFileInfo` names its fields in plain
/// text - `Unique Document GUID`, `Unique Document Increments` - so nothing
/// about them has to be inferred, and it is also where the save counter
/// lives. `Global/History` carries the same GUID structurally, as the newest
/// episode, and the five document-level GUIDs beside it that no labelled
/// field states.
///
/// Where both name the GUID they are required to agree: on the 74-file corpus
/// they do, 74 out of 74. A file where they did not would be one this does
/// not understand, and it states no identity at all rather than picking a
/// side - an identity that is wrong is worse here than one that is missing,
/// since the whole point of it is to decide whether two files are the same.
///
/// `None` where neither stream states a GUID.
///
/// # Errors
///
/// Fails where a stream is present but its framing does not decode.
pub fn document_identity(
    container: &RvtContainer,
) -> Result<Option<BimDocumentIdentity>, Box<dyn Error>> {
    let labelled = read_basic_file_info(container)?;
    let history = decode_global_stream(container, "Global/History")?;
    let episodes = history
        .as_deref()
        .and_then(rvt_model::EpisodeTable::parse)
        .and_then(|episodes| episodes.newest())
        .map(format_guid);
    let guids = history.as_deref().and_then(rvt_model::DocumentGuids::parse);
    let stated = labelled
        .as_ref()
        .and_then(|info| info.document_guid.clone());
    let document_guid = match (stated, episodes) {
        (Some(stated), Some(episode)) if stated != episode => None,
        (stated, episode) => stated.or(episode),
    };
    if document_guid.is_none() {
        return Ok(None);
    }
    Ok(Some(BimDocumentIdentity {
        document_guid,
        increment: labelled.as_ref().and_then(|info| info.document_increments),
        // A nil GUID is a field the document leaves unset, not a lineage.
        creation_guid: guids.and_then(|guids| optional_guid(guids.creation)),
        detach_guid: guids.and_then(|guids| optional_guid(guids.detach)),
        worksharing: labelled.and_then(|info| info.worksharing),
    }))
}

/// A GUID the way `BasicFileInfo` states one, so the two sources can be
/// compared as text and either can fill the field.
fn format_guid(bytes: [u8; 16]) -> String {
    let hex = bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
        text
    });
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn optional_guid(bytes: [u8; 16]) -> Option<String> {
    (bytes != [0; 16]).then(|| format_guid(bytes))
}

/// The file's `BasicFileInfo`, or `None` where it has none.
///
/// # Errors
///
/// Fails where the stream is present but does not decode.
pub fn read_basic_file_info(
    container: &RvtContainer,
) -> Result<Option<BasicFileInfo>, Box<dyn Error>> {
    if container.stream("BasicFileInfo").is_none() {
        return Ok(None);
    }
    let bytes = container.read_stream_with_limit("BasicFileInfo", 16 * 1024 * 1024)?;
    Ok(Some(BasicFileInfo::parse(&bytes)?))
}

#[cfg(test)]
mod tests {
    use rvt_schema::TypeReference;

    use super::*;

    /// A record whose body runs past its own member is completed from the
    /// head of the next one, which is exactly the run that member's own walk
    /// skips as `leading_carry`.
    #[test]
    fn takes_the_bytes_a_record_is_short_off_the_next_member() {
        let next = [1_u8, 2, 3, 4, 5, 6];
        let tail = spilled_tail(4, [next.as_slice()].into_iter());
        assert_eq!(tail, Some(vec![1, 2, 3, 4]));
    }

    /// A record longer than a member spans several, so the tail is taken from
    /// each in turn until nothing is owing.
    #[test]
    fn takes_a_tail_that_spans_several_members() {
        let first = [1_u8, 2, 3];
        let second = [4_u8, 5, 6];
        let third = [7_u8, 8];
        let tail = spilled_tail(
            8,
            [first.as_slice(), second.as_slice(), third.as_slice()].into_iter(),
        );
        assert_eq!(tail, Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));
    }

    /// Nothing is stitched unless all of it is found: a body completed in part
    /// is not the record, and half a body would decode as a walk that stops
    /// early rather than as the absence the caller can act on.
    #[test]
    fn refuses_a_tail_the_following_members_do_not_hold() {
        let only = [1_u8, 2, 3];
        assert_eq!(spilled_tail(5, [only.as_slice()].into_iter()), None);
        assert_eq!(spilled_tail(5, std::iter::empty()), None);
    }

    #[test]
    fn promotes_only_a_bounds_verified_pipe_to_metric_geometry() {
        let element = ExportedElement {
            pipe_line_candidate: Some(PipeLineGeometryFields {
                line_offset: 100,
                nominal_diameter_feet: 0.2,
                start: rvt_model::RvtPoint3 {
                    coordinates_feet: [12.0, 20.0, 30.0],
                },
                end: rvt_model::RvtPoint3 {
                    coordinates_feet: [15.0, 20.0, 30.0],
                },
            }),
            geometry_bounds: Some(GElementBounds {
                offset: 24,
                min: [12.0, 19.88, 29.88],
                max: [15.0, 20.12, 30.12],
            }),
            ..ExportedElement::default()
        };

        let Some(BimGeometry::SweptDisk(geometry)) = normalize_geometry(
            &element,
            BimElementType::PipeSegment,
            &BTreeMap::new(),
            &|_| false,
        ) else {
            panic!("verified pipe geometry was not promoted");
        };
        assert!((geometry.directrix.start.coordinates[0] - 3.6576).abs() < 1.0e-12);
        assert!((geometry.directrix.end.coordinates[0] - 4.572).abs() < 1.0e-12);
        assert!((geometry.radius.value - 0.036_576).abs() < 1.0e-12);

        let mut mismatched = element;
        mismatched.geometry_bounds.as_mut().unwrap().max[2] += 1.0;
        assert!(
            normalize_geometry(
                &mismatched,
                BimElementType::PipeSegment,
                &BTreeMap::new(),
                &|_| false
            )
            .is_none()
        );
    }

    #[test]
    fn promotes_an_owner_verified_fitting_axis_without_inventing_a_body() {
        let element = ExportedElement {
            fitting_axis_candidate: Some(FittingCenterLineFields {
                owner_element_id: 417_660,
                start: rvt_model::RvtPoint3 {
                    coordinates_feet: [10.0, 20.0, 30.0],
                },
                end: rvt_model::RvtPoint3 {
                    coordinates_feet: [10.0, 20.0, 32.0],
                },
            }),
            ..ExportedElement::default()
        };
        let Some(BimGeometry::AxisLine(line)) = normalize_geometry(
            &element,
            BimElementType::PipeFitting,
            &BTreeMap::new(),
            &|_| false,
        ) else {
            panic!("verified fitting axis was not promoted");
        };
        for (actual, expected) in line
            .start
            .coordinates
            .into_iter()
            .chain(line.end.coordinates)
            .zip([3.048, 6.096, 9.144, 3.048, 6.096, 9.7536])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
    }

    #[test]
    fn promotes_a_bounds_verified_ginstance_transform_to_metric_placement() {
        let transform = GInstanceTransformFields {
            offset: 164,
            basis: [[0.0, 0.0, 1.0], [0.0, -1.0, 0.0], [1.0, 0.0, 0.0]],
            origin: rvt_model::RvtPoint3 {
                coordinates_feet: [10.0, 20.0, 30.0],
            },
            symbol_element_id: Some(417_391),
        };
        let placement = normalize_placement(Some(transform)).unwrap();
        for (actual, expected) in placement
            .origin
            .coordinates
            .into_iter()
            .zip([3.048, 6.096, 9.144])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
        for (actual, expected) in placement
            .reference_direction
            .into_iter()
            .chain(placement.axis)
            .zip(transform.basis[0].into_iter().chain(transform.basis[2]))
        {
            assert!((actual - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn links_a_placed_symbol_body_only_when_it_is_unique_and_contained() {
        let bounds = |min: [f64; 3], max: [f64; 3]| GElementBounds {
            offset: 0,
            min,
            max,
        };
        let symbol = |placed: bool| ExportedElement {
            brep_is_placed: placed,
            brep_placement_box: Some(bounds([1.0, 1.0, 1.0], [2.0, 2.0, 2.0])),
            ..ExportedElement::default()
        };
        let instance = |origin: [f64; 3], own: GElementBounds| ExportedElement {
            ginstance_transform: Some(GInstanceTransformFields {
                offset: 0,
                basis: IDENTITY_TRANSFORM.basis,
                origin: rvt_model::RvtPoint3 {
                    coordinates_feet: origin,
                },
                symbol_element_id: Some(5),
            }),
            placement_bounds: Some(own),
            ..ExportedElement::default()
        };
        let around = bounds([0.0, 0.0, 0.0], [3.0, 3.0, 3.0]);
        let model = |instances: Vec<(u32, ExportedElement)>, symbol: ExportedElement| {
            let mut elements = BTreeMap::new();
            elements.insert(5, symbol);
            elements.extend(instances);
            attach_placed_symbol_bodies(&mut elements);
            elements
        };

        // The stair case: identity placement, a symbol whose body is already
        // placed, its box inside the element's own, and nothing else naming it.
        let linked = model(vec![(9, instance([0.0; 3], around))], symbol(true));
        assert_eq!(linked[&9].placed_symbol_body, Some(5));

        // A symbol two elements name is not one element's body.
        let shared = model(
            vec![
                (9, instance([0.0; 3], around)),
                (11, instance([0.0; 3], around)),
            ],
            symbol(true),
        );
        assert_eq!(shared[&9].placed_symbol_body, None);
        assert_eq!(shared[&11].placed_symbol_body, None);

        // A body outside the element's own extent is not this element's.
        let outside = model(
            vec![(9, instance([0.0; 3], bounds([5.0; 3], [6.0; 3])))],
            symbol(true),
        );
        assert_eq!(outside[&9].placed_symbol_body, None);

        // A transform that is not the identity has something to verify, and
        // the bounds cross-check is what verifies it.
        let moved = model(vec![(9, instance([4.0, 0.0, 0.0], around))], symbol(true));
        assert_eq!(moved[&9].placed_symbol_body, None);

        // A symbol body in its own local frame is not already placed.
        let unplaced = model(vec![(9, instance([0.0; 3], around))], symbol(false));
        assert_eq!(unplaced[&9].placed_symbol_body, None);
    }

    #[test]
    fn attaches_symbol_bounds_only_when_the_symbol_box_has_volume() {
        let symbol_bounds = |max: [f64; 3]| GElementBounds {
            offset: 54,
            min: [-1.0, -2.0, -3.0],
            max,
        };
        let instance_bounds = |max: [f64; 3]| GElementBounds {
            offset: 12,
            min: [9.0, 18.0, 27.0],
            max: [10.0 + max[0], 20.0 + max[1], 30.0 + max[2]],
        };
        let model = |max: [f64; 3]| {
            let mut elements = BTreeMap::new();
            elements.insert(
                5,
                ExportedElement {
                    class_index: Some(7),
                    category: Some(1),
                    geometry_graph: Some(GElementGraphFields {
                        top_level_nodes: Vec::new(),
                        bounds: symbol_bounds(max),
                        tight_bounds: symbol_bounds(max),
                    }),
                    ..ExportedElement::default()
                },
            );
            elements.insert(
                9,
                ExportedElement {
                    class_index: Some(3),
                    category: Some(1),
                    ginstance_transform: Some(GInstanceTransformFields {
                        offset: 0,
                        basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                        origin: rvt_model::RvtPoint3 {
                            coordinates_feet: [10.0, 20.0, 30.0],
                        },
                        symbol_element_id: Some(5),
                    }),
                    placement_bounds: Some(instance_bounds(max)),
                    ..ExportedElement::default()
                },
            );
            elements
        };

        let mut volumetric = model([1.0, 2.0, 3.0]);
        attach_symbol_bounds(&mut volumetric);
        assert_eq!(
            volumetric[&9].verified_symbol_bounds,
            Some(VerifiedSymbolBounds {
                symbol_element_id: 5,
                bounds: symbol_bounds([1.0, 2.0, 3.0]),
                instance_bounds: instance_bounds([1.0, 2.0, 3.0]),
            })
        );

        // A symbol box flat on an axis is still a verified link - the two
        // boxes agree, which is the whole of what the link claims - but it is
        // an extent and not a volume, so it becomes no representation. The
        // refusal belongs where the box would be written, not where the link
        // is made.
        let mut flat = model([1.0, 2.0, -3.0]);
        attach_symbol_bounds(&mut flat);
        assert_eq!(
            flat[&9].verified_symbol_bounds,
            Some(VerifiedSymbolBounds {
                symbol_element_id: 5,
                bounds: symbol_bounds([1.0, 2.0, -3.0]),
                instance_bounds: instance_bounds([1.0, 2.0, -3.0]),
            })
        );
        assert_eq!(
            normalize_geometry(&flat[&9], BimElementType::Unknown, &flat, &|_| false),
            None
        );
    }

    #[test]
    fn an_instance_without_a_category_takes_its_verified_symbol_s() {
        // The bounds cross-check is the verification; the category is then
        // inherited rather than required to match, because most placed family
        // instances in the corpus carry no category of their own and the
        // exporter drops a categoryless element with its body. A category that
        // *disagrees* still refuses the link.
        let symbol_bounds = GElementBounds {
            offset: 54,
            min: [-1.0, -2.0, -3.0],
            max: [1.0, 2.0, 3.0],
        };
        let model = |instance_category: Option<i32>| {
            let mut elements = BTreeMap::new();
            elements.insert(
                5,
                ExportedElement {
                    class_index: Some(7),
                    category: Some(-2_008_049),
                    geometry_graph: Some(GElementGraphFields {
                        top_level_nodes: Vec::new(),
                        bounds: symbol_bounds,
                        tight_bounds: symbol_bounds,
                    }),
                    ..ExportedElement::default()
                },
            );
            elements.insert(
                9,
                ExportedElement {
                    class_index: Some(3),
                    category: instance_category,
                    ginstance_transform: Some(GInstanceTransformFields {
                        offset: 0,
                        basis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                        origin: rvt_model::RvtPoint3 {
                            coordinates_feet: [10.0, 20.0, 30.0],
                        },
                        symbol_element_id: Some(5),
                    }),
                    placement_bounds: Some(GElementBounds {
                        offset: 12,
                        min: [9.0, 18.0, 27.0],
                        max: [11.0, 22.0, 33.0],
                    }),
                    ..ExportedElement::default()
                },
            );
            elements
        };

        let mut inherited = model(None);
        attach_symbol_bounds(&mut inherited);
        assert!(inherited[&9].verified_symbol_bounds.is_some());
        assert_eq!(inherited[&9].category, Some(-2_008_049));
        assert_eq!(inherited[&9].category_source, Some("symbol"));

        // A category the element declared is kept, and its provenance with it.
        let mut declared = model(Some(-2_008_049));
        attach_symbol_bounds(&mut declared);
        assert!(declared[&9].verified_symbol_bounds.is_some());
        assert_eq!(declared[&9].category_source, None);

        // Two categories that disagree refuse the link outright.
        let mut disagreeing = model(Some(-2_000_151));
        attach_symbol_bounds(&mut disagreeing);
        assert_eq!(disagreeing[&9].verified_symbol_bounds, None);
        assert_eq!(disagreeing[&9].category, Some(-2_000_151));
    }

    /// One class in a test schema: its index, its name, and the index and name
    /// of its parent where it has one.
    type NamedClass<'a> = (u16, &'a str, Option<(u16, &'a str)>);

    /// A schema of empty classes, for a test that only needs class names and
    /// the chain between them.
    fn named_classes(classes: &[NamedClass<'_>]) -> Schema {
        Schema {
            classes: classes
                .iter()
                .map(|(index, name, parent)| rvt_schema::ClassDefinition {
                    index: *index,
                    name: (*name).to_owned(),
                    name_bytes: name.as_bytes().to_vec(),
                    parent: parent.map_or(TypeReference::None, |(index, name)| {
                        TypeReference::Reference {
                            index,
                            name: name.to_owned(),
                        }
                    }),
                    version: 1,
                    properties: Vec::new(),
                    guids: Vec::new(),
                    unknown_word: 0,
                    inline: false,
                    offset: 0,
                    end_offset: 0,
                })
                .collect(),
            top_level_class_count: 0,
            property_count: 0,
            parsed_property_count: 0,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
            unresolved_references: Vec::new(),
            inline_index_mismatches: Vec::new(),
        }
    }

    /// The category reaches an instance through its type's family, and a
    /// record declaring an `m_categoryId` that is not a family cannot answer.
    #[test]
    fn an_instance_takes_the_category_its_family_declares() {
        // `class_by_index` addresses the list from `INITIAL_CLASS_INDEX`, so
        // the classes must start there and stay contiguous.
        let schema = named_classes(&[
            (12, "Element", None),
            (13, "FamilyBase", None),
            (14, "Family", Some((13, "FamilyBase"))),
            (15, "FamilySymbol", None),
            (16, "FamilyInstance", None),
            (17, "ScheduleSchema", None),
        ]);
        let mut elements = BTreeMap::new();
        // The family, and an impostor of another class declaring the same
        // property with a different value.
        elements.insert(
            700,
            ExportedElement {
                class_index: Some(14),
                declared_category_id: Some(-2_000_126),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            701,
            ExportedElement {
                class_index: Some(17),
                declared_category_id: Some(-2_000_100),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            800,
            ExportedElement {
                class_index: Some(15),
                family_element_id: Some(700),
                ..ExportedElement::default()
            },
        );
        // A symbol naming the impostor gets nothing from it.
        elements.insert(
            801,
            ExportedElement {
                class_index: Some(15),
                family_element_id: Some(701),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            900,
            ExportedElement {
                class_index: Some(16),
                type_element_id: Some(800),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            901,
            ExportedElement {
                class_index: Some(16),
                type_element_id: Some(801),
                ..ExportedElement::default()
            },
        );
        // An element that declared its own category keeps it.
        elements.insert(
            902,
            ExportedElement {
                class_index: Some(16),
                type_element_id: Some(800),
                category: Some(-2_000_151),
                category_source: Some("declared"),
                ..ExportedElement::default()
            },
        );

        inherit_family_categories(&mut elements, Some(&schema));

        assert_eq!(elements[&900].category, Some(-2_000_126));
        assert_eq!(elements[&900].category_source, Some("family"));
        // The type carries it too: its own family declares it.
        assert_eq!(elements[&800].category, Some(-2_000_126));
        assert_eq!(elements[&901].category, None, "a schedule is not a family");
        assert_eq!(elements[&902].category, Some(-2_000_151));
        assert_eq!(elements[&902].category_source, Some("declared"));
        // Only a declared category marks a record as a type or definition.
        assert!(elements[&902].declares_a_category());
        assert!(!elements[&900].declares_a_category());
    }

    /// A type or a definition is not a product, whichever way into the export
    /// it takes. The join against Revit's own export of AR S1 says so without
    /// an exception - of the 11 291 products we share with it not one declares
    /// a category - so the reading belongs to the selection as a whole and not
    /// to the one way in that happened to state it.
    #[test]
    fn a_record_declaring_its_category_is_never_a_candidate() {
        let schema = named_classes(&[(12, "Element", None), (13, "SWall", None)]);
        let mut elements = BTreeMap::new();
        // An instance: a building class, in a phase, on a level, declaring no
        // category of its own.
        elements.insert(
            100,
            ExportedElement {
                class_index: Some(13),
                created_phase_id: Some(3),
                level_id: Some(1),
                ..ExportedElement::default()
            },
        );
        // The type it stands on. Same class, same phase, same level - it is
        // the declared category that tells the two apart.
        elements.insert(
            101,
            ExportedElement {
                class_index: Some(13),
                created_phase_id: Some(3),
                level_id: Some(1),
                category: Some(-2_000_011),
                category_source: Some("declared"),
                ..ExportedElement::default()
            },
        );
        // A definition carrying a verified body. The geometry way in used to
        // admit it; the box it verifies against is in its own local frame.
        elements.insert(
            102,
            ExportedElement {
                class_index: Some(13),
                category: Some(-2_000_011),
                category_source: Some("declared"),
                verified_symbol_bounds: Some(VerifiedSymbolBounds {
                    symbol_element_id: 101,
                    bounds: rvt_model::GElementBounds {
                        offset: 0,
                        min: [0.0, 0.0, 0.0],
                        max: [1.0, 1.0, 1.0],
                    },
                    instance_bounds: rvt_model::GElementBounds {
                        offset: 0,
                        min: [0.0, 0.0, 0.0],
                        max: [1.0, 1.0, 1.0],
                    },
                }),
                ..ExportedElement::default()
            },
        );
        // An instance of a class the export does not know, admitted by the
        // category it inherited from its family. That way in stays open.
        elements.insert(
            103,
            ExportedElement {
                class_index: Some(12),
                created_phase_id: Some(3),
                level_id: Some(1),
                category: Some(-2_000_011),
                category_source: Some("family"),
                ..ExportedElement::default()
            },
        );

        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: Some(schema),
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        let (model, ..) = metadata_model(&recovered, false, None);
        let selected = model
            .elements
            .iter()
            .map(|element| element.id.0.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            selected,
            ["100".to_owned(), "103".to_owned()].into_iter().collect(),
        );
    }

    /// A parameter nothing names does not reach the model. AR S1 carries two
    /// such codes on every wall - Autodesk publishes neither, and Revit's own
    /// export writes neither - and the sheet a reader sees is the poorer for
    /// repeating the code back at them.
    #[test]
    fn a_parameter_nothing_names_is_left_out_of_the_model() {
        let schema = named_classes(&[(12, "Element", None), (13, "SWall", None)]);
        let mut elements = BTreeMap::new();
        elements.insert(
            100,
            ExportedElement {
                class_index: Some(13),
                created_phase_id: Some(3),
                level_id: Some(1),
                parameters: vec![
                    // WALL_BASE_OFFSET, which the catalogue names.
                    rvt_model::Parameter {
                        id: -1_001_108,
                        value: ParameterValue::Double(-0.328_083_989_501_312_35),
                    },
                    // Published nowhere, declared nowhere in the file.
                    rvt_model::Parameter {
                        id: -1_001_111,
                        value: ParameterValue::Double(-0.328_083_989_501_312_35),
                    },
                ],
                ..ExportedElement::default()
            },
        );
        let recovered = RecoveredElements {
            release: Some(2023),
            catalog: Catalog::for_release(2023),
            parameter_values_schema_bound: true,
            schema: Some(schema),
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        let (model, counts) = metadata_model(&recovered, false, None);
        let names = model.elements[0]
            .properties
            .iter()
            .map(|property| property.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"Base Offset"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.starts_with("param_")),
            "{names:?}"
        );
        assert_eq!(counts.included, 1);
        assert_eq!(counts.unnamed, 1);
        assert_eq!(counts.unverified, 0);
    }

    /// A storey nothing exported stands on is still a real storey of the
    /// project - "01 `ПромЭтаж`", "25 Кровля 5" and "25 Кровля 6" of AR S1,
    /// each a single unoccupied `Level` record Revit's own export writes
    /// anyway.
    /// Occupancy still wins where it disagrees with the file's history: AR
    /// S1's "02 Этаж" keeps two elevations among its records and only the
    /// occupied one is where Revit puts it too. A name split several ways
    /// with none of them occupied falls back to whichever elevation most of
    /// that name's own records agree on.
    #[test]
    fn a_storey_nothing_stands_on_is_kept_by_its_own_unoccupied_record() {
        let schema = named_classes(&[(12, "Level", None), (13, "SWall", None)]);
        let mut elements = BTreeMap::new();
        let level = |name: &str, elevation_feet: f64| ExportedElement {
            class_index: Some(12),
            name: Some((name.to_owned(), "declared")),
            elevation_feet: Some(elevation_feet),
            ..ExportedElement::default()
        };
        // Occupied storeys, one of them split into two elevations by the
        // file's own history - only the occupied one, 10 ft, is kept.
        elements.insert(10, level("01 Этаж", 0.0));
        elements.insert(11, level("02 Этаж", 10.0));
        elements.insert(12, level("02 Этаж", 20.0));
        // A storey with no occupant at all: a single record, kept anyway.
        elements.insert(13, level("01 ПромЭтаж", -50.0));
        // A name split three ways with no occupant anywhere: the elevation
        // two of its three records agree on wins.
        elements.insert(14, level("Attic", 30.0));
        elements.insert(15, level("Attic", 30.0));
        elements.insert(16, level("Attic", 40.0));
        // Excluded regardless of occupancy: a family document's own
        // reference level, and one marked deleted.
        elements.insert(
            17,
            ExportedElement {
                family_id: Some(999),
                ..level("Family Reference Level", 0.0)
            },
        );
        elements.insert(
            18,
            ExportedElement {
                moribund: true,
                ..level("Deleted Level", 0.0)
            },
        );
        // What stands on the two occupied storeys - nothing stands on 12, so
        // its 20 ft never surfaces.
        elements.insert(
            100,
            ExportedElement {
                class_index: Some(13),
                level_id: Some(10),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            101,
            ExportedElement {
                class_index: Some(13),
                level_id: Some(11),
                ..ExportedElement::default()
            },
        );

        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: Some(schema),
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        let is_model_element = |element: &ExportedElement| element.class_index == Some(13);
        let (levels, canonical) = building_storeys(&recovered, &is_model_element);

        let by_name = levels
            .iter()
            .map(|level| {
                (
                    level.name.clone(),
                    level.elevation.as_ref().map(|value| value.value),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_name.len(), 4, "{by_name:?}");
        let feet = revit_catalog::internal_feet_to_metres;
        assert_eq!(by_name[&Some("01 Этаж".to_owned())], feet(0.0));
        assert_eq!(
            by_name[&Some("02 Этаж".to_owned())],
            feet(10.0),
            "the occupied record's elevation wins over the unoccupied duplicate"
        );
        assert_eq!(
            by_name[&Some("01 ПромЭтаж".to_owned())],
            feet(-50.0),
            "an unoccupied storey with one record is still kept"
        );
        assert_eq!(
            by_name[&Some("Attic".to_owned())],
            feet(30.0),
            "the elevation most of an unoccupied name's own records agree on wins"
        );
        assert!(!by_name.contains_key(&Some("Family Reference Level".to_owned())));
        assert!(!by_name.contains_key(&Some("Deleted Level".to_owned())));

        // The minority duplicate of "02 Этаж" is folded to the same storey
        // as the occupied one, not left dangling or given its own entry.
        let canonical_02 = canonical.get(&BimElementId("11".to_owned())).cloned();
        assert_eq!(canonical.get(&BimElementId("12".to_owned())), None);
        assert_eq!(canonical_02, Some(BimElementId("11".to_owned())));
    }

    /// The project's `ProjectInfo` element names the project, its number and
    /// its address; `project_identity` reads exactly those five built-in
    /// parameters and none of an element's own.
    #[test]
    fn project_identity_reads_the_five_built_in_parameters_project_info_carries() {
        let schema = named_classes(&[(12, "ProjectInfo", None), (13, "SWall", None)]);
        let mut elements = BTreeMap::new();
        elements.insert(
            1365,
            ExportedElement {
                class_index: Some(12),
                parameters: vec![
                    rvt_model::Parameter {
                        id: -1_006_316,
                        value: ParameterValue::Text("SRG-DP-RP-DUIS-66702".to_owned()),
                    },
                    rvt_model::Parameter {
                        id: -1_006_317,
                        value: ParameterValue::Text("A descriptive project name".to_owned()),
                    },
                    rvt_model::Parameter {
                        id: -1_006_318,
                        value: ParameterValue::Text("221B Baker Street".to_owned()),
                    },
                    rvt_model::Parameter {
                        id: -1_006_320,
                        value: ParameterValue::Text("РП".to_owned()),
                    },
                    rvt_model::Parameter {
                        id: -1_019_006,
                        value: ParameterValue::Text("Section 1".to_owned()),
                    },
                    // Not one of the five: never mistaken for one of them.
                    rvt_model::Parameter {
                        id: -1_006_319,
                        value: ParameterValue::Text("Client Name".to_owned()),
                    },
                ],
                ..ExportedElement::default()
            },
        );
        // A wall of some other class: never mistaken for the project's own
        // record.
        elements.insert(
            2,
            ExportedElement {
                class_index: Some(13),
                ..ExportedElement::default()
            },
        );
        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: Some(schema),
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };

        let identity = project_identity(&recovered).expect("a ProjectInfo record");
        assert_eq!(identity.number.as_deref(), Some("SRG-DP-RP-DUIS-66702"));
        assert_eq!(identity.name.as_deref(), Some("A descriptive project name"));
        assert_eq!(identity.address.as_deref(), Some("221B Baker Street"));
        assert_eq!(identity.phase.as_deref(), Some("РП"));
        assert_eq!(identity.building_name.as_deref(), Some("Section 1"));
    }

    /// No `ProjectInfo` record at all - a file whose class schema does not
    /// name one, or one whose only record is moribund - names no project.
    #[test]
    fn project_identity_is_none_without_a_project_info_record() {
        let schema = named_classes(&[(12, "SWall", None)]);
        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: Some(schema),
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements: BTreeMap::new(),
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        assert!(project_identity(&recovered).is_none());
    }

    /// Every `GeoSite` record in the file states the same location, so
    /// `site_location` trusts it - and converts Revit's radians the same way
    /// `internal_feet_to_metres` already converts a level's elevation.
    /// Values are AR S1's own: Revit's default Boston siting, which
    /// `s1_revit.ifc` states as `IfcSite` `(42,24,53,508911)` degrees-minutes-
    /// seconds / `(-71,-15,-29,-58837)` / `0.` - `ifc-export`'s own test
    /// converts these same degrees into that compound form and checks it
    /// against that exact statement.
    #[test]
    fn site_location_agrees_when_every_geo_site_record_does() {
        let geo_site = rvt_model::GeoSiteFields {
            latitude_radians: 0.740_279_021_367_38,
            longitude_radians: -1.243_687_973_267_624_5,
            elevation_feet: 0.0,
        };
        let mut elements = BTreeMap::new();
        for (id, name) in [(1367, "52939_2004"), (7_031_652, "ОБРАЗЕЦ")] {
            elements.insert(
                id,
                ExportedElement {
                    geo_site: Some(geo_site),
                    name: Some((name.to_owned(), "declared")),
                    ..ExportedElement::default()
                },
            );
        }
        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: None,
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        let site = site_location(&recovered).expect("every record agrees");
        assert!((site.latitude_degrees - 42.414_863_586_425_76).abs() < 1e-9);
        assert!((site.longitude_degrees - -71.258_071_899_414_03).abs() < 1e-9);
        assert_eq!(site.elevation.map(|value| value.value), Some(0.0));
    }

    /// The two `GeoLocation` records AR S1 carries, and the tracking element
    /// that says which of them is in force. The values are that file's own:
    /// `Внутренний`'s frame, which `s1_revit.ifc`'s `IfcSite` placement is
    /// the inverse of.
    fn geo_location_elements() -> BTreeMap<u32, ExportedElement> {
        let frame = |basis, origin| {
            Some(GInstanceTransformFields {
                offset: 134,
                basis,
                origin: rvt_model::RvtPoint3 {
                    coordinates_feet: origin,
                },
                symbol_element_id: None,
            })
        };
        let mut elements = BTreeMap::new();
        elements.insert(
            1366,
            ExportedElement {
                active_geo_location: Some(rvt_model::ActiveGeoLocationFields {
                    active_id: 1368,
                    project_id: 1371,
                }),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            1368,
            ExportedElement {
                geo_location_placement: frame(
                    [
                        [0.025_794_452_427_383_475, 0.999_667_267_756_612_7, 0.0],
                        [-0.999_667_267_756_612_7, 0.025_794_452_427_383_475, 0.0],
                        [0.0, 0.0, 1.0],
                    ],
                    [
                        101.282_288_650_673_27,
                        53.516_806_742_184_244,
                        -0.902_230_971_128_607_2,
                    ],
                ),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            1371,
            ExportedElement {
                geo_location_placement: frame(
                    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    [0.0; 3],
                ),
                ..ExportedElement::default()
            },
        );
        elements
    }

    fn recovered_with(elements: BTreeMap<u32, ExportedElement>) -> RecoveredElements {
        RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: None,
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        }
    }

    #[test]
    fn site_placement_follows_the_location_the_file_declares_active() {
        let placement =
            site_placement(&recovered_with(geo_location_elements())).expect("an active location");
        for (actual, expected) in placement
            .origin
            .coordinates
            .into_iter()
            .zip([30.870_841_580_725_212, 16.311_922_695_017_76, -0.275])
        {
            assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
        }
        assert!((placement.reference_direction[1] - 0.999_667_267_756_612_7).abs() < 1e-12);
        for (actual, expected) in placement.axis.into_iter().zip([0.0, 0.0, 1.0]) {
            assert!((actual - expected).abs() < 1e-12);
        }
    }

    /// A project that was never placed states an identity location, and an
    /// identity placement says nothing an absent one does not.
    #[test]
    fn site_placement_refuses_an_identity_location() {
        let mut elements = geo_location_elements();
        elements.get_mut(&1366).unwrap().active_geo_location =
            Some(rvt_model::ActiveGeoLocationFields {
                active_id: 1371,
                project_id: 1371,
            });
        assert!(site_placement(&recovered_with(elements)).is_none());
    }

    #[test]
    fn site_placement_is_none_without_a_tracking_element() {
        let mut elements = geo_location_elements();
        elements.remove(&1366);
        assert!(site_placement(&recovered_with(elements)).is_none());
    }

    /// Two named locations whose coordinates genuinely differ - a project
    /// that really does carry more than one site - is refused rather than
    /// guessed at: there is nothing here yet that says which one is active.
    #[test]
    fn site_location_refuses_to_guess_between_disagreeing_records() {
        let mut elements = BTreeMap::new();
        elements.insert(
            1,
            ExportedElement {
                geo_site: Some(rvt_model::GeoSiteFields {
                    latitude_radians: 0.7,
                    longitude_radians: -1.2,
                    elevation_feet: 0.0,
                }),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            2,
            ExportedElement {
                geo_site: Some(rvt_model::GeoSiteFields {
                    latitude_radians: 0.9,
                    longitude_radians: -1.4,
                    elevation_feet: 0.0,
                }),
                ..ExportedElement::default()
            },
        );
        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: None,
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements,
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        assert!(site_location(&recovered).is_none());
    }

    /// No `GeoSite` record at all names no site - the common case, since most
    /// files never touch `Manage > Location`.
    #[test]
    fn site_location_is_none_without_a_geo_site_record() {
        let recovered = RecoveredElements {
            release: None,
            catalog: None,
            parameter_values_schema_bound: false,
            schema: None,
            partition_paths: Vec::new(),
            parameter_names: BTreeMap::new(),
            parameter_specs: BTreeMap::new(),
            elements: BTreeMap::new(),
            authored_unique_ids: BTreeMap::new(),
            document_identity: None,
        };
        assert!(site_location(&recovered).is_none());
    }

    /// An element wears the build-up its type declares, and says which record
    /// it came from; the type itself keeps its own without naming a source.
    #[test]
    fn an_element_reaches_the_layer_table_its_type_declares() {
        let layer = |width_feet: f64, material: i32| rvt_model::CompoundLayer {
            width_feet,
            function: 1,
            embedding_type: 0,
            material_id: Some(material),
            profile_id: None,
            layer_id: 0,
            cap: false,
        };
        let mut elements = BTreeMap::new();
        elements.insert(
            29_073,
            ExportedElement {
                name: Some(("(наружные)блок_т_t=200".to_owned(), "declared")),
                compound_structures: vec![rvt_model::CompoundStructure {
                    offset: 0,
                    layers: vec![layer(0.0, 4_340_120), layer(0.656_167_979_002_624_7, 2_959)],
                    coarse_scale_fill_pattern_id: None,
                    end_cap: 0,
                    opening_wrapping: 0,
                    shell_layers_exterior: 1,
                    shell_layers_interior: 0,
                    variable_layer_index: None,
                    structural_layer_index: Some(1),
                }],
                ..ExportedElement::default()
            },
        );
        elements.insert(
            2_959,
            ExportedElement {
                name: Some(("SP_кладка_блоки".to_owned(), "declared")),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            5_390_966,
            ExportedElement {
                type_element_id: Some(29_073),
                ..ExportedElement::default()
            },
        );

        let wall = normalize_material_layers(&elements[&5_390_966], &elements).unwrap();
        assert_eq!(
            wall.source_type_id,
            Some(BimElementId("29073".to_owned())),
            "the record the layers were read from is named"
        );
        assert_eq!(wall.name.as_deref(), Some("(наружные)блок_т_t=200"));
        assert_eq!(wall.layers.len(), 2);
        // 200 mm, in metres, from Revit's internal feet.
        let total = wall.total_thickness().unwrap();
        assert!((total.value - 0.2).abs() < 1.0e-12, "{}", total.value);
        assert_eq!(
            total.unit.as_ref().map(|unit| unit.id.as_str()),
            Some("autodesk.unit.unit:meters-1.0.0")
        );
        // The shell layer is outside the core; the structural one is in it.
        assert!(!wall.layers[0].is_core);
        assert!(wall.layers[1].is_core);
        assert!(wall.layers[1].is_structural);
        assert_eq!(
            wall.layers[1].material.as_ref().unwrap().name.as_deref(),
            Some("SP_кладка_блоки")
        );
        // A material this file does not hold keeps its identifier and has no name.
        let unnamed = wall.layers[0].material.as_ref().unwrap();
        assert_eq!(unnamed.name, None);
        assert_eq!(unnamed.id.as_ref().unwrap().value, "4340120");

        let declaring = normalize_material_layers(&elements[&29_073], &elements).unwrap();
        assert_eq!(declaring.source_type_id, None);
    }

    #[test]
    fn a_type_s_parameters_reach_its_instances_without_joining_their_own() {
        let parameter = |id: i32, value: &str| rvt_model::Parameter {
            id,
            value: ParameterValue::Text(value.to_owned()),
        };
        let mut elements = BTreeMap::new();
        elements.insert(
            5,
            ExportedElement {
                parameters: vec![
                    parameter(214_890, "SANEXT"),
                    parameter(214_891, "\u{448}\u{442}."),
                ],
                ..ExportedElement::default()
            },
        );
        // One instance reaches its type through the declared reference, one
        // through bounds, and one names a type that stores nothing.
        elements.insert(
            9,
            ExportedElement {
                type_element_id: Some(5),
                parameters: vec![parameter(214_891, "\u{43c}")],
                ..ExportedElement::default()
            },
        );
        elements.insert(
            11,
            ExportedElement {
                verified_symbol_bounds: Some(VerifiedSymbolBounds {
                    symbol_element_id: 5,
                    bounds: GElementBounds {
                        offset: 0,
                        min: [0.0; 3],
                        max: [1.0; 3],
                    },
                    instance_bounds: GElementBounds {
                        offset: 0,
                        min: [0.0; 3],
                        max: [1.0; 3],
                    },
                }),
                ..ExportedElement::default()
            },
        );
        elements.insert(
            13,
            ExportedElement {
                type_element_id: Some(7),
                ..ExportedElement::default()
            },
        );
        inherit_symbol_parameters(&mut elements);

        // The instance sets 214891 itself, so only the type's other value is
        // inherited, and the element's own list is untouched either way.
        assert_eq!(
            elements[&9].type_parameters,
            vec![parameter(214_890, "SANEXT")]
        );
        assert_eq!(elements[&9].parameters, vec![parameter(214_891, "\u{43c}")]);
        assert_eq!(elements[&11].type_parameters.len(), 2);
        assert!(elements[&13].type_parameters.is_empty());
        // A type carries none of its own instances' values.
        assert!(elements[&5].type_parameters.is_empty());
    }

    #[test]
    fn a_declared_type_reference_is_preferred_over_a_bounds_verified_one() {
        let bounds = VerifiedSymbolBounds {
            symbol_element_id: 5,
            bounds: GElementBounds {
                offset: 0,
                min: [0.0; 3],
                max: [1.0; 3],
            },
            instance_bounds: GElementBounds {
                offset: 0,
                min: [0.0; 3],
                max: [1.0; 3],
            },
        };
        let element = |declared: Option<i32>| ExportedElement {
            type_element_id: declared,
            verified_symbol_bounds: Some(bounds),
            ..ExportedElement::default()
        };
        assert_eq!(element(Some(9)).type_element_reference(), Some(9));
        assert_eq!(element(None).type_element_reference(), Some(5));
        // A negative identifier is not an element, and must not wrap round.
        assert_eq!(element(Some(-2)).type_element_reference(), Some(5));
        assert_eq!(ExportedElement::default().type_element_reference(), None);
    }

    /// A duplicate record from a later partition overwrites the type
    /// reference an earlier one already set - real AR S1 pairs where the
    /// earlier partition's copy names a door 100 mm narrower than the one
    /// `s1_revit.ifc` agrees with.
    #[test]
    fn a_later_records_type_reference_overwrites_an_earlier_ones() {
        let declared_naming = |property: &str, id: i32| {
            let mut values = vec![None; TYPE_ELEMENT_ID_PROPERTIES.len()];
            let at = TYPE_ELEMENT_ID_PROPERTIES
                .iter()
                .position(|candidate| *candidate == property)
                .unwrap();
            values[at] = Some(id);
            values
        };
        let mut entry = ExportedElement::default();
        fold_type_element_id(&mut entry, &declared_naming("m_masterSymbolId", 5_110_107));
        assert_eq!(entry.type_element_id, Some(5_110_107));

        fold_type_element_id(&mut entry, &declared_naming("m_masterSymbolId", 8_364_806));
        assert_eq!(
            entry.type_element_id,
            Some(8_364_806),
            "the later record should win, not the first"
        );

        // A later record naming nothing leaves the last real answer alone -
        // this only overwrites when it has something to overwrite with.
        fold_type_element_id(&mut entry, &vec![None; TYPE_ELEMENT_ID_PROPERTIES.len()]);
        assert_eq!(entry.type_element_id, Some(8_364_806));
    }

    #[test]
    fn promotes_only_verified_mapped_symbol_bounds_as_a_box() {
        let element = ExportedElement {
            verified_symbol_bounds: Some(VerifiedSymbolBounds {
                symbol_element_id: 417_391,
                bounds: GElementBounds {
                    offset: 54,
                    min: [-1.0, -2.0, -3.0],
                    max: [1.0, 2.0, 3.0],
                },
                // The same box where the instance stands, ten feet along each
                // axis: what is promoted is the placed extent, because the
                // writer reads every geometry in project coordinates.
                instance_bounds: GElementBounds {
                    offset: 12,
                    min: [9.0, 18.0, 27.0],
                    max: [11.0, 22.0, 33.0],
                },
            }),
            ..ExportedElement::default()
        };
        let Some(BimGeometry::BoundingBox(bounds)) = normalize_geometry(
            &element,
            BimElementType::SanitaryTerminal,
            &BTreeMap::new(),
            &|_| false,
        ) else {
            panic!("verified symbol bounds were not promoted");
        };
        for (actual, expected) in bounds
            .min
            .coordinates
            .into_iter()
            .chain(bounds.max.coordinates)
            .zip([2.7432, 5.4864, 8.2296, 3.3528, 6.7056, 10.0584])
        {
            assert!((actual - expected).abs() < 1.0e-12);
        }
        // A pipe segment is a swept curve, not a placed symbol, so it takes no
        // symbol extent. An unclassified source does: whether the exporter can
        // name the kind of element something is has nothing to do with whether
        // its geometry was verified, and refusing `Unknown` discarded the body
        // of most of the corpus. See `carries_family_symbol_geometry`.
        assert!(
            normalize_geometry(
                &element,
                BimElementType::PipeSegment,
                &BTreeMap::new(),
                &|_| false
            )
            .is_none()
        );
        for carried in [BimElementType::Unknown, BimElementType::DistributionElement] {
            assert!(matches!(
                normalize_geometry(&element, carried, &BTreeMap::new(), &|_| false),
                Some(BimGeometry::BoundingBox(_))
            ));
        }
    }

    /// A square planar face one foot on a side, placed at `origin`.
    fn square_body(origin: [f64; 3]) -> rvt_model::SymbolBrep {
        let corner = |x: f64, y: f64| [origin[0] + x, origin[1] + y, origin[2]];
        let edge = |from: [f64; 3], to: [f64; 3]| rvt_model::BrepEdge {
            start: from,
            end: to,
            curve: rvt_model::BrepCurve::Line,
        };
        rvt_model::SymbolBrep {
            faces: vec![rvt_model::BrepFace {
                surface: rvt_model::BrepSurface::Plane {
                    origin,
                    x_axis: [1.0, 0.0, 0.0],
                    y_axis: [0.0, 1.0, 0.0],
                },
                loops: vec![vec![
                    edge(corner(0.0, 0.0), corner(1.0, 0.0)),
                    edge(corner(1.0, 0.0), corner(1.0, 1.0)),
                    edge(corner(1.0, 1.0), corner(0.0, 1.0)),
                    edge(corner(0.0, 1.0), corner(0.0, 0.0)),
                ]],
                material_id: None,
            }],
            ..rvt_model::SymbolBrep::default()
        }
    }

    /// A closed unit cube's six faces, corner at `origin`. A real
    /// `BrepBody`'s `edges`/`one_sided_edges`/`open_edges` come from the
    /// source record's own `GEdge.m_pFace` bookkeeping, which this synthetic
    /// fixture has none of to read - a cube has twelve edges and closes, so
    /// that is what is asserted directly.
    fn closed_box_faces(origin: [f64; 3]) -> Vec<rvt_model::BrepFace> {
        let point = |dx: f64, dy: f64, dz: f64| [origin[0] + dx, origin[1] + dy, origin[2] + dz];
        let edge = |from: [f64; 3], to: [f64; 3]| rvt_model::BrepEdge {
            start: from,
            end: to,
            curve: rvt_model::BrepCurve::Line,
        };
        let quad = |corners: [[f64; 3]; 4]| rvt_model::BrepFace {
            surface: rvt_model::BrepSurface::Plane {
                origin: corners[0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: vec![vec![
                edge(corners[0], corners[1]),
                edge(corners[1], corners[2]),
                edge(corners[2], corners[3]),
                edge(corners[3], corners[0]),
            ]],
            material_id: None,
        };
        vec![
            quad([
                point(0.0, 0.0, 0.0),
                point(1.0, 0.0, 0.0),
                point(1.0, 1.0, 0.0),
                point(0.0, 1.0, 0.0),
            ]),
            quad([
                point(0.0, 0.0, 1.0),
                point(0.0, 1.0, 1.0),
                point(1.0, 1.0, 1.0),
                point(1.0, 0.0, 1.0),
            ]),
            quad([
                point(0.0, 0.0, 0.0),
                point(0.0, 1.0, 0.0),
                point(0.0, 1.0, 1.0),
                point(0.0, 0.0, 1.0),
            ]),
            quad([
                point(1.0, 0.0, 0.0),
                point(1.0, 0.0, 1.0),
                point(1.0, 1.0, 1.0),
                point(1.0, 1.0, 0.0),
            ]),
            quad([
                point(0.0, 0.0, 0.0),
                point(0.0, 0.0, 1.0),
                point(1.0, 0.0, 1.0),
                point(1.0, 0.0, 0.0),
            ]),
            quad([
                point(0.0, 1.0, 0.0),
                point(1.0, 1.0, 0.0),
                point(1.0, 1.0, 1.0),
                point(0.0, 1.0, 1.0),
            ]),
        ]
    }

    /// A door's frame and its panel are two closed solids under one
    /// `FamilySymbol` - not one solid, and not a hint to merge them into one.
    /// Placed touching, the way a panel sits inside its frame: the merged,
    /// whole-record reading of `SymbolBrep::bounds_a_volume` sees a shared
    /// edge at their common face used four times, not two, and reads the
    /// record as an open `Gap` rather than a closed volume - the exact
    /// failure `report_symbol_link_funnel` measured on AR S1's doors, 325 of
    /// 424 real instances reduced to a bounding box. `requires_loop_closure`
    /// is set so the merged reading cannot pass some other way and mask
    /// whether the split path was actually what supplied the geometry.
    #[test]
    fn a_symbol_declaring_two_touching_closed_bodies_becomes_an_assembly() {
        let frame = closed_box_faces([0.0, 0.0, 0.0]);
        let panel = closed_box_faces([1.0, 0.0, 0.0]);
        let combined = rvt_model::SymbolBrep {
            faces: frame.into_iter().chain(panel).collect(),
            bodies: vec![
                rvt_model::BrepBody {
                    node_id: 0,
                    faces: (0..6).collect(),
                    edges: 12,
                    one_sided_edges: 0,
                    open_edges: 0,
                },
                rvt_model::BrepBody {
                    node_id: 1,
                    faces: (6..12).collect(),
                    edges: 12,
                    one_sided_edges: 0,
                    open_edges: 0,
                },
            ],
            ..rvt_model::SymbolBrep::default()
        };
        let symbol = ExportedElement {
            brep: Some(combined),
            brep_requires_loop_closure: true,
            ..ExportedElement::default()
        };

        // The merged, whole-record reading really does fail - otherwise this
        // test would pass regardless of whether the split path exists.
        assert!(
            !normalize_element_brep(&symbol, &IDENTITY_TRANSFORM).is_some_and(|brep| brep.complete),
            "the merged reading of two touching bodies should not itself be complete"
        );

        let geometry = normalize_symbol_geometry(&symbol, &IDENTITY_TRANSFORM)
            .expect("two independently closed bodies should assemble");
        let BimGeometry::Assembly(parts) = geometry else {
            panic!("expected an assembly, got something else");
        };
        assert_eq!(parts.len(), 2);
        for part in &parts {
            assert!(part.complete, "{part:?}");
            assert_eq!(part.faces.len(), 6);
        }
    }

    /// One closed body and one open, one-sided sheet beside it - a
    /// construction surface, say - is not two bodies to assemble: an
    /// assembly of one part is not an assembly reading at all, so this falls
    /// through to whatever the merged or box fallback gives instead.
    #[test]
    fn a_single_closed_body_beside_an_open_one_is_not_an_assembly() {
        let closed = closed_box_faces([0.0, 0.0, 0.0]);
        let open = vec![rvt_model::BrepFace {
            surface: rvt_model::BrepSurface::Plane {
                origin: [5.0, 0.0, 0.0],
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: vec![vec![rvt_model::BrepEdge {
                start: [5.0, 0.0, 0.0],
                end: [6.0, 0.0, 0.0],
                curve: rvt_model::BrepCurve::Line,
            }]],
            material_id: None,
        }];
        let combined = rvt_model::SymbolBrep {
            faces: closed.into_iter().chain(open).collect(),
            bodies: vec![
                rvt_model::BrepBody {
                    node_id: 0,
                    faces: (0..6).collect(),
                    edges: 12,
                    one_sided_edges: 0,
                    open_edges: 0,
                },
                rvt_model::BrepBody {
                    node_id: 1,
                    faces: vec![6],
                    edges: 1,
                    one_sided_edges: 1,
                    open_edges: 0,
                },
            ],
            ..rvt_model::SymbolBrep::default()
        };
        let symbol = ExportedElement {
            brep: Some(combined),
            brep_requires_loop_closure: true,
            ..ExportedElement::default()
        };
        assert!(normalize_symbol_geometry(&symbol, &IDENTITY_TRANSFORM).is_none());
    }

    /// A flat plan symbol's front and back faces, drawn at the same
    /// elevation and sharing every edge, close under every topological
    /// test `SymbolBrep` has - `is_closed`, and `bounds_a_volume`'s own
    /// endpoint pairing, since each edge really is drawn by exactly two
    /// faces - while enclosing no volume at all. Measured on AR S1: this is
    /// exactly the shape of a "(`оборудование_2D`)" plan symbol, and splitting a
    /// symbol's several bodies (see `normalize_symbol_geometry`) turned every
    /// one of them into a "closed" body before `spans_a_volume` existed to
    /// refuse it - 339 `IfcSanitaryTerminal` on AR S1 Revit's own export
    /// carries none of.
    #[test]
    fn a_flat_two_sided_sandwich_does_not_span_a_volume() {
        let front = square_body([0.0, 0.0, 0.0]);
        let mut back = square_body([0.0, 0.0, 0.0]);
        // The same square, wound the other way - a real back face of a
        // zero-thickness sheet, not a second copy of the front.
        for face in &mut back.faces {
            face.loops[0].reverse();
            for edge in &mut face.loops[0] {
                std::mem::swap(&mut edge.start, &mut edge.end);
            }
        }
        let sandwich = rvt_model::SymbolBrep {
            faces: front.faces.into_iter().chain(back.faces).collect(),
            bodies: vec![rvt_model::BrepBody {
                node_id: 0,
                faces: vec![0, 1],
                edges: 8,
                one_sided_edges: 0,
                open_edges: 0,
            }],
            ..rvt_model::SymbolBrep::default()
        };
        let normalized = normalize_brep(&sandwich, &IDENTITY_TRANSFORM, false)
            .expect("a readable, if degenerate, body");
        assert!(
            !normalized.complete,
            "a zero-thickness sandwich should not be accepted as a closed solid"
        );
    }

    #[test]
    fn tallies_a_body_against_the_class_that_owns_it() {
        // A wall carrying its own placed body, a symbol carrying a local one,
        // and the instance that names the symbol. The wall is what the export
        // never asks for and the symbol is the only path it does ask through,
        // so the two have to be told apart by owner.
        let mut elements: BTreeMap<u32, ExportedElement> = BTreeMap::new();
        let wall = elements.entry(1).or_default();
        wall.class_index = Some(10);
        wall.created_phase_id = Some(3);
        wall.brep = Some(square_body([40.0, 5.0, 0.0]));
        wall.brep_records = 2;
        // What the recovery sets when the body reproduces the bounds of the
        // record it came from; see `body_is_placed_in`.
        wall.brep_is_placed = true;
        wall.brep_requires_loop_closure = true;
        wall.brep_box_residuals = BodyBoxResiduals {
            exact: Some(0.0),
            graph: Some(0.0),
            ..BodyBoxResiduals::default()
        };
        wall.geometry_bounds = Some(rvt_model::GElementBounds {
            offset: 0,
            min: [40.0, 5.0, 0.0],
            max: [41.0, 6.0, 0.0],
        });
        let symbol = elements.entry(2).or_default();
        symbol.class_index = Some(20);
        symbol.brep = Some(square_body([0.0, 0.0, 0.0]));
        symbol.brep_records = 1;
        let instance = elements.entry(3).or_default();
        instance.class_index = Some(30);
        instance.created_phase_id = Some(3);
        instance.verified_symbol_bounds = Some(VerifiedSymbolBounds {
            symbol_element_id: 2,
            bounds: rvt_model::GElementBounds {
                offset: 0,
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 0.0],
            },
            instance_bounds: rvt_model::GElementBounds {
                offset: 0,
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 0.0],
            },
        });

        let rows = tally_class_geometry(&elements, |element| match element.class_index {
            Some(10) => Some("SWall"),
            Some(20) => Some("FamilySymbol"),
            Some(30) => Some("FamilyInstance"),
            _ => None,
        });

        let wall = &rows["SWall"];
        assert_eq!(wall.body_ids, 1);
        // Both of the id's body-bearing records are counted, though only the
        // last is kept.
        assert_eq!(wall.body_records, 2);
        // The body reproduces the record's own bounds and sits where the
        // building is, not at a symbol's origin.
        assert_eq!(wall.bodies_matching_their_bounds, 1);
        // Its record carried an exact block, so the graph box adds nothing:
        // that column counts only what a second tier would newly place.
        assert_eq!(wall.bodies_placed_only_by_graph_bounds, 0);
        assert_eq!(wall.bodies_away_from_the_origin, 1);
        // It is a model element that owns a body and reaches the export by no
        // symbol at all - the case the funnel cannot see.
        assert_eq!(wall.model_elements, 1);
        assert_eq!(wall.model_elements_with_their_own_body, 1);
        assert_eq!(wall.model_elements_with_a_verified_symbol_body, 0);

        let symbol = &rows["FamilySymbol"];
        assert_eq!(symbol.body_ids, 1);
        assert_eq!(symbol.verified_by_an_instance, 1);
        assert_eq!(symbol.bodies_away_from_the_origin, 0);
        // A symbol is a type definition, never a model element.
        assert_eq!(symbol.model_elements, 0);

        let instance = &rows["FamilyInstance"];
        assert_eq!(instance.body_ids, 0);
        assert_eq!(instance.model_elements_with_a_verified_symbol_body, 1);

        assert_eq!(
            geometry_statistics(&elements, None).bodies_requiring_loop_closure,
            1
        );
    }

    fn bounds(min: [f64; 3], max: [f64; 3]) -> rvt_model::GElementBounds {
        rvt_model::GElementBounds {
            offset: 0,
            min,
            max,
        }
    }

    #[test]
    fn a_body_is_judged_on_the_box_its_record_declares() {
        let exact = bounds([40.0, 5.0, 0.0], [41.0, 6.0, 9.0]);
        let graph = bounds([0.0, 0.0, 0.0], [1.0, 1.0, 9.0]);
        // `GRep.m_bBox`, read at its declared offset, is the record's box.
        // The duplicated-block scan is a search for that same field, and on a
        // record whose two boxes differ it can land on a different pair of
        // blocks entirely, so it is what gives way when the two disagree.
        assert_eq!(body_placement_box(Some(exact), Some(graph)), Some(graph));
        assert_eq!(body_placement_box(Some(exact), None), Some(exact));
        // A record whose node array could not be read still has the scan.
        assert_eq!(body_placement_box(None, Some(graph)), Some(graph));
        assert_eq!(body_placement_box(None, None), None);
    }

    #[test]
    fn a_body_is_placed_only_where_it_reproduces_its_own_records_box() {
        let body = square_body([40.0, 5.0, 0.0]);
        // A flat square is a region, not a solid: its box holds no volume.
        assert!(!body_is_placed_in(
            &body,
            &bounds([40.0, 5.0, 0.0], [41.0, 6.0, 0.0])
        ));

        // The same face as one side of a volumetric box does not reproduce it.
        assert!(!body_is_placed_in(
            &body,
            &bounds([40.0, 5.0, 0.0], [41.0, 6.0, 9.0])
        ));

        let mut box_body = square_body([40.0, 5.0, 0.0]);
        let mut lid = square_body([40.0, 5.0, 9.0]);
        box_body.faces.append(&mut lid.faces);
        assert!(body_is_placed_in(
            &box_body,
            &bounds([40.0, 5.0, 0.0], [41.0, 6.0, 9.0])
        ));
        // A box the body does not reach is a different frame, not this one.
        assert!(!body_is_placed_in(
            &box_body,
            &bounds([0.0, 0.0, 0.0], [1.0, 1.0, 9.0])
        ));

        // An arc bulges past the endpoints the extent is taken from, and the
        // bulge is part of the body. This one runs from (41, 5) to (40, 5)
        // the long way round, reaching y = 4.5 half a foot outside the box
        // its endpoints describe.
        let mut curved = box_body.clone();
        curved.faces[0].loops[0][0].curve = rvt_model::BrepCurve::Arc(rvt_model::BrepArc {
            center: [40.5, 5.0, 0.0],
            x_axis: [1.0, 0.0, 0.0],
            // Handed so that the sweep from 0 to pi runs through -Y.
            z_axis: [0.0, 0.0, -1.0],
            radius: 0.5,
            start_angle: 0.0,
            end_angle: std::f64::consts::PI,
        });
        // The box that ignores the bulge is no longer this body's box.
        assert!(!body_is_placed_in(
            &curved,
            &bounds([40.0, 5.0, 0.0], [41.0, 6.0, 9.0])
        ));
        // The one that accounts for it is.
        assert!(body_is_placed_in(
            &curved,
            &bounds([40.0, 4.5, 0.0], [41.0, 6.0, 9.0])
        ));
        // An arc that does not sweep through the extreme contributes only its
        // endpoints: a quarter turn from (41, 5) reaches y = 5 - 0.5*sin, not
        // the full radius, so the box stays the one the endpoints give.
        let mut quarter = box_body.clone();
        quarter.faces[0].loops[0][0].curve = rvt_model::BrepCurve::Arc(rvt_model::BrepArc {
            center: [40.5, 5.0, 0.0],
            x_axis: [1.0, 0.0, 0.0],
            z_axis: [0.0, 0.0, -1.0],
            radius: 0.5,
            start_angle: 0.0,
            end_angle: std::f64::consts::FRAC_PI_2,
        });
        assert!(body_is_placed_in(
            &quarter,
            &bounds([40.0, 4.5, 0.0], [41.0, 6.0, 9.0])
        ));
    }

    /// Schema class indices for the fixtures below. Arbitrary: the module
    /// takes them from the caller and never looks a class up by name.
    const FIXTURE_FACE: u16 = 700;
    const FIXTURE_EDGE_LOOP: u16 = 701;
    const FIXTURE_EDGE: u16 = 702;
    const FIXTURE_PLANE: u16 = 703;
    const FIXTURE_GBREP: u16 = 704;

    fn fixture_classes() -> rvt_model::BrepClassIndexes {
        rvt_model::BrepClassIndexes {
            face: FIXTURE_FACE,
            edge_loop: FIXTURE_EDGE_LOOP,
            // No fixture here writes the derived loop class; `rvt-model`'s own
            // tests cover it.
            edge_loop_with_chain_envelopes: None,
            edge: FIXTURE_EDGE,
            plane: FIXTURE_PLANE,
            // Nothing in these fixtures is curved, and a class index no
            // object carries resolves nothing.
            cyl_surf: 800,
            cone_surf: 801,
            surf_rev: 802,
            ruled_surf: 803,
            g_line: 804,
            g_arc: 805,
        }
    }

    fn fixture_object(object_id: u32, class_index: u16) -> rvt_model::SerialObject {
        rvt_model::SerialObject {
            object_id,
            class_index,
            offset: 0,
            bytes: 0,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers: Vec::new(),
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// One face of a fixture: the plane it lies in, the corners its boundary
    /// runs through in order, whether it declares a loop object, and whether
    /// it carries [`FACE_INSIDE_THE_BOX_FLAG`].
    ///
    /// `joins` names the face on the other side of each of its edges, in the
    /// same order as `corners`, for the edges no other face of the fixture
    /// traverses. That is how a face lands *inside* a shell: its edges name
    /// the faces they cut across, and those faces' own loops never use them.
    struct FixtureFace {
        face: u32,
        plane: u32,
        origin: [f64; 3],
        x_axis: [f64; 3],
        y_axis: [f64; 3],
        corners: Vec<[f64; 3]>,
        joins: Vec<u32>,
        declares_a_loop: bool,
        inside_the_box: bool,
    }

    /// One edge of a fixture: where it runs, and for each side the face that
    /// names it and whether that face's own boundary traverses it.
    struct FixtureEdge {
        from: [f64; 3],
        to: [f64; 3],
        sides: Vec<(usize, bool)>,
    }

    /// The edges these faces run between their corners, and for each face the
    /// edge its every step reaches.
    ///
    /// Corners are matched bit for bit, which is exact here because a fixture
    /// states the same corner to both faces that reach it.
    fn fixture_edges(faces: &[FixtureFace]) -> (Vec<FixtureEdge>, Vec<Vec<usize>>) {
        let key = |point: [f64; 3]| point.map(f64::to_bits);
        let index_of = |wanted: u32| {
            faces
                .iter()
                .position(|face| face.face == wanted)
                .expect("the joined face is in the fixture")
        };
        let mut edges: Vec<FixtureEdge> = Vec::new();
        let mut by_ends: BTreeMap<([u64; 3], [u64; 3]), usize> = BTreeMap::new();
        let mut steps: Vec<Vec<usize>> = Vec::new();
        for (at, face) in faces.iter().enumerate() {
            let mut face_steps = Vec::new();
            for (index, from) in face.corners.iter().enumerate() {
                let to = face.corners[(index + 1) % face.corners.len()];
                let (first, last) = (key(*from), key(to));
                let ends = if first <= last {
                    (first, last)
                } else {
                    (last, first)
                };
                let fresh = !by_ends.contains_key(&ends);
                let edge = *by_ends.entry(ends).or_insert_with(|| {
                    edges.push(FixtureEdge {
                        from: *from,
                        to,
                        sides: Vec::new(),
                    });
                    edges.len() - 1
                });
                edges[edge].sides.push((at, true));
                if fresh {
                    if let Some(other) = face.joins.get(index) {
                        edges[edge].sides.push((index_of(*other), false));
                    }
                }
                assert!(edges[edge].sides.len() < 3, "three faces reached one edge");
                face_steps.push(edge);
            }
            steps.push(face_steps);
        }
        (edges, steps)
    }

    /// Assemble the `SerialObject`s a record would carry for these faces: a
    /// `Plane` and a `Face` each, one `EdgeLoop` per face that declares one,
    /// one `Edge` for each pair of corners two faces run between - with the
    /// `(u, v)` each of the two faces states it at - and one `GBRep` node
    /// naming every face.
    ///
    /// Every `Edge` is written in the traversal order of the first face that
    /// reached it, with `m_flags` clear, so the *other* face reads it
    /// reversed: that is how two faces sharing an edge traverse it, and it is
    /// what lets these fixtures be written as corner lists.
    fn fixture_record(faces: &[FixtureFace]) -> Vec<rvt_model::SerialObject> {
        let (edges, steps) = fixture_edges(faces);
        let mut objects = fixture_surface_objects(faces, &steps);
        objects.extend(fixture_edge_objects(faces, &edges, &steps));
        let mut node = fixture_object(500, FIXTURE_GBREP);
        node.references = faces
            .iter()
            .map(|face| rvt_model::GElementNodeReference {
                object_id: face.face,
                class_index: FIXTURE_FACE,
            })
            .collect();
        objects.push(node);
        objects
    }

    fn fixture_edge_id(edge: usize) -> u32 {
        1000 + u32::try_from(edge).unwrap()
    }

    fn fixture_loop_id(face: &FixtureFace) -> u32 {
        face.face + 100
    }

    /// A `Plane`, a `Face` and - where the face declares one - an `EdgeLoop`
    /// for each of these faces.
    fn fixture_surface_objects(
        faces: &[FixtureFace],
        steps: &[Vec<usize>],
    ) -> Vec<rvt_model::SerialObject> {
        let mut objects = Vec::new();
        for face in faces {
            let mut plane = fixture_object(face.plane, FIXTURE_PLANE);
            plane.numbers = vec![0.0; 4];
            plane.numbers.extend(face.origin);
            plane.numbers.extend(face.x_axis);
            plane.numbers.extend(face.y_axis);
            objects.push(plane);

            let mut object = fixture_object(face.face, FIXTURE_FACE);
            object.references = vec![
                rvt_model::GElementNodeReference {
                    object_id: if face.declares_a_loop {
                        fixture_loop_id(face)
                    } else {
                        0
                    },
                    class_index: FIXTURE_EDGE_LOOP,
                },
                rvt_model::GElementNodeReference {
                    object_id: face.plane,
                    class_index: FIXTURE_PLANE,
                },
            ];
            // `GFaceMarks::read` wants every field: m_tag, m_controlCommand,
            // m_categoryId, m_cutType and m_renderStyleId, then the two flag
            // words.
            object.integers = vec![i32::try_from(face.face).unwrap(), 0, -1, 0, -1];
            object.alternate_integers = vec![
                if face.inside_the_box {
                    FACE_INSIDE_THE_BOX_FLAG
                } else {
                    0
                },
                0x4,
            ];
            objects.push(object);
        }
        for (at, face) in faces.iter().enumerate() {
            if !face.declares_a_loop {
                continue;
            }
            let mut object = fixture_object(fixture_loop_id(face), FIXTURE_EDGE_LOOP);
            object.references = vec![rvt_model::GElementNodeReference {
                object_id: 0,
                class_index: FIXTURE_EDGE_LOOP,
            }];
            object.identifiers = vec![
                face.face,
                fixture_edge_id(steps[at][0]),
                fixture_edge_id(steps[at][steps[at].len() - 1]),
            ];
            objects.push(object);
        }
        objects
    }

    /// One `Edge` per edge, carrying the two faces that name it, the next
    /// link each of them reads, and the `(u, v)` each states its endpoints at.
    fn fixture_edge_objects(
        faces: &[FixtureFace],
        edges: &[FixtureEdge],
        steps: &[Vec<usize>],
    ) -> Vec<rvt_model::SerialObject> {
        let mut objects = Vec::new();
        for (at, edge) in edges.iter().enumerate() {
            let mut object = fixture_object(fixture_edge_id(at), FIXTURE_EDGE);
            let face_of = |side: usize| {
                edge.sides
                    .get(side)
                    .map_or(0, |(face, _)| faces[*face].face)
            };
            // `identifiers` is [pFace0, pFace1, next0, next1, prev0, prev1].
            // Only the next links are read, and only for a face that declares
            // a loop and traverses this edge; the loop's own id ends the ring.
            let next = |side: usize| {
                let Some(&(face, traverses)) = edge.sides.get(side) else {
                    return 0;
                };
                if !traverses || !faces[face].declares_a_loop {
                    return 0;
                }
                let index = steps[face]
                    .iter()
                    .position(|step| *step == at)
                    .expect("the face reached this edge");
                if index + 1 == steps[face].len() {
                    fixture_loop_id(&faces[face])
                } else {
                    fixture_edge_id(steps[face][index + 1])
                }
            };
            object.identifiers = vec![face_of(0), face_of(1), next(0), next(1), 0, 0];
            object.small_integers = vec![0];
            let uv = |point: [f64; 3], side: usize| -> [f64; 2] {
                let Some((face, _)) = edge.sides.get(side) else {
                    return [0.0, 0.0];
                };
                let face = &faces[*face];
                let local = [
                    point[0] - face.origin[0],
                    point[1] - face.origin[1],
                    point[2] - face.origin[2],
                ];
                let dot =
                    |axis: [f64; 3]| local[0] * axis[0] + local[1] * axis[1] + local[2] * axis[2];
                [dot(face.x_axis), dot(face.y_axis)]
            };
            // An `EdgePnt` pair per endpoint, each face's own first: the
            // layout `resolve_edge` reads off the end of the numbers.
            for point in [edge.from, edge.to] {
                let (first, second) = (uv(point, 0), uv(point, 1));
                object
                    .numbers
                    .extend([first[0], first[1], second[0], second[1]]);
            }
            objects.push(object);
        }
        objects
    }

    /// The extent of the fixture box: 1 x 1 x 2, from the origin.
    const FIXTURE_BOX: [f64; 3] = [1.0, 1.0, 2.0];

    /// The six faces of [`FIXTURE_BOX`], each declaring a loop and each wound
    /// so that its normal points out of the box - which is what lets two
    /// faces share an edge, since they have to traverse it in opposite
    /// directions.
    ///
    /// Face ids run 1..=6 in the order the axes come: 1 and 2 are x = 0 and
    /// x = 1, 3 and 4 are y = 0 and y = 1, 5 and 6 are z = 0 and z = 2.
    fn fixture_box_faces() -> Vec<FixtureFace> {
        let mut faces = Vec::new();
        for axis in 0..3 {
            for far in [false, true] {
                let (first, second) = ((axis + 1) % 3, (axis + 2) % 3);
                // The near face's normal is -axis, so its two in-plane axes
                // come the other way round.
                let (first, second) = if far {
                    (first, second)
                } else {
                    (second, first)
                };
                let mut origin = [0.0; 3];
                if far {
                    origin[axis] = FIXTURE_BOX[axis];
                }
                let mut x_axis = [0.0; 3];
                x_axis[first] = 1.0;
                let mut y_axis = [0.0; 3];
                y_axis[second] = 1.0;
                let corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
                    .into_iter()
                    .map(|uv| {
                        let mut corner = origin;
                        corner[first] += uv[0] * FIXTURE_BOX[first];
                        corner[second] += uv[1] * FIXTURE_BOX[second];
                        corner
                    })
                    .collect();
                let face = u32::try_from(axis * 2).unwrap() + u32::from(far) + 1;
                faces.push(FixtureFace {
                    face,
                    plane: face + 50,
                    origin,
                    x_axis,
                    y_axis,
                    corners,
                    joins: Vec::new(),
                    declares_a_loop: true,
                    inside_the_box: true,
                });
            }
        }
        faces
    }

    /// [`fixture_box_faces`] plus one face spanning the box's footprint
    /// halfway up - the shape of wall 5572231, whose seventh face is the top
    /// of the layers its compound structure separates on.
    ///
    /// The interior face declares no loop, as that wall's does not, so it is
    /// ordered from the edges that name it; the six faces of the box declare
    /// theirs, which is what leaves the interior face's edges over instead of
    /// breaking their own boundaries.
    fn box_with_an_interior_face(interior_inside_the_box: bool) -> Vec<FixtureFace> {
        let mut faces = fixture_box_faces();
        let height = FIXTURE_BOX[2] / 2.0;
        faces.push(FixtureFace {
            face: 7,
            plane: 57,
            origin: [0.0, 0.0, height],
            x_axis: [1.0, 0.0, 0.0],
            y_axis: [0.0, 1.0, 0.0],
            corners: vec![
                [0.0, 0.0, height],
                [FIXTURE_BOX[0], 0.0, height],
                [FIXTURE_BOX[0], FIXTURE_BOX[1], height],
                [0.0, FIXTURE_BOX[1], height],
            ],
            // Each of its edges cuts across one of the four sides, whose own
            // loops never use them: y = 0, then x = 1, y = 1 and x = 0.
            joins: vec![3, 2, 4, 1],
            declares_a_loop: false,
            inside_the_box: interior_inside_the_box,
        });
        faces
    }

    #[test]
    fn a_face_inside_the_shell_is_dropped_where_the_records_box_does_not_bound_it() {
        let objects = fixture_record(&box_with_an_interior_face(false));
        let classes = fixture_classes();
        let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        assert!(
            assembled.excluded_faces.is_empty(),
            "{:?}",
            assembled.excluded_faces
        );
        assert_eq!(assembled.faces.len(), 7);
        assert_eq!(assembled.bodies.len(), 1);

        let box_of_the_record = bounds([0.0, 0.0, 0.0], [1.0, 1.0, 2.0]);
        let (placed_body, placed) = place_declared_body(assembled, Some(&box_of_the_record));
        // The interior face changes no extent, so the body reproduces the box
        // with it - and the shell it writes is not a solid: the four edges the
        // interior face draws are drawn by nothing else.
        assert!(placed);
        assert!(placed_body.is_closed());
        assert!(!placed_body.bounds_a_volume());

        let trimmed = place_body_less_its_cut_faces(
            &objects,
            &classes,
            &[FIXTURE_GBREP],
            Some(&box_of_the_record),
            &placed_body,
        )
        .expect("the trim reads the box's own six faces");
        assert_eq!(trimmed.face_ids, [1, 2, 3, 4, 5, 6]);
        assert!(trimmed.bounds_a_volume());
        assert!(body_is_placed_in(&trimmed, &box_of_the_record));
    }

    #[test]
    fn topology_alone_does_not_turn_unmatched_face_loops_into_a_closed_shell() {
        let objects = fixture_record(&box_with_an_interior_face(true));
        let classes = fixture_classes();
        let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        let box_of_the_record = bounds([0.0, 0.0, 0.0], [1.0, 1.0, 2.0]);
        let (body, placed) = place_declared_body(assembled, Some(&box_of_the_record));

        // The source's edge references name two faces on every side, but the
        // interior face draws four edges no other loop draws. This is the
        // shape of the panel population that used to become a closed shell
        // with a volume hundreds of thousands of times too large.
        assert!(placed);
        assert!(body.is_closed());
        assert!(!body.bounds_a_volume());

        let normalized =
            normalize_brep(&body, &IDENTITY_TRANSFORM, true).expect("finite planar body");
        assert!(!normalized.complete);
    }

    #[test]
    fn an_exact_duplicate_interior_sheet_is_not_written_into_the_closed_shell() {
        let objects = fixture_record(&fixture_box_faces());
        let classes = fixture_classes();
        let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        let box_of_the_record = bounds([0.0, 0.0, 0.0], [1.0, 1.0, 2.0]);
        let (mut body, placed) = place_declared_body(assembled, Some(&box_of_the_record));
        let sheet = square_body([0.0, 0.0, 1.0]).faces.remove(0);
        body.faces.extend([sheet.clone(), sheet]);

        // The duplicate sheet pairs its own edges, so the old closure test
        // accepted all eight faces. Removing the exact pair leaves the box's
        // independently closed six-face boundary.
        assert!(placed);
        assert!(body.bounds_a_volume());
        assert_eq!(body.redundant_duplicate_face_indexes(), [6, 7]);

        let normalized =
            normalize_brep(&body, &IDENTITY_TRANSFORM, false).expect("finite planar body");
        assert!(normalized.complete);
        assert_eq!(normalized.faces.len(), 6);
    }

    #[test]
    fn a_face_the_records_box_bounds_is_never_dropped() {
        // The same seven faces, with the interior one marked as the box's own.
        // Nothing is dropped, so nothing is re-read: a body is only ever
        // trimmed of the faces the record itself sets apart.
        let objects = fixture_record(&box_with_an_interior_face(true));
        let classes = fixture_classes();
        let assembled = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        let box_of_the_record = bounds([0.0, 0.0, 0.0], [1.0, 1.0, 2.0]);
        let (placed_body, placed) = place_declared_body(assembled, Some(&box_of_the_record));
        assert!(placed);
        assert!(!placed_body.bounds_a_volume());
        assert!(
            place_body_less_its_cut_faces(
                &objects,
                &classes,
                &[FIXTURE_GBREP],
                Some(&box_of_the_record),
                &placed_body,
            )
            .is_none()
        );
    }

    #[test]
    fn a_trim_that_places_a_different_body_is_refused() {
        let body = |face_ids: &[u32]| rvt_model::SymbolBrep {
            face_ids: face_ids.to_vec(),
            ..rvt_model::SymbolBrep::default()
        };
        // The same body, less one face.
        assert!(is_trim_of(&body(&[1, 2, 3]), &body(&[1, 2, 3, 4])));
        // A body carrying a face the first pass did not: not this body read
        // again, however well it places. See `place_body_less_its_cut_faces`.
        assert!(!is_trim_of(&body(&[1, 2, 9]), &body(&[1, 2, 3, 4])));
        // Nothing dropped, and nothing left.
        assert!(!is_trim_of(&body(&[1, 2, 3, 4]), &body(&[1, 2, 3, 4])));
        assert!(!is_trim_of(&body(&[]), &body(&[1, 2, 3, 4])));
    }

    #[test]
    fn a_placed_body_outranks_the_one_its_id_already_holds() {
        let mut kept = ExportedElement::default();
        // Nothing held yet: anything is an improvement.
        assert!(keep_body(false, &kept));
        kept.brep = Some(square_body([0.0, 0.0, 0.0]));

        // An unplaced body still replaces an unplaced one, which is what every
        // id did before the two could be told apart.
        assert!(keep_body(false, &kept));
        // A placed one takes it over.
        assert!(keep_body(true, &kept));

        kept.brep_is_placed = true;
        // A held placed body wins over an unplaced newcomer.
        assert!(!keep_body(false, &kept));
        // Between two placed bodies the later record wins, however large the
        // one already held: an earlier partition's copy is a state the model
        // has moved on from.
        assert!(keep_body(true, &kept));
    }

    #[test]
    fn a_member_is_left_to_its_own_product_only_when_that_product_draws_it() {
        let objects = fixture_record(&fixture_box_faces());
        let classes = fixture_classes();
        let body = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        let member = ExportedElement {
            brep: Some(body),
            brep_is_placed: true,
            ..ExportedElement::default()
        };
        let bodiless = ExportedElement::default();
        let elements = BTreeMap::from([(7, member), (8, bodiless)]);

        // A product that draws the same solid itself.
        assert!(drawn_as_its_own_product(7, &elements, &|_| true));
        // Not selected as a product, so the host is the only one to draw it.
        assert!(!drawn_as_its_own_product(7, &elements, &|_| false));
        // A product with no body of its own would draw nothing in its place.
        assert!(!drawn_as_its_own_product(8, &elements, &|_| true));
        // An id this file did not recover is nobody's product.
        assert!(!drawn_as_its_own_product(9, &elements, &|_| true));
    }

    #[test]
    fn a_placed_body_is_emitted_without_a_symbol_or_a_transform() {
        let mut wall = ExportedElement::default();
        let objects = fixture_record(&fixture_box_faces());
        let classes = fixture_classes();
        let body = rvt_model::assemble_symbol_brep(&objects, &classes, &[FIXTURE_GBREP]);
        assert!(body.bounds_a_volume());
        wall.brep = Some(body);
        wall.brep_is_placed = true;

        let geometry =
            normalize_geometry(&wall, BimElementType::Wall, &BTreeMap::new(), &|_| false);
        let Some(BimGeometry::Brep(brep)) = geometry else {
            panic!("a placed body should reach the export: {geometry:?}");
        };
        assert!(brep.complete);
        assert_eq!(brep.faces.len(), 6);
        // Carried straight through in the source's own project coordinates,
        // converted to metres and to nothing else.
        let mut max = [f64::NEG_INFINITY; 3];
        for point in brep
            .faces
            .iter()
            .flat_map(|face| &face.loops)
            .flatten()
            .flat_map(|edge| [&edge.start, &edge.end])
        {
            assert_eq!(point.unit.id, "autodesk.unit.unit:meters-1.0.0");
            for (axis, coordinate) in point.coordinates.iter().enumerate() {
                max[axis] = max[axis].max(*coordinate);
            }
        }
        for (actual, feet) in max.into_iter().zip([1.0, 1.0, 2.0]) {
            assert!((actual - feet * 0.304_8).abs() < 1.0e-12, "{actual}");
        }

        // A record declaring its own category is a type definition, and its
        // box is in its own local frame however exactly the body reproduces
        // it. The same body is refused there.
        wall.category = Some(-2_000_011);
        wall.category_source = Some("declared");
        assert!(
            normalize_geometry(&wall, BimElementType::Wall, &BTreeMap::new(), &|_| false).is_none()
        );
        wall.category = None;
        wall.category_source = None;

        // Without the placed flag there is no symbol to fall back to, so the
        // same body is not emitted: the flag is the whole of the gate.
        wall.brep_is_placed = false;
        assert!(
            normalize_geometry(&wall, BimElementType::Wall, &BTreeMap::new(), &|_| false).is_none()
        );
    }

    /// `GFace.m_renderStyleId` names a `MaterialElem` the same way a compound
    /// layer's own material does; `resolve_face_materials` reads that
    /// element's name and shading colour once the element table is in
    /// scope, the same lookup `normalize_material_layers` already trusts.
    #[test]
    fn resolve_face_materials_names_a_face_from_its_render_style_id() {
        let face = BimBrepFace {
            surface: BimBrepSurface::Plane {
                origin: BimPoint3 {
                    coordinates: [0.0, 0.0, 0.0],
                    unit: metres_unit(),
                },
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: Vec::new(),
            material: Some(Box::new(BimMaterial {
                id: Some(BimExternalId {
                    system: "autodesk.revit.elementId".to_owned(),
                    value: "42".to_owned(),
                }),
                name: None,
                color: None,
            })),
        };
        let elements = BTreeMap::from([(
            42,
            ExportedElement {
                name: Some(("Glass".to_owned(), "declared")),
                material_color: Some(rvt_model::MaterialColorFields {
                    red: 0xba,
                    green: 0xbf,
                    blue: 0xc5,
                }),
                ..ExportedElement::default()
            },
        )]);

        let geometry = resolve_face_materials(
            BimGeometry::Brep(BimBrep {
                faces: vec![face],
                complete: true,
            }),
            &elements,
        );

        let BimGeometry::Brep(brep) = geometry else {
            panic!("still a Brep");
        };
        let material = brep.faces[0].material.as_ref().expect("kept its identity");
        assert_eq!(
            material.id,
            Some(BimExternalId {
                system: "autodesk.revit.elementId".to_owned(),
                value: "42".to_owned(),
            })
        );
        assert_eq!(material.name.as_deref(), Some("Glass"));
        assert_eq!(
            material.color,
            Some(BimColor {
                red: 0xba,
                green: 0xbf,
                blue: 0xc5
            })
        );
    }

    /// A `render_style_id` naming nothing this file recovered - a linked
    /// model's material, say - keeps its identity and states no name rather
    /// than one made up.
    #[test]
    fn an_unresolved_face_material_keeps_its_identity_and_no_name() {
        let face = BimBrepFace {
            surface: BimBrepSurface::Plane {
                origin: BimPoint3 {
                    coordinates: [0.0, 0.0, 0.0],
                    unit: metres_unit(),
                },
                x_axis: [1.0, 0.0, 0.0],
                y_axis: [0.0, 1.0, 0.0],
            },
            loops: Vec::new(),
            material: Some(Box::new(BimMaterial {
                id: Some(BimExternalId {
                    system: "autodesk.revit.elementId".to_owned(),
                    value: "999".to_owned(),
                }),
                name: None,
                color: None,
            })),
        };

        let geometry = resolve_face_materials(
            BimGeometry::Brep(BimBrep {
                faces: vec![face],
                complete: true,
            }),
            &BTreeMap::new(),
        );

        let BimGeometry::Brep(brep) = geometry else {
            panic!("still a Brep");
        };
        let material = brep.faces[0].material.as_ref().expect("kept its identity");
        assert_eq!(material.name, None);
    }

    #[test]
    fn a_room_is_named_by_its_number_and_called_by_its_name() {
        let Some(catalog) = Catalog::for_release(2023) else {
            // The catalogue is generated per release; without it there is no
            // built-in to read and nothing this test can assert.
            return;
        };
        let mut room = ExportedElement {
            parameters: vec![
                rvt_model::Parameter {
                    id: -1_006_900,
                    value: ParameterValue::Text("Комната".to_owned()),
                },
                rvt_model::Parameter {
                    id: -1_006_901,
                    value: ParameterValue::Text("204".to_owned()),
                },
            ],
            ..ExportedElement::default()
        };
        let (number, name) = room_identity(&room, Some(catalog));
        assert_eq!(number.as_deref(), Some("204"));
        assert_eq!(name.as_deref(), Some("Комната"));

        // A room with no number keeps its name, which is the case for one of
        // AR S1's 554.
        room.parameters.pop();
        let (number, name) = room_identity(&room, Some(catalog));
        assert_eq!(number, None);
        assert_eq!(name.as_deref(), Some("Комната"));
    }
}
